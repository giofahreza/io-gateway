//! Durable, expiry-driven Codex reset-credit automation.
//!
//! Earned reset credits are account-wide and cannot safely be represented by
//! dashboard cache state or an in-memory timer.  This module stores policy,
//! claims, idempotency keys, and safe outcome metadata in the hardened policy
//! SQLite database.  It intentionally stores no provider credential, request
//! body, raw upstream response, or free-form error string.

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use futures_util::future::BoxFuture;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, path::Path, time::Duration};
use tracing::warn;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 2;
const SCAN_LEASE: Duration = Duration::from_secs(2 * 60);
// A full safe redemption opens three App Server sessions. Each has bounded
// initialize, account/read, and operation requests, so the worst case is
// roughly 270 seconds before shutdown margin. Keep a materially larger lease
// so another gateway process cannot re-enter a still-running idempotent
// action merely because a child is slow.
const ACTION_LEASE: Duration = Duration::from_secs(10 * 60);
const MAX_POLICY_ROWS_PER_TICK: usize = 128;
const MAX_ACTION_ROWS_PER_TICK: usize = 256;
// Reconciliation intentionally alternates one due action with one due
// policy scan.  A single App Server operation can take minutes, so draining a
// large stale action snapshot first can make an expiring credit miss its final
// safe attempt.  Keep the per-kind bounds for overload protection while
// selecting each action afresh at the instant it is about to run.
const MAX_RECONCILIATION_TURNS_PER_TICK: usize =
    MAX_POLICY_ROWS_PER_TICK + MAX_ACTION_ROWS_PER_TICK;
const MIN_SCAN_MINUTES: u64 = 1;
const MAX_SCAN_MINUTES: u64 = 24 * 60;
pub(crate) const MIN_AUTOMATIC_EXPIRY_LEAD_MINUTES: u64 = 30;
const MIN_EXPIRY_WINDOW_MINUTES: u64 = MIN_AUTOMATIC_EXPIRY_LEAD_MINUTES;
const MAX_EXPIRY_WINDOW_MINUTES: u64 = 7 * 24 * 60;
const MAX_NATURAL_RESET_MINUTES: u64 = 7 * 24 * 60;

pub(crate) const DEFAULT_SCAN_INTERVAL_MINUTES: u64 = 30;
pub(crate) const DEFAULT_EXPIRY_WINDOW_MINUTES: u64 = 60;
pub(crate) const DEFAULT_FINAL_ATTEMPT_MINUTES: u64 = 5;
pub(crate) const DEFAULT_MIN_NATURAL_RESET_REMAINING_MINUTES: u64 = 10;

const TABLES: &[&str] = &[
    "codex_reset_credit_metadata",
    "codex_reset_credit_policies",
    "codex_reset_credit_actions",
    "codex_reset_credit_attempts",
];

/// Per-account automation settings.  The absence of a policy means disabled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CodexResetCreditPolicy {
    pub account_key: String,
    pub enabled: bool,
    pub scan_interval_minutes: u64,
    pub expiry_window_minutes: u64,
    pub final_attempt_minutes: u64,
    pub min_natural_reset_remaining_minutes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_scan_at: Option<String>,
    pub updated_at: String,
}

/// Strict administrative write shape.  The route additionally verifies that
/// `account_key` refers to a current stable Codex account before saving it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexResetCreditPolicyUpdate {
    pub account_key: String,
    pub enabled: bool,
    #[serde(default = "default_scan_interval_minutes")]
    pub scan_interval_minutes: u64,
    #[serde(default = "default_expiry_window_minutes")]
    pub expiry_window_minutes: u64,
    #[serde(default = "default_final_attempt_minutes")]
    pub final_attempt_minutes: u64,
    #[serde(default = "default_min_natural_reset_remaining_minutes")]
    pub min_natural_reset_remaining_minutes: u64,
}

fn default_scan_interval_minutes() -> u64 {
    DEFAULT_SCAN_INTERVAL_MINUTES
}

fn default_expiry_window_minutes() -> u64 {
    DEFAULT_EXPIRY_WINDOW_MINUTES
}

fn default_final_attempt_minutes() -> u64 {
    DEFAULT_FINAL_ATTEMPT_MINUTES
}

fn default_min_natural_reset_remaining_minutes() -> u64 {
    DEFAULT_MIN_NATURAL_RESET_REMAINING_MINUTES
}

impl CodexResetCreditPolicyUpdate {
    fn normalized(&self) -> Result<Self, String> {
        let account_key = normalized_automation_account_key(&self.account_key)?;
        if !(MIN_SCAN_MINUTES..=MAX_SCAN_MINUTES).contains(&self.scan_interval_minutes) {
            return Err(format!(
                "Codex reset-credit scan interval must be between {} and {} minutes",
                MIN_SCAN_MINUTES, MAX_SCAN_MINUTES
            ));
        }
        if !(MIN_EXPIRY_WINDOW_MINUTES..=MAX_EXPIRY_WINDOW_MINUTES)
            .contains(&self.expiry_window_minutes)
        {
            return Err(format!(
                "Codex reset-credit expiry window must be between {} and {} minutes",
                MIN_EXPIRY_WINDOW_MINUTES, MAX_EXPIRY_WINDOW_MINUTES
            ));
        }
        if self.final_attempt_minutes == 0
            || self.final_attempt_minutes >= self.expiry_window_minutes
        {
            return Err(
                "Codex reset-credit final-attempt minutes must be at least 1 and smaller than the expiry window"
                    .to_string(),
            );
        }
        if self.min_natural_reset_remaining_minutes > MAX_NATURAL_RESET_MINUTES {
            return Err(format!(
                "Codex natural-reset threshold must not exceed {} minutes",
                MAX_NATURAL_RESET_MINUTES
            ));
        }
        Ok(Self {
            account_key,
            enabled: self.enabled,
            scan_interval_minutes: self.scan_interval_minutes,
            expiry_window_minutes: self.expiry_window_minutes,
            final_attempt_minutes: self.final_attempt_minutes,
            min_natural_reset_remaining_minutes: self.min_natural_reset_remaining_minutes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResetCreditActionState {
    Pending,
    Deferred,
    Submitted,
    Verified,
    NoCredit,
    ManualReview,
    Expired,
}

impl ResetCreditActionState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Deferred => "deferred",
            Self::Submitted => "submitted",
            Self::Verified => "verified",
            Self::NoCredit => "no_credit",
            Self::ManualReview => "manual_review",
            Self::Expired => "expired",
        }
    }

    fn from_db(value: &str) -> Result<Self, String> {
        match value {
            "pending" => Ok(Self::Pending),
            "deferred" => Ok(Self::Deferred),
            "submitted" => Ok(Self::Submitted),
            "verified" => Ok(Self::Verified),
            "no_credit" => Ok(Self::NoCredit),
            "manual_review" => Ok(Self::ManualReview),
            "expired" => Ok(Self::Expired),
            _ => Err("invalid persisted Codex reset-credit action state".to_string()),
        }
    }

    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Verified | Self::NoCredit | Self::ManualReview | Self::Expired
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResetCreditActionTrigger {
    Automatic,
    Manual,
}

impl ResetCreditActionTrigger {
    fn as_db(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResetCreditAttemptOutcome {
    Claimed,
    Submitted,
    DeferredNoEligibleLimit,
    DeferredNaturalResetSoon,
    DeferredNaturalResetUnknown,
    NoCredit,
    VerificationFailed,
    Verified,
    Expired,
    RetryScheduled,
    DeferredPolicyDisabled,
}

impl ResetCreditAttemptOutcome {
    fn as_db(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Submitted => "submitted",
            Self::DeferredNoEligibleLimit => "deferred_no_eligible_limit",
            Self::DeferredNaturalResetSoon => "deferred_natural_reset_soon",
            Self::DeferredNaturalResetUnknown => "deferred_natural_reset_unknown",
            Self::NoCredit => "no_credit",
            Self::VerificationFailed => "verification_failed",
            Self::Verified => "verified",
            Self::Expired => "expired",
            Self::RetryScheduled => "retry_scheduled",
            Self::DeferredPolicyDisabled => "deferred_policy_disabled",
        }
    }
}

/// Safe, durable metadata for one selected credit.  It deliberately excludes
/// prompt data, provider credentials, raw errors, and raw response payloads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CodexResetCreditAction {
    pub id: String,
    pub account_key: String,
    pub credit_id: String,
    pub reset_type: String,
    /// `None` means the upstream exposed a redeemable credit but no expiry.
    /// Automatic redemption never creates such an action; this exists so the
    /// manual and automatic paths can still share one durable coordinator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit_expires_at: Option<String>,
    /// Internal retry material.  It is persisted locally and sent only to the
    /// upstream consume call; never expose it in an admin/browser response.
    #[serde(skip_serializing)]
    pub idempotency_key: String,
    pub state: ResetCreditActionState,
    pub trigger: String,
    pub next_attempt_at: String,
    pub attempt_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResetCreditActionCandidate {
    pub account_key: String,
    pub credit_id: String,
    pub reset_type: String,
    pub credit_expires_at: Option<DateTime<Utc>>,
    pub trigger: ResetCreditActionTrigger,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResetCreditActionClaim {
    Acquired(CodexResetCreditAction),
    Busy,
    NotDue,
    Terminal(CodexResetCreditAction),
}

/// Initializes the reset-credit schema under the existing policy database's
/// identity transaction.  The initialized bit means a missing table after an
/// established deployment is corruption, never a reason to recreate an empty
/// action ledger and risk a duplicate spend.
pub(crate) fn initialize(connection: &Connection) -> Result<(), String> {
    ensure_metadata_column(connection)?;
    let initialized: bool = connection
        .query_row(
            "SELECT codex_reset_credits_initialized FROM managed_registry_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(sql)?;
    let present = table_count(connection)?;
    if (initialized && present != TABLES.len()) || (present != 0 && present != TABLES.len()) {
        return Err(
            "established Codex reset-credit table is missing; refusing to recreate redemption state"
                .to_string(),
        );
    }
    if present == TABLES.len() {
        let version: i64 = connection
            .query_row(
                "SELECT version FROM codex_reset_credit_metadata WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        match version {
            SCHEMA_VERSION => {}
            1 => migrate_legacy_wham_actions(connection)?,
            _ => return Err("unsupported Codex reset-credit schema version".to_string()),
        }
        if !initialized {
            connection
                .execute(
                    "UPDATE managed_registry_metadata SET codex_reset_credits_initialized=1 WHERE singleton=1",
                    [],
                )
                .map_err(sql)?;
        }
        return Ok(());
    }

    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS codex_reset_credit_metadata (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                version INTEGER NOT NULL
             );
             INSERT INTO codex_reset_credit_metadata(singleton,version) VALUES(1,2);
             CREATE TABLE IF NOT EXISTS codex_reset_credit_policies (
                account_key TEXT PRIMARY KEY,
                enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
                scan_interval_seconds INTEGER NOT NULL CHECK(scan_interval_seconds>0),
                expiry_window_seconds INTEGER NOT NULL CHECK(expiry_window_seconds>0),
                final_attempt_seconds INTEGER NOT NULL CHECK(final_attempt_seconds>=0),
                min_natural_reset_seconds INTEGER NOT NULL CHECK(min_natural_reset_seconds>=0),
                next_scan_at TEXT,
                scan_lease_owner TEXT,
                scan_lease_expires_at TEXT,
                updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_codex_reset_credit_policies_due
                ON codex_reset_credit_policies(enabled,next_scan_at);
             CREATE TABLE IF NOT EXISTS codex_reset_credit_actions (
                id TEXT PRIMARY KEY,
                account_key TEXT NOT NULL,
                credit_id TEXT NOT NULL,
                reset_type TEXT NOT NULL,
                credit_expires_at TEXT NOT NULL,
                trigger TEXT NOT NULL CHECK(trigger IN ('automatic','manual')),
                idempotency_key TEXT NOT NULL UNIQUE,
                transport TEXT NOT NULL CHECK(transport IN ('app_server','wham_legacy')),
                state TEXT NOT NULL CHECK(state IN ('pending','deferred','submitted','verified','no_credit','manual_review','expired')),
                next_attempt_at TEXT NOT NULL,
                lease_owner TEXT,
                lease_expires_at TEXT,
                attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count>=0),
                last_outcome TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                submitted_at TEXT,
                verified_at TEXT,
                UNIQUE(account_key,credit_id)
             );
             CREATE INDEX IF NOT EXISTS idx_codex_reset_credit_actions_due
                ON codex_reset_credit_actions(state,next_attempt_at,credit_expires_at);
             CREATE INDEX IF NOT EXISTS idx_codex_reset_credit_actions_account
                ON codex_reset_credit_actions(account_key,state);
             CREATE TABLE IF NOT EXISTS codex_reset_credit_attempts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                action_id TEXT NOT NULL REFERENCES codex_reset_credit_actions(id),
                occurred_at TEXT NOT NULL,
                outcome TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_codex_reset_credit_attempts_action
                ON codex_reset_credit_attempts(action_id,id DESC);",
        )
        .map_err(sql)?;
    connection
        .execute(
            "UPDATE managed_registry_metadata SET codex_reset_credits_initialized=1 WHERE singleton=1",
            [],
        )
        .map_err(sql)?;
    Ok(())
}

/// Version 1 used an undocumented direct Wham adapter. Its `submitted` rows
/// may already have reached that different endpoint, so their idempotency key
/// cannot safely be replayed through App Server. Preserve those rows as a
/// visible manual-review task; pending/deferred rows never recorded a POST and
/// can be revalidated afresh through the new transport.
fn migrate_legacy_wham_actions(connection: &Connection) -> Result<(), String> {
    if !table_has_column(connection, "codex_reset_credit_actions", "transport")? {
        connection
            .execute_batch(
                "ALTER TABLE codex_reset_credit_actions
                 ADD COLUMN transport TEXT NOT NULL DEFAULT 'wham_legacy'
                 CHECK(transport IN ('app_server','wham_legacy'));",
            )
            .map_err(sql)?;
    }
    connection
        .execute(
            "UPDATE codex_reset_credit_actions
             SET state='manual_review', lease_owner=NULL, lease_expires_at=NULL,
                 last_outcome='legacy_transport_manual_review'
             WHERE transport='wham_legacy' AND state='submitted'",
            [],
        )
        .map_err(sql)?;
    connection
        .execute(
            "UPDATE codex_reset_credit_actions
             SET transport='app_server'
             WHERE transport='wham_legacy' AND state IN ('pending','deferred')",
            [],
        )
        .map_err(sql)?;
    connection
        .execute(
            "UPDATE codex_reset_credit_metadata SET version=?1 WHERE singleton=1",
            [SCHEMA_VERSION],
        )
        .map_err(sql)?;
    Ok(())
}

fn ensure_metadata_column(connection: &Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare("PRAGMA table_info(managed_registry_metadata)")
        .map_err(sql)?;
    let mut rows = statement.query([]).map_err(sql)?;
    let mut exists = false;
    while let Some(row) = rows.next().map_err(sql)? {
        let name: String = row.get(1).map_err(sql)?;
        if name == "codex_reset_credits_initialized" {
            exists = true;
            break;
        }
    }
    if !exists {
        connection
            .execute_batch(
                "ALTER TABLE managed_registry_metadata
                 ADD COLUMN codex_reset_credits_initialized INTEGER NOT NULL DEFAULT 0
                 CHECK(codex_reset_credits_initialized IN (0,1));",
            )
            .map_err(sql)?;
    }
    Ok(())
}

fn table_count(connection: &Connection) -> Result<usize, String> {
    TABLES.iter().try_fold(0usize, |count, table| {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .map_err(sql)?;
        Ok(count + usize::from(exists))
    })
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(sql)?;
    let mut rows = statement.query([]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        let name: String = row.get(1).map_err(sql)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Creates or replaces an account's automation policy.  Enabling or editing a
/// policy schedules an immediate fresh scan, which provides startup/edit
/// catch-up without relying on an in-memory timer.
pub(crate) fn upsert_policy(
    cfg: &crate::Config,
    update: &CodexResetCreditPolicyUpdate,
) -> Result<CodexResetCreditPolicy, String> {
    upsert_policy_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        update,
        Utc::now(),
    )
}

pub(crate) fn upsert_policy_at_path(
    path: &Path,
    update: &CodexResetCreditPolicyUpdate,
    now: DateTime<Utc>,
) -> Result<CodexResetCreditPolicy, String> {
    let update = update.normalized()?;
    let now_text = stamp(now);
    let next_scan = update.enabled.then_some(now_text.clone());
    let mut connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    transaction
        .execute(
            "INSERT INTO codex_reset_credit_policies(
                account_key,enabled,scan_interval_seconds,expiry_window_seconds,
                final_attempt_seconds,min_natural_reset_seconds,next_scan_at,
                scan_lease_owner,scan_lease_expires_at,updated_at
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,NULL,NULL,?8)
             ON CONFLICT(account_key) DO UPDATE SET
                enabled=excluded.enabled,
                scan_interval_seconds=excluded.scan_interval_seconds,
                expiry_window_seconds=excluded.expiry_window_seconds,
                final_attempt_seconds=excluded.final_attempt_seconds,
                min_natural_reset_seconds=excluded.min_natural_reset_seconds,
                next_scan_at=excluded.next_scan_at,
                scan_lease_owner=NULL,
                scan_lease_expires_at=NULL,
                updated_at=excluded.updated_at",
            params![
                &update.account_key,
                bool_to_sql(update.enabled),
                minutes_to_seconds(update.scan_interval_minutes)?,
                minutes_to_seconds(update.expiry_window_minutes)?,
                minutes_to_seconds(update.final_attempt_minutes)?,
                minutes_to_seconds(update.min_natural_reset_remaining_minutes)?,
                &next_scan,
                &now_text,
            ],
        )
        .map_err(sql)?;
    let policy = select_policy(&transaction, &update.account_key)?
        .ok_or_else(|| "Codex reset-credit policy disappeared during update".to_string())?;
    transaction.commit().map_err(sql)?;
    Ok(policy)
}

pub(crate) fn list_policies(cfg: &crate::Config) -> Result<Vec<CodexResetCreditPolicy>, String> {
    list_policies_at_path(&crate::api_key_policy_store::policy_db_path(cfg))
}

pub(crate) fn list_policies_at_path(path: &Path) -> Result<Vec<CodexResetCreditPolicy>, String> {
    let connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let mut statement = connection
        .prepare(
            "SELECT account_key,enabled,scan_interval_seconds,expiry_window_seconds,
                    final_attempt_seconds,min_natural_reset_seconds,next_scan_at,updated_at
             FROM codex_reset_credit_policies ORDER BY account_key",
        )
        .map_err(sql)?;
    let rows = statement.query_map([], policy_from_row).map_err(sql)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql)
}

/// Claims every policy whose durable scan deadline has passed.  The lease is
/// cross-process and short; network I/O always happens after this transaction
/// commits.
pub(crate) fn claim_due_policy_scans(
    cfg: &crate::Config,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<CodexResetCreditPolicy>, String> {
    claim_due_policy_scans_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        worker_id,
        now,
    )
}

pub(crate) fn claim_due_policy_scans_at_path(
    path: &Path,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<CodexResetCreditPolicy>, String> {
    let worker_id = normalized_identifier(worker_id, "reset-credit worker ID")?;
    let now_text = stamp(now);
    let lease_until = stamp(add_std(now, SCAN_LEASE)?);
    let mut connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let mut statement = transaction
        .prepare(
            "SELECT account_key FROM codex_reset_credit_policies
             WHERE enabled=1
               AND (next_scan_at IS NULL OR next_scan_at<=?1)
               AND (scan_lease_expires_at IS NULL OR scan_lease_expires_at<=?1)
             ORDER BY next_scan_at,account_key LIMIT 1",
        )
        .map_err(sql)?;
    let accounts = statement
        .query_map(params![&now_text], |row| row.get::<_, String>(0))
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    drop(statement);

    let mut claimed = Vec::new();
    for account_key in accounts {
        let changed = transaction
            .execute(
                "UPDATE codex_reset_credit_policies
                 SET scan_lease_owner=?2,scan_lease_expires_at=?3
                 WHERE account_key=?1 AND enabled=1
                   AND (next_scan_at IS NULL OR next_scan_at<=?4)
                   AND (scan_lease_expires_at IS NULL OR scan_lease_expires_at<=?4)",
                params![&account_key, &worker_id, &lease_until, &now_text],
            )
            .map_err(sql)?;
        if changed == 1 {
            if let Some(policy) = select_policy(&transaction, &account_key)? {
                claimed.push(policy);
            }
        }
    }
    transaction.commit().map_err(sql)?;
    Ok(claimed)
}

pub(crate) fn finish_policy_scan(
    cfg: &crate::Config,
    account_key: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    finish_policy_scan_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        account_key,
        worker_id,
        now,
    )
}

pub(crate) fn finish_policy_scan_at_path(
    path: &Path,
    account_key: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let account_key = normalized_identifier(account_key, "Codex account key")?;
    let worker_id = normalized_identifier(worker_id, "reset-credit worker ID")?;
    let mut connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let policy = select_policy(&transaction, &account_key)?.ok_or_else(|| {
        "Codex reset-credit policy disappeared before scan completion".to_string()
    })?;
    let next_scan = policy
        .enabled
        .then(|| add_minutes(now, policy.scan_interval_minutes))
        .transpose()?
        .map(stamp);
    let changed = transaction
        .execute(
            "UPDATE codex_reset_credit_policies
             SET next_scan_at=?3,scan_lease_owner=NULL,scan_lease_expires_at=NULL
             WHERE account_key=?1 AND scan_lease_owner=?2",
            params![&account_key, &worker_id, &next_scan],
        )
        .map_err(sql)?;
    if changed != 1 {
        // A policy edit or a recovered peer may revoke this old claim while
        // the local App Server call is in flight. That cancellation is
        // expected; the current policy's durable immediate deadline will be
        // scanned by its current owner instead of letting stale work win.
        transaction.commit().map_err(sql)?;
        return Ok(());
    }
    transaction.commit().map_err(sql)
}

pub(crate) fn register_action(
    cfg: &crate::Config,
    candidate: &ResetCreditActionCandidate,
    now: DateTime<Utc>,
) -> Result<CodexResetCreditAction, String> {
    register_action_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        candidate,
        now,
    )
}

pub(crate) fn register_action_at_path(
    path: &Path,
    candidate: &ResetCreditActionCandidate,
    now: DateTime<Utc>,
) -> Result<CodexResetCreditAction, String> {
    let mut connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let action = register_action_in_transaction(&transaction, candidate, now)?;
    transaction.commit().map_err(sql)?;
    Ok(action)
}

/// Registers an automatic action only while the exact policy scan that chose
/// it still owns a live lease and still has the same operator configuration.
/// This closes the gap where a disable/edit could otherwise race a slow
/// App-Server read and leave a stale automatic action behind.
fn register_automatic_action_for_claimed_policy_at_path(
    path: &Path,
    policy: &CodexResetCreditPolicy,
    worker_id: &str,
    candidate: &ResetCreditActionCandidate,
    now: DateTime<Utc>,
) -> Result<Option<CodexResetCreditAction>, String> {
    if candidate.trigger != ResetCreditActionTrigger::Automatic
        || normalized_identifier(&candidate.account_key, "Codex account key")? != policy.account_key
    {
        return Err(
            "automatic reset-credit candidate does not match its claimed policy".to_string(),
        );
    }
    let worker_id = normalized_identifier(worker_id, "reset-credit worker ID")?;
    let mut connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let current: bool = transaction
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM codex_reset_credit_policies
                WHERE account_key=?1 AND enabled=1
                  AND scan_lease_owner=?2 AND scan_lease_expires_at>?3
                  AND updated_at=?4
             )",
            params![
                &policy.account_key,
                &worker_id,
                stamp(now),
                &policy.updated_at
            ],
            |row| row.get(0),
        )
        .map_err(sql)?;
    if !current {
        transaction.commit().map_err(sql)?;
        return Ok(None);
    }
    let action = register_action_in_transaction(&transaction, candidate, now)?;
    transaction.commit().map_err(sql)?;
    Ok(Some(action))
}

fn register_action_in_transaction(
    transaction: &Transaction<'_>,
    candidate: &ResetCreditActionCandidate,
    now: DateTime<Utc>,
) -> Result<CodexResetCreditAction, String> {
    let account_key = normalized_identifier(&candidate.account_key, "Codex account key")?;
    let credit_id = opaque_identifier(&candidate.credit_id, "reset-credit ID")?;
    let reset_type = normalized_identifier(&candidate.reset_type, "reset-credit type")?;
    let now_text = stamp(now);
    // The first version of this table deliberately uses an empty string for a
    // missing expiry rather than a made-up timestamp.  Existing schema rows
    // are `NOT NULL`, and empty is unambiguously decoded back to `None`.
    let expires = candidate.credit_expires_at.map(stamp).unwrap_or_default();
    let id = Uuid::new_v4().to_string();
    let idempotency_key = Uuid::new_v4().to_string();
    // Preserve the one action/idempotency key for an exact credit. Before a
    // POST, a manual deferred action may safely be adopted by an enabled
    // automatic policy so it gains the policy's natural-reset safeguards.
    // Once submitted (or terminal), never rewrite its semantics or expiry.
    if let Some(existing) = select_action_by_account_credit(&transaction, &account_key, &credit_id)?
    {
        if candidate.trigger == ResetCreditActionTrigger::Manual
            && existing.trigger == ResetCreditActionTrigger::Manual.as_db()
            && existing.state == ResetCreditActionState::Deferred
            && existing.credit_expires_at.is_none()
            && select_blocking_action_for_account_except(
                &transaction,
                &account_key,
                Some(&existing.id),
                now,
            )?
            .is_some()
        {
            return Err(
                "another reset-credit action for this Codex account is still active or requires manual review"
                    .to_string(),
            );
        }
        if matches!(
            existing.state,
            ResetCreditActionState::Pending | ResetCreditActionState::Deferred
        ) {
            transaction
                .execute(
                    "UPDATE codex_reset_credit_actions
                     SET reset_type=?3,credit_expires_at=?4,
                         trigger=CASE WHEN ?5='automatic' THEN 'automatic' ELSE trigger END,
                         next_attempt_at=?6,updated_at=?6
                     WHERE account_key=?1 AND credit_id=?2
                       AND state IN ('pending','deferred')",
                    params![
                        &account_key,
                        &credit_id,
                        &reset_type,
                        &expires,
                        candidate.trigger.as_db(),
                        &now_text,
                    ],
                )
                .map_err(sql)?;
        }
        let action = select_action_by_account_credit(&transaction, &account_key, &credit_id)?
            .ok_or_else(|| {
                "Codex reset-credit action disappeared during registration".to_string()
            })?;
        return Ok(action);
    }
    // A reset credit is account-wide. One durable account gate prevents two
    // different credits from observing the same reached window in separate
    // gateway processes and both being spent. `manual_review` remains a gate
    // too: an ambiguous prior submission must never cause automatic selection
    // of a second credit.
    let blocking = match candidate.trigger {
        // An expiry-less manual request is harmless only until an automatic
        // policy selects a concrete expiring credit.  It must not suppress
        // that later policy scan, but it remains a full account-wide gate for
        // another manual click so two unbounded manual requests cannot race
        // each other into separate spends.
        ResetCreditActionTrigger::Automatic => {
            select_automatic_blocking_action_for_account(&transaction, &account_key, now)?
        }
        ResetCreditActionTrigger::Manual => {
            select_blocking_action_for_account(&transaction, &account_key, now)?
        }
    };
    if let Some(existing) = blocking {
        if candidate.trigger == ResetCreditActionTrigger::Automatic {
            return Ok(existing);
        }
        return Err(
            "another reset-credit action for this Codex account is still active or requires manual review"
                .to_string(),
        );
    }
    transaction
        .execute(
            "INSERT INTO codex_reset_credit_actions(
                id,account_key,credit_id,reset_type,credit_expires_at,trigger,idempotency_key,
                transport,state,next_attempt_at,lease_owner,lease_expires_at,attempt_count,last_outcome,
                created_at,updated_at,submitted_at,verified_at
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,'app_server','pending',?8,NULL,NULL,0,NULL,?8,?8,NULL,NULL)
             ON CONFLICT(account_key,credit_id) DO UPDATE SET
                reset_type=excluded.reset_type,
                credit_expires_at=excluded.credit_expires_at,
                updated_at=excluded.updated_at",
            params![
                &id,
                &account_key,
                &credit_id,
                &reset_type,
                &expires,
                candidate.trigger.as_db(),
                &idempotency_key,
                &now_text,
            ],
        )
        .map_err(sql)?;
    let action = select_action_by_account_credit(&transaction, &account_key, &credit_id)?
        .ok_or_else(|| "Codex reset-credit action disappeared during registration".to_string())?;
    Ok(action)
}

pub(crate) fn list_due_actions(
    cfg: &crate::Config,
    now: DateTime<Utc>,
) -> Result<Vec<CodexResetCreditAction>, String> {
    list_due_actions_at_path(&crate::api_key_policy_store::policy_db_path(cfg), now)
}

pub(crate) fn list_due_actions_at_path(
    path: &Path,
    now: DateTime<Utc>,
) -> Result<Vec<CodexResetCreditAction>, String> {
    list_due_actions_at_path_with_limit(path, now, MAX_ACTION_ROWS_PER_TICK)
}

/// Fetches a bounded, current slice of actions that can acquire a lease. The
/// scheduler calls this with one row so it never executes a stale 256-row
/// snapshot after slow network work. Public/testing callers retain the wider
/// bounded view above.
fn list_due_actions_at_path_with_limit(
    path: &Path,
    now: DateTime<Utc>,
    limit: usize,
) -> Result<Vec<CodexResetCreditAction>, String> {
    let now_text = stamp(now);
    let limit = i64::try_from(limit)
        .map_err(|_| "Codex reset-credit action query limit exceeds SQLite range".to_string())?;
    let connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let mut statement = connection
        .prepare(
            "SELECT id,account_key,credit_id,reset_type,credit_expires_at,idempotency_key,
                    state,trigger,next_attempt_at,attempt_count,last_outcome,created_at,updated_at,
                    submitted_at,verified_at
             FROM codex_reset_credit_actions
             WHERE ((trigger='automatic' AND state IN ('pending','deferred','submitted'))
                 OR (trigger='manual' AND state='submitted'))
               AND next_attempt_at<=?1
               AND (lease_expires_at IS NULL OR lease_expires_at<=?1)
             -- An unknown expiry must never outrank a real deadline.  It can
             -- be safely reconciled later, while an expiring automatic
             -- credit cannot be recovered after its deadline.
             ORDER BY CASE WHEN credit_expires_at='' THEN 1 ELSE 0 END,
                      credit_expires_at,next_attempt_at,created_at,id LIMIT ?2",
        )
        .map_err(sql)?;
    let actions = statement
        .query_map(params![&now_text, limit], action_from_row)
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    Ok(actions)
}

fn next_due_action_at_path(
    path: &Path,
    now: DateTime<Utc>,
) -> Result<Option<CodexResetCreditAction>, String> {
    Ok(list_due_actions_at_path_with_limit(path, now, 1)?
        .into_iter()
        .next())
}

pub(crate) fn list_actions(cfg: &crate::Config) -> Result<Vec<CodexResetCreditAction>, String> {
    let connection = crate::api_key_policy_store::open_connection_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
    )?;
    let mut statement = connection
        .prepare(
            "SELECT id,account_key,credit_id,reset_type,credit_expires_at,idempotency_key,
                    state,trigger,next_attempt_at,attempt_count,last_outcome,created_at,updated_at,
                    submitted_at,verified_at
             FROM codex_reset_credit_actions ORDER BY updated_at DESC,id DESC LIMIT ?1",
        )
        .map_err(sql)?;
    let actions = statement
        .query_map([MAX_ACTION_ROWS_PER_TICK as i64], action_from_row)
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    Ok(actions)
}

fn action_at_path(path: &Path, action_id: &str) -> Result<Option<CodexResetCreditAction>, String> {
    let action_id = normalized_identifier(action_id, "reset-credit action ID")?;
    let connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    select_action(&connection, &action_id)
}

fn policy_at_path(
    path: &Path,
    account_key: &str,
) -> Result<Option<CodexResetCreditPolicy>, String> {
    let account_key = normalized_identifier(account_key, "Codex account key")?;
    let connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    select_policy(&connection, &account_key)
}

pub(crate) fn claim_action(
    cfg: &crate::Config,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
    force: bool,
) -> Result<ResetCreditActionClaim, String> {
    claim_action_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        action_id,
        worker_id,
        now,
        force,
    )
}

pub(crate) fn claim_action_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
    force: bool,
) -> Result<ResetCreditActionClaim, String> {
    let action_id = normalized_identifier(action_id, "reset-credit action ID")?;
    let worker_id = normalized_identifier(worker_id, "reset-credit worker ID")?;
    let now_text = stamp(now);
    let lease_until = stamp(add_std(now, ACTION_LEASE)?);
    let mut connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let action = select_action(&transaction, &action_id)?
        .ok_or_else(|| "Codex reset-credit action was not found".to_string())?;
    if action.state.terminal() {
        transaction.commit().map_err(sql)?;
        return Ok(ResetCreditActionClaim::Terminal(action));
    }
    let expires_at = action
        .credit_expires_at
        .as_deref()
        .map(parse_stamp)
        .transpose()?;
    if expires_at.is_some_and(|expires_at| expires_at <= now) {
        let state = if action.state == ResetCreditActionState::Submitted {
            // A POST may have reached ChatGPT just before this process crashed
            // or timed out.  Calling it merely "expired" would hide that
            // ambiguity and make reconciliation unsafe.
            ResetCreditActionState::ManualReview
        } else {
            ResetCreditActionState::Expired
        };
        let outcome = if state == ResetCreditActionState::ManualReview {
            ResetCreditAttemptOutcome::VerificationFailed
        } else {
            ResetCreditAttemptOutcome::Expired
        };
        finish_claimed_action_tx(&transaction, &action.id, None, state, outcome, now, None)?;
        let action = select_action(&transaction, &action_id)?.expect("updated action exists");
        transaction.commit().map_err(sql)?;
        return Ok(ResetCreditActionClaim::Terminal(action));
    }
    if !force && parse_stamp(&action.next_attempt_at)? > now {
        transaction.commit().map_err(sql)?;
        return Ok(ResetCreditActionClaim::NotDue);
    }
    // Registration and execution can race across browser requests and
    // gateway processes. Re-check the durable account gate in this same
    // immediate transaction before acquiring a lease, so an expiry-less
    // manual action that was allowed to coexist with a pending automatic
    // action cannot wake up later and send a second credit consume.
    let conflicting_action = if action.trigger == ResetCreditActionTrigger::Automatic.as_db() {
        select_automatic_blocking_action_for_account_except(
            &transaction,
            &action.account_key,
            Some(&action.id),
            now,
        )?
    } else {
        select_blocking_action_for_account_except(
            &transaction,
            &action.account_key,
            Some(&action.id),
            now,
        )?
    };
    if conflicting_action.is_some() {
        transaction.commit().map_err(sql)?;
        return Ok(ResetCreditActionClaim::Busy);
    }
    let changed = transaction
        .execute(
            "UPDATE codex_reset_credit_actions
             SET lease_owner=?2,lease_expires_at=?3,attempt_count=attempt_count+1,updated_at=?4
             WHERE id=?1
               AND state IN ('pending','deferred','submitted')
               AND (lease_expires_at IS NULL OR lease_expires_at<=?4)",
            params![&action_id, &worker_id, &lease_until, &now_text],
        )
        .map_err(sql)?;
    if changed != 1 {
        transaction.commit().map_err(sql)?;
        return Ok(ResetCreditActionClaim::Busy);
    }
    append_attempt(
        &transaction,
        &action_id,
        now,
        ResetCreditAttemptOutcome::Claimed,
    )?;
    let action = select_action(&transaction, &action_id)?.expect("claimed action exists");
    transaction.commit().map_err(sql)?;
    Ok(ResetCreditActionClaim::Acquired(action))
}

pub(crate) fn mark_action_submitted(
    cfg: &crate::Config,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let path = crate::api_key_policy_store::policy_db_path(cfg);
    mark_action_submitted_at_path(&path, action_id, worker_id, now)
}

fn mark_action_submitted_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    with_claimed_action_at_path(path, action_id, worker_id, now, |transaction, id| {
        transaction
            .execute(
                "UPDATE codex_reset_credit_actions
                 SET state='submitted',submitted_at=COALESCE(submitted_at,?3),updated_at=?3
                 WHERE id=?1 AND lease_owner=?2",
                params![id, worker_id, stamp(now)],
            )
            .map_err(sql)?;
        append_attempt(transaction, id, now, ResetCreditAttemptOutcome::Submitted)
    })
}

pub(crate) fn defer_action(
    cfg: &crate::Config,
    action_id: &str,
    worker_id: &str,
    outcome: DeferredActionOutcome,
    next_attempt_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let path = crate::api_key_policy_store::policy_db_path(cfg);
    defer_action_at_path(&path, action_id, worker_id, outcome, next_attempt_at, now)
}

fn defer_action_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    outcome: DeferredActionOutcome,
    next_attempt_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), String> {
    with_claimed_action_at_path(path, action_id, worker_id, now, |transaction, id| {
        finish_claimed_action_tx(
            transaction,
            id,
            Some(worker_id),
            ResetCreditActionState::Deferred,
            outcome.as_attempt(),
            now,
            Some(next_attempt_at),
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeferredActionOutcome {
    NoEligibleLimit,
    NaturalResetSoon,
    NaturalResetUnknown,
    PolicyDisabled,
}

impl DeferredActionOutcome {
    fn as_attempt(self) -> ResetCreditAttemptOutcome {
        match self {
            Self::NoEligibleLimit => ResetCreditAttemptOutcome::DeferredNoEligibleLimit,
            Self::NaturalResetSoon => ResetCreditAttemptOutcome::DeferredNaturalResetSoon,
            Self::NaturalResetUnknown => ResetCreditAttemptOutcome::DeferredNaturalResetUnknown,
            Self::PolicyDisabled => ResetCreditAttemptOutcome::DeferredPolicyDisabled,
        }
    }
}

/// Releases a current action lease while retaining `submitted`.  This is the
/// only retry path after a possible POST: the same credit ID and persisted
/// idempotency key will be retried, never a different credit.
fn reschedule_submitted_action_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    next_attempt_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), String> {
    with_claimed_action_at_path(path, action_id, worker_id, now, |transaction, id| {
        finish_claimed_action_tx(
            transaction,
            id,
            Some(worker_id),
            ResetCreditActionState::Submitted,
            ResetCreditAttemptOutcome::RetryScheduled,
            now,
            Some(next_attempt_at),
        )
    })
}

pub(crate) fn mark_action_no_credit(
    cfg: &crate::Config,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let path = crate::api_key_policy_store::policy_db_path(cfg);
    mark_action_no_credit_at_path(&path, action_id, worker_id, now)
}

fn mark_action_no_credit_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    with_claimed_action_at_path(path, action_id, worker_id, now, |transaction, id| {
        finish_claimed_action_tx(
            transaction,
            id,
            Some(worker_id),
            ResetCreditActionState::NoCredit,
            ResetCreditAttemptOutcome::NoCredit,
            now,
            None,
        )
    })
}

pub(crate) fn mark_action_verified(
    cfg: &crate::Config,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let path = crate::api_key_policy_store::policy_db_path(cfg);
    mark_action_verified_at_path(&path, action_id, worker_id, now)
}

fn mark_action_verified_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    with_claimed_action_at_path(path, action_id, worker_id, now, |transaction, id| {
        finish_claimed_action_tx(
            transaction,
            id,
            Some(worker_id),
            ResetCreditActionState::Verified,
            ResetCreditAttemptOutcome::Verified,
            now,
            None,
        )?;
        transaction
            .execute(
                "UPDATE codex_reset_credit_actions SET verified_at=?2 WHERE id=?1",
                params![id, stamp(now)],
            )
            .map_err(sql)?;
        Ok(())
    })
}

pub(crate) fn mark_action_manual_review(
    cfg: &crate::Config,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let path = crate::api_key_policy_store::policy_db_path(cfg);
    mark_action_manual_review_at_path(&path, action_id, worker_id, now)
}

fn mark_action_manual_review_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    with_claimed_action_at_path(path, action_id, worker_id, now, |transaction, id| {
        finish_claimed_action_tx(
            transaction,
            id,
            Some(worker_id),
            ResetCreditActionState::ManualReview,
            ResetCreditAttemptOutcome::VerificationFailed,
            now,
            None,
        )
    })
}

/// One reconciliation pass.  The worker wakes once per minute so it can honour
/// final action deadlines, while every account policy remains on its durable
/// (normally 30-minute) scan schedule.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CodexResetCreditRunSummary {
    pub claimed_policy_scans: usize,
    pub registered_actions: usize,
    pub processed_actions: usize,
    pub upstream_failures: usize,
}

/// Narrow adapter boundary for deterministic coordinator tests.  The durable
/// coordinator never receives a raw credential, request payload, or upstream
/// response body; the live adapter owns that sensitive transport work.
trait ResetCreditUpstream {
    fn fresh_state<'a>(
        &'a self,
        account_key: &'a str,
    ) -> BoxFuture<'a, Result<crate::target::codex::quota::FreshRateLimitResetState, String>>;

    fn consume<'a>(
        &'a self,
        account_key: &'a str,
        credit_id: &'a str,
        idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<crate::target::codex::quota::ConsumeRateLimitResetResult, String>>;

    fn on_verified(&self, account_key: &str);
}

struct LiveResetCreditUpstream<'a> {
    state: &'a crate::AppState,
}

impl ResetCreditUpstream for LiveResetCreditUpstream<'_> {
    fn fresh_state<'a>(
        &'a self,
        account_key: &'a str,
    ) -> BoxFuture<'a, Result<crate::target::codex::quota::FreshRateLimitResetState, String>> {
        Box::pin(async move {
            // Keep the gateway credential identity check separate from the
            // App Server profile. The latter owns managed ChatGPT refresh
            // state; no raw gateway bearer token is passed to the child.
            let _ = resolve_automation_token(self.state, account_key)?;
            crate::target::codex::quota::fresh_rate_limit_reset_state(self.state, account_key).await
        })
    }

    fn consume<'a>(
        &'a self,
        account_key: &'a str,
        credit_id: &'a str,
        idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<crate::target::codex::quota::ConsumeRateLimitResetResult, String>>
    {
        Box::pin(async move {
            let _ = resolve_automation_token(self.state, account_key)?;
            crate::target::codex::quota::consume_rate_limit_reset_credit_with_token(
                self.state,
                account_key,
                credit_id,
                idempotency_key,
            )
            .await
        })
    }

    fn on_verified(&self, account_key: &str) {
        // Resetting upstream limits makes a local cooldown stale.  Clear only
        // the exact account, never every Codex account merely because one
        // credit was redeemed.
        crate::clear_router_account_runtime(self.state, "codex", account_key);

        let matching_indexes = {
            let tokens = self.state.tokens.lock().unwrap();
            tokens
                .iter()
                .enumerate()
                .filter_map(|(index, token)| {
                    (crate::codex_stats_key(token) == account_key).then_some(index)
                })
                .collect::<Vec<_>>()
        };
        let mut cache = self.state.quota_cache.lock().unwrap();
        for index in matching_indexes {
            if let Some(entry) = cache.get_mut(index) {
                *entry = None;
            }
        }
        // Snapshots contain all Codex accounts and cannot be safely patched
        // in place without trusting a stale response.  Dropping this one
        // provider snapshot lets the ordinary background refresh rebuild it.
        self.state.quota_snapshots.lock().unwrap().remove("codex");
    }
}

/// Only a stable ChatGPT account identity may authorize unattended spending.
/// `manual-N`, labels, and filenames can be reordered or renamed, so accepting
/// them would let a policy drift to a different account after a reload.
pub(crate) fn validate_automation_account_key(
    state: &crate::AppState,
    account_key: &str,
) -> Result<(), String> {
    resolve_automation_token(state, account_key)?;
    crate::target::codex::app_server::validate_config_for_account(state.cfg.as_ref(), account_key)?;
    Ok(())
}

/// A disabled policy must remain removable even after its credential was
/// deleted or temporarily disabled.  Keep the identity requirement just as
/// strict as an enabled policy, but do not require a live token merely to
/// turn automation off.
pub(crate) fn validate_automation_policy_account_key(account_key: &str) -> Result<(), String> {
    normalized_automation_account_key(account_key).map(|_| ())
}

fn normalized_automation_account_key(account_key: &str) -> Result<String, String> {
    let account_key = normalized_identifier(account_key, "Codex account key")?;
    let Some(account_id) = account_key.strip_prefix("codex:account_id:") else {
        return Err(
            "automatic reset-credit policies require a stable codex:account_id:<id> account"
                .to_string(),
        );
    };
    if account_id.trim().is_empty() {
        return Err("Codex account ID must not be empty".to_string());
    }
    Ok(account_key)
}

fn resolve_automation_token(
    state: &crate::AppState,
    account_key: &str,
) -> Result<crate::target::codex::tokens::UpstreamToken, String> {
    let account_key = normalized_automation_account_key(account_key)?;
    let account_id = account_key
        .strip_prefix("codex:account_id:")
        .expect("normalized automation account key has its required prefix");

    let matches = state
        .tokens
        .lock()
        .unwrap()
        .iter()
        .filter(|token| {
            token.enabled
                && token
                    .account_id
                    .as_deref()
                    .is_some_and(|candidate| candidate == account_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [token] => Ok(token.clone()),
        [] => Err("the selected stable Codex account is not currently enabled".to_string()),
        _ => Err(
            "multiple enabled Codex credentials match this account; resolve the duplicate before enabling automation"
                .to_string(),
        ),
    }
}

/// Network reconciliation can take minutes when a local App Server child is
/// slow. Production must take a fresh wall-clock reading at each durable
/// claim/settlement, while unit tests need a fixed logical clock to prove
/// exact expiry boundaries deterministically.
trait ResetCreditClock {
    fn now(&self) -> DateTime<Utc>;
}

struct SystemResetCreditClock;

impl ResetCreditClock for SystemResetCreditClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

struct FixedResetCreditClock(DateTime<Utc>);

impl ResetCreditClock for FixedResetCreditClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

/// Starts the live durable reconciler.  The immediate first tick performs
/// restart catch-up; durable `next_scan_at` values prevent repeated scans.
pub(crate) async fn background_worker(state: crate::AppState) {
    let worker_id = format!("codex-reset-credit:{}", Uuid::new_v4());
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.codex_reset_credit_wake.notified() => {}
        }
        if let Err(error) = run_once(&state, &worker_id).await {
            warn!(%error, "Codex reset-credit reconciliation failed");
        }
    }
}

async fn run_once(
    state: &crate::AppState,
    worker_id: &str,
) -> Result<CodexResetCreditRunSummary, String> {
    let upstream = LiveResetCreditUpstream { state };
    let clock = SystemResetCreditClock;
    run_once_at_path_with_clock(
        &crate::api_key_policy_store::policy_db_path(&state.cfg),
        &upstream,
        worker_id,
        &clock,
    )
    .await
}

/// Deterministic entry point used by local unit tests and administrative
/// callers that deliberately supply a logical time. The live worker uses the
/// wall-clock variant above so slow scans cannot make an expired action look
/// current.
pub(crate) async fn run_once_at(
    state: &crate::AppState,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<CodexResetCreditRunSummary, String> {
    let upstream = LiveResetCreditUpstream { state };
    let clock = FixedResetCreditClock(now);
    run_once_at_path_with_clock(
        &crate::api_key_policy_store::policy_db_path(&state.cfg),
        &upstream,
        worker_id,
        &clock,
    )
    .await
}

async fn run_once_at_path_with<U: ResetCreditUpstream + ?Sized>(
    path: &Path,
    upstream: &U,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<CodexResetCreditRunSummary, String> {
    let clock = FixedResetCreditClock(now);
    run_once_at_path_with_clock(path, upstream, worker_id, &clock).await
}

async fn run_once_at_path_with_clock<
    U: ResetCreditUpstream + ?Sized,
    C: ResetCreditClock + ?Sized,
>(
    path: &Path,
    upstream: &U,
    worker_id: &str,
    clock: &C,
) -> Result<CodexResetCreditRunSummary, String> {
    let mut summary = CodexResetCreditRunSummary::default();

    // Alternate a current, single due action with a current, single policy
    // scan.  The previous implementation collected up to 256 actions and
    // executed that whole stale snapshot before scanning policies.  Since one
    // App Server operation can take minutes, an expiry-less submitted action
    // could keep an expiring credit waiting past its deadline.  Re-querying
    // each one-row action turn also lets a competing gateway process settle
    // or lease it without making this worker run obsolete work.
    let mut action_turns = 0usize;
    let mut policy_turns = 0usize;
    let mut prefer_action = true;
    for _ in 0..MAX_RECONCILIATION_TURNS_PER_TICK {
        let can_try_action = action_turns < MAX_ACTION_ROWS_PER_TICK;
        let can_try_policy = policy_turns < MAX_POLICY_ROWS_PER_TICK;
        if !can_try_action && !can_try_policy {
            break;
        }

        let first_is_action = prefer_action && can_try_action || !can_try_policy;
        let first_ran = if first_is_action {
            let ran =
                reconcile_one_due_action_at_path(path, upstream, worker_id, clock, &mut summary)
                    .await?;
            if ran {
                action_turns += 1;
            }
            ran
        } else {
            let ran = reconcile_one_due_policy_scan_at_path(
                path,
                upstream,
                worker_id,
                clock,
                &mut summary,
            )
            .await?;
            if ran {
                policy_turns += 1;
            }
            ran
        };
        if first_ran {
            prefer_action = !first_is_action;
            continue;
        }

        // If the preferred queue is empty (or a competing process took the
        // only leaseable row), give the other class one turn rather than
        // ending a healthy reconciliation pass early.
        let can_try_other = if first_is_action {
            policy_turns < MAX_POLICY_ROWS_PER_TICK
        } else {
            action_turns < MAX_ACTION_ROWS_PER_TICK
        };
        if !can_try_other {
            break;
        }
        let other_ran = if first_is_action {
            let ran = reconcile_one_due_policy_scan_at_path(
                path,
                upstream,
                worker_id,
                clock,
                &mut summary,
            )
            .await?;
            if ran {
                policy_turns += 1;
            }
            ran
        } else {
            let ran =
                reconcile_one_due_action_at_path(path, upstream, worker_id, clock, &mut summary)
                    .await?;
            if ran {
                action_turns += 1;
            }
            ran
        };
        if other_ran {
            prefer_action = first_is_action;
            continue;
        }
        break;
    }
    Ok(summary)
}

/// Reconciles at most one currently-due action. `true` means a row was
/// selected for this turn, even if a concurrent process claimed it before our
/// lease update. Counting that as a turn avoids repeatedly hot-looping on the
/// same contested row while other ready work waits.
async fn reconcile_one_due_action_at_path<
    U: ResetCreditUpstream + ?Sized,
    C: ResetCreditClock + ?Sized,
>(
    path: &Path,
    upstream: &U,
    worker_id: &str,
    clock: &C,
    summary: &mut CodexResetCreditRunSummary,
) -> Result<bool, String> {
    let Some(action) = next_due_action_at_path(path, clock.now())? else {
        return Ok(false);
    };
    if process_due_action_at_path_with_clock(path, upstream, &action.id, worker_id, clock).await? {
        summary.processed_actions += 1;
    }
    Ok(true)
}

/// Claims and completes one current policy scan.  Keeping this separate from
/// the scheduler makes its one-for-one fairness with due actions explicit and
/// prevents a slow profile from hiding a newly due final action behind an
/// entire policy batch.
async fn reconcile_one_due_policy_scan_at_path<
    U: ResetCreditUpstream + ?Sized,
    C: ResetCreditClock + ?Sized,
>(
    path: &Path,
    upstream: &U,
    worker_id: &str,
    clock: &C,
    summary: &mut CodexResetCreditRunSummary,
) -> Result<bool, String> {
    let Some(policy) = claim_due_policy_scans_at_path(path, worker_id, clock.now())?
        .into_iter()
        .next()
    else {
        return Ok(false);
    };
    summary.claimed_policy_scans += 1;
    let scan_result = match upstream.fresh_state(&policy.account_key).await {
        Ok(fresh) => {
            let now = clock.now();
            if let Some(candidate) = select_automatic_candidate(&policy, &fresh, now) {
                match register_automatic_action_for_claimed_policy_at_path(
                    path, &policy, worker_id, &candidate, now,
                ) {
                    Ok(Some(_)) => {
                        summary.registered_actions += 1;
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(error) => Err(error),
                }
            } else {
                Ok(())
            }
        }
        Err(_) => {
            summary.upstream_failures += 1;
            Ok(())
        }
    };

    // Always release the policy lease first. A failed upstream read must not
    // turn into a permanently wedged policy, and a database error from action
    // registration must not leave a lease until process restart.
    finish_policy_scan_at_path(path, &policy.account_key, worker_id, clock.now())?;
    scan_result?;
    // A registered action stays due in SQLite. The next scheduler turn is an
    // action turn, which re-selects it alongside any older final-deadline
    // work. Do not nest App Server redemption inside a policy turn: a large
    // policy batch could otherwise recreate the same starvation that this
    // scheduler is designed to prevent.
    Ok(true)
}

async fn process_due_action_at_path_with_clock<
    U: ResetCreditUpstream + ?Sized,
    C: ResetCreditClock + ?Sized,
>(
    path: &Path,
    upstream: &U,
    action_id: &str,
    worker_id: &str,
    clock: &C,
) -> Result<bool, String> {
    match process_action_at_path_with_clock(path, upstream, action_id, worker_id, clock, false)
        .await
    {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        // Upstream failures are durably rescheduled inside the action. A
        // storage error is still surfaced to the worker for observability.
        Err(error) => Err(error),
    }
}

/// Runs a single registered action.  All network I/O happens outside SQLite
/// transactions.  A POST is preceded by a durable `submitted` mark, and any
/// uncertain result is retried only with the same credit and idempotency key.
async fn process_action_at_path_with<U: ResetCreditUpstream + ?Sized>(
    path: &Path,
    upstream: &U,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
    force: bool,
) -> Result<Option<CodexResetCreditAction>, String> {
    let clock = FixedResetCreditClock(now);
    process_action_at_path_with_clock(path, upstream, action_id, worker_id, &clock, force).await
}

async fn process_action_at_path_with_clock<
    U: ResetCreditUpstream + ?Sized,
    C: ResetCreditClock + ?Sized,
>(
    path: &Path,
    upstream: &U,
    action_id: &str,
    worker_id: &str,
    clock: &C,
    force: bool,
) -> Result<Option<CodexResetCreditAction>, String> {
    let now = clock.now();
    let action = match claim_action_at_path(path, action_id, worker_id, now, force)? {
        ResetCreditActionClaim::Acquired(action) => action,
        ResetCreditActionClaim::Terminal(action) => return Ok(Some(action)),
        ResetCreditActionClaim::Busy | ResetCreditActionClaim::NotDue => return Ok(None),
    };
    let policy = policy_at_path(path, &action.account_key)?;
    let automatic = action.trigger == ResetCreditActionTrigger::Automatic.as_db();
    if automatic && !policy.as_ref().is_some_and(|policy| policy.enabled) {
        if action.state == ResetCreditActionState::Submitted {
            // A prior POST may have succeeded before its response was lost.
            // Disabling future automatic selection must not rewrite that
            // ambiguity to `deferred`, which would eventually release the
            // account gate as a harmless expiration.
            reschedule_submitted_action_at_path(
                path,
                &action.id,
                worker_id,
                submitted_policy_disabled_retry_at(&action, now)?,
                now,
            )?;
        } else {
            let next = action_retry_at(&action, &ActionSettings::default(), now)?;
            defer_action_at_path(
                path,
                &action.id,
                worker_id,
                DeferredActionOutcome::PolicyDisabled,
                next,
                now,
            )?;
        }
        return action_at_path(path, &action.id);
    }
    let settings = ActionSettings::from_action(&action, policy.as_ref());

    renew_action_lease_at_path(path, &action.id, worker_id, clock.now())?;
    let fresh = match upstream.fresh_state(&action.account_key).await {
        Ok(fresh) => fresh,
        Err(_) => {
            let now = clock.now();
            reschedule_after_fresh_failure(path, &action, worker_id, now)?;
            return action_at_path(path, &action.id);
        }
    };
    let now = clock.now();
    if let Some(action) = settle_expired_claimed_action_at_path(
        path,
        &action,
        worker_id,
        now,
        action.state == ResetCreditActionState::Submitted,
    )? {
        return Ok(Some(action));
    }

    let submitted = action.state == ResetCreditActionState::Submitted;
    match selected_credit_status(&fresh, &action) {
        SelectedCreditStatus::Unknown => {
            if submitted {
                // The pre-POST durable mark intentionally admits an
                // ambiguous crash window. Without fresh proof that the
                // selected credit is still available, do not send a retry
                // merely because its idempotency key is reusable.
                mark_action_manual_review_at_path(path, &action.id, worker_id, now)?;
            } else {
                defer_action_at_path(
                    path,
                    &action.id,
                    worker_id,
                    DeferredActionOutcome::NoEligibleLimit,
                    action_retry_at(&action, &settings, now)?,
                    now,
                )?;
            }
            return action_at_path(path, &action.id);
        }
        SelectedCreditStatus::Unavailable => {
            if submitted {
                // A prior POST may have consumed this exact credit. It is not
                // safe to reinterpret that ambiguity as an ordinary missing
                // credit and open the account gate for another automatic one.
                mark_action_manual_review_at_path(path, &action.id, worker_id, now)?;
            } else {
                mark_action_no_credit_at_path(path, &action.id, worker_id, now)?;
            }
            return action_at_path(path, &action.id);
        }
        SelectedCreditStatus::Available => {}
    }
    if !fresh.rate_limit_reached {
        if submitted {
            // A restarted submitted action might have posted before the
            // crash, or might not. Never issue a delayed automatic consume
            // after the reached limit has naturally cleared.
            mark_action_manual_review_at_path(path, &action.id, worker_id, now)?;
        } else {
            defer_action_at_path(
                path,
                &action.id,
                worker_id,
                DeferredActionOutcome::NoEligibleLimit,
                action_retry_at(&action, &settings, now)?,
                now,
            )?;
        }
        return action_at_path(path, &action.id);
    }
    if automatic {
        let natural_reset_after_seconds = fresh.natural_reset_after_seconds;
        let deferred_outcome = match natural_reset_after_seconds {
            None => Some(DeferredActionOutcome::NaturalResetUnknown),
            Some(seconds)
                if settings.min_natural_reset_remaining_minutes > 0
                    && seconds
                        <= settings
                            .min_natural_reset_remaining_minutes
                            .saturating_mul(60) =>
            {
                Some(DeferredActionOutcome::NaturalResetSoon)
            }
            Some(_) => None,
        };
        if let Some(outcome) = deferred_outcome {
            if submitted {
                // The submit marker might precede a crash before the POST.
                // Preserve the one-credit gate for an operator rather than
                // sending a late retry after the configured no-spend point.
                mark_action_manual_review_at_path(path, &action.id, worker_id, now)?;
            } else {
                defer_action_at_path(
                    path,
                    &action.id,
                    worker_id,
                    outcome,
                    action_retry_at(&action, &settings, now)?,
                    now,
                )?;
            }
            return action_at_path(path, &action.id);
        }
    }
    if !submitted {
        // Persist before sending. A crash in the next instruction can only
        // cause an idempotent retry of this exact action.
        mark_action_submitted_at_path(path, &action.id, worker_id, now)?;
    }

    let now = clock.now();
    if let Some(action) =
        settle_expired_claimed_action_at_path(path, &action, worker_id, now, true)?
    {
        return Ok(Some(action));
    }
    renew_action_lease_at_path(path, &action.id, worker_id, now)?;
    let consumed = match upstream
        .consume(
            &action.account_key,
            &action.credit_id,
            &action.idempotency_key,
        )
        .await
    {
        Ok(consumed) => consumed,
        Err(_) => {
            let now = clock.now();
            reschedule_submitted_action_at_path(
                path,
                &action.id,
                worker_id,
                uncertain_retry_at(&action, now)?,
                now,
            )?;
            return action_at_path(path, &action.id);
        }
    };

    match consumed.outcome.as_str() {
        "reset" | "already_redeemed" => {
            renew_action_lease_at_path(path, &action.id, worker_id, clock.now())?;
            match upstream.fresh_state(&action.account_key).await {
                Ok(post) if post.rate_limit_cleared => {
                    let now = clock.now();
                    mark_action_verified_at_path(path, &action.id, worker_id, now)?;
                    upstream.on_verified(&action.account_key);
                }
                Ok(_) => {
                    let now = clock.now();
                    // The provider accepted the logical redemption but the
                    // authoritative post-read cannot prove it cleared the limit.
                    // Do not select/spend another credit automatically.
                    mark_action_manual_review_at_path(path, &action.id, worker_id, now)?;
                }
                Err(_) => {
                    let now = clock.now();
                    reschedule_submitted_action_at_path(
                        path,
                        &action.id,
                        worker_id,
                        uncertain_retry_at(&action, now)?,
                        now,
                    )?;
                }
            }
        }
        "nothing_to_reset" => {
            let now = clock.now();
            defer_action_at_path(
                path,
                &action.id,
                worker_id,
                DeferredActionOutcome::NoEligibleLimit,
                action_retry_at(&action, &settings, now)?,
                now,
            )?;
        }
        "no_credit" => mark_action_no_credit_at_path(path, &action.id, worker_id, clock.now())?,
        _ => mark_action_manual_review_at_path(path, &action.id, worker_id, clock.now())?,
    }
    action_at_path(path, &action.id)
}

#[derive(Clone, Copy)]
struct ActionSettings {
    scan_interval_minutes: u64,
    final_attempt_minutes: u64,
    min_natural_reset_remaining_minutes: u64,
}

impl Default for ActionSettings {
    fn default() -> Self {
        Self {
            scan_interval_minutes: DEFAULT_SCAN_INTERVAL_MINUTES,
            final_attempt_minutes: DEFAULT_FINAL_ATTEMPT_MINUTES,
            // A deliberate manual action should not be silently discarded
            // merely because the natural reset is nearby.
            min_natural_reset_remaining_minutes: 0,
        }
    }
}

impl ActionSettings {
    fn from_action(
        action: &CodexResetCreditAction,
        policy: Option<&CodexResetCreditPolicy>,
    ) -> Self {
        if action.trigger == ResetCreditActionTrigger::Automatic.as_db() {
            if let Some(policy) = policy {
                return Self {
                    scan_interval_minutes: policy.scan_interval_minutes,
                    final_attempt_minutes: policy.final_attempt_minutes,
                    min_natural_reset_remaining_minutes: policy.min_natural_reset_remaining_minutes,
                };
            }
        }
        Self::default()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectedCreditStatus {
    Unknown,
    Unavailable,
    Available,
}

fn selected_credit_status(
    fresh: &crate::target::codex::quota::FreshRateLimitResetState,
    action: &CodexResetCreditAction,
) -> SelectedCreditStatus {
    if fresh.available_credit_count == 0 {
        return SelectedCreditStatus::Unavailable;
    }
    let Some(credits) = fresh.credits.as_ref() else {
        return SelectedCreditStatus::Unknown;
    };
    match credits.iter().find(|credit| credit.id == action.credit_id) {
        Some(credit) if credit_is_available_codex_reset(credit) => SelectedCreditStatus::Available,
        _ => SelectedCreditStatus::Unavailable,
    }
}

fn select_automatic_candidate(
    policy: &CodexResetCreditPolicy,
    fresh: &crate::target::codex::quota::FreshRateLimitResetState,
    now: DateTime<Utc>,
) -> Option<ResetCreditActionCandidate> {
    if fresh.available_credit_count == 0 {
        return None;
    }
    let credits = fresh.credits.as_ref()?;
    credits
        .iter()
        .filter(|credit| credit_is_available_codex_reset(credit))
        .filter_map(|credit| {
            let expires_at = credit
                .expires_at
                .as_deref()
                .and_then(parse_upstream_timestamp)?;
            let remaining = expires_at.signed_duration_since(now).num_seconds();
            if remaining < (MIN_AUTOMATIC_EXPIRY_LEAD_MINUTES.saturating_mul(60) as i64)
                || remaining > (policy.expiry_window_minutes.saturating_mul(60) as i64)
            {
                return None;
            }
            Some(ResetCreditActionCandidate {
                account_key: policy.account_key.clone(),
                credit_id: credit.id.clone(),
                reset_type: credit.reset_type.clone(),
                credit_expires_at: Some(expires_at),
                trigger: ResetCreditActionTrigger::Automatic,
            })
        })
        .min_by(
            |left, right| match left.credit_expires_at.cmp(&right.credit_expires_at) {
                Ordering::Equal => left.credit_id.cmp(&right.credit_id),
                order => order,
            },
        )
}

fn credit_is_available_codex_reset(
    credit: &crate::target::codex::quota::RateLimitResetCredit,
) -> bool {
    // `status` and `resetType` are documented App Server enums. Treat an
    // unknown lookalike as ineligible rather than normalizing it into a
    // spendable credit.
    credit.status == "available"
        && !credit.id.is_empty()
        && credit.id.trim() == credit.id
        && !credit.id.chars().any(char::is_control)
        && credit.reset_type == "codexRateLimits"
}

fn parse_upstream_timestamp(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.with_timezone(&Utc));
    }
    let timestamp = value.parse::<i64>().ok()?;
    // ChatGPT App Server uses Unix seconds. Accept millisecond-form values as
    // well because the existing HTTP endpoint has historically returned both.
    if timestamp.unsigned_abs() >= 100_000_000_000 {
        DateTime::from_timestamp_millis(timestamp)
    } else {
        DateTime::from_timestamp(timestamp, 0)
    }
}

fn action_retry_at(
    action: &CodexResetCreditAction,
    settings: &ActionSettings,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    let ordinary = add_minutes(now, settings.scan_interval_minutes)?;
    let Some(expires_at) = action
        .credit_expires_at
        .as_deref()
        .map(parse_stamp)
        .transpose()?
    else {
        return Ok(ordinary);
    };
    let final_attempt = expires_at
        .checked_sub_signed(ChronoDuration::seconds(minutes_to_seconds(
            settings.final_attempt_minutes,
        )?))
        .ok_or("Codex reset-credit final-attempt timestamp overflow".to_string())?;
    if final_attempt > now {
        Ok(ordinary.min(final_attempt))
    } else {
        // We have already performed the final useful check. Do not form a
        // tight retry loop in the remaining few minutes.
        Ok(expires_at)
    }
}

fn uncertain_retry_at(
    action: &CodexResetCreditAction,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    let retry = add_minutes(now, 1)?;
    let expiry = action
        .credit_expires_at
        .as_deref()
        .map(parse_stamp)
        .transpose()?;
    Ok(expiry.map_or(retry, |expiry| retry.min(expiry)))
}

/// Keep a durable action lease alive across a bounded App Server child call.
/// The worker uses a fresh wall-clock reading for this in production; a lost
/// lease is a hard stop before another consume can be sent.
fn renew_action_lease_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let action_id = normalized_identifier(action_id, "reset-credit action ID")?;
    let worker_id = normalized_identifier(worker_id, "reset-credit worker ID")?;
    let lease_until = stamp(add_std(now, ACTION_LEASE)?);
    let now_text = stamp(now);
    let mut connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let changed = transaction
        .execute(
            "UPDATE codex_reset_credit_actions
             SET lease_expires_at=?3,updated_at=?4
             WHERE id=?1 AND lease_owner=?2 AND lease_expires_at>?4
               AND state IN ('pending','deferred','submitted')",
            params![&action_id, &worker_id, &lease_until, &now_text],
        )
        .map_err(sql)?;
    if changed != 1 {
        return Err("Codex reset-credit action lease was lost before upstream work".to_string());
    }
    transaction.commit().map_err(sql)
}

/// Re-check expiry after a slow fresh read and immediately before a consume.
/// A submitted action is intentionally promoted to manual review, because a
/// previous POST may already have spent its credit; an unsubmitted action can
/// safely become ordinary expiration.
fn settle_expired_claimed_action_at_path(
    path: &Path,
    action: &CodexResetCreditAction,
    worker_id: &str,
    now: DateTime<Utc>,
    submitted: bool,
) -> Result<Option<CodexResetCreditAction>, String> {
    let Some(expires_at) = action
        .credit_expires_at
        .as_deref()
        .map(parse_stamp)
        .transpose()?
    else {
        return Ok(None);
    };
    if expires_at > now {
        return Ok(None);
    }
    let (state, outcome) = if submitted {
        (
            ResetCreditActionState::ManualReview,
            ResetCreditAttemptOutcome::VerificationFailed,
        )
    } else {
        (
            ResetCreditActionState::Expired,
            ResetCreditAttemptOutcome::Expired,
        )
    };
    with_claimed_action_at_path(path, &action.id, worker_id, now, |transaction, id| {
        finish_claimed_action_tx(transaction, id, Some(worker_id), state, outcome, now, None)
    })?;
    action_at_path(path, &action.id)
}

/// Disabling a policy stops new automatic selections. It does not make an
/// uncertain submitted POST safe to forget. Wake again at expiry so the
/// durable claim can convert it to `manual_review` without issuing another
/// consume request while the policy remains disabled.
fn submitted_policy_disabled_retry_at(
    action: &CodexResetCreditAction,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    let expiry = action
        .credit_expires_at
        .as_deref()
        .map(parse_stamp)
        .transpose()?;
    match expiry {
        Some(expiry) => Ok(expiry),
        None => add_minutes(now, DEFAULT_SCAN_INTERVAL_MINUTES),
    }
}

fn reschedule_after_fresh_failure(
    path: &Path,
    action: &CodexResetCreditAction,
    worker_id: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    if action.state == ResetCreditActionState::Submitted {
        reschedule_submitted_action_at_path(
            path,
            &action.id,
            worker_id,
            uncertain_retry_at(action, now)?,
            now,
        )
    } else {
        defer_action_at_path(
            path,
            &action.id,
            worker_id,
            DeferredActionOutcome::NoEligibleLimit,
            uncertain_retry_at(action, now)?,
            now,
        )
    }
}

/// Redeems a selected credit through the same action ledger used by unattended
/// automation.  The legacy endpoint accepts a client idempotency key, but the
/// durable action ID is authoritative: retries for an account/credit pair
/// recover the original persisted key even after a process crash.
pub(crate) async fn consume_manually(
    state: &crate::AppState,
    form: crate::target::codex::quota::ConsumeRateLimitResetForm,
) -> Result<serde_json::Value, String> {
    let token = crate::target::codex::quota::select_token_for_reset(state, &form)?;
    let account_key = crate::codex_stats_key(&token);
    validate_automation_account_key(state, &account_key)?;
    let credit_id = form.credit_id.as_deref().ok_or_else(|| {
        "select a concrete reset credit ID; automatic selection cannot be safely coordinated"
            .to_string()
    })?;
    let upstream = LiveResetCreditUpstream { state };
    let fresh = upstream.fresh_state(&account_key).await?;
    let candidate = select_manual_candidate(&account_key, credit_id, &fresh, Utc::now())?;
    let path = crate::api_key_policy_store::policy_db_path(&state.cfg);
    let action = register_action_at_path(&path, &candidate, Utc::now())?;
    let worker_id = format!("codex-reset-credit-manual:{}", Uuid::new_v4());
    let action_id = action.id.clone();
    let clock = SystemResetCreditClock;
    let _ =
        process_action_at_path_with_clock(&path, &upstream, &action_id, &worker_id, &clock, true)
            .await?;
    let action = action_at_path(&path, &action_id)?
        .ok_or_else(|| "reset-credit action disappeared before completion".to_string())?;
    let (ok, message) = match action.state {
        ResetCreditActionState::Verified => (true, "Usage limit reset was verified."),
        ResetCreditActionState::Deferred => (
            true,
            "Credit was retained; no eligible Codex rate-limit window is currently resettable.",
        ),
        ResetCreditActionState::Submitted => (
            true,
            "Redemption is pending safe verification with the persisted idempotency key.",
        ),
        ResetCreditActionState::NoCredit => (false, "The selected reset credit is no longer available."),
        ResetCreditActionState::ManualReview => (
            false,
            "The redemption outcome is ambiguous and needs manual review; no additional credit was selected.",
        ),
        ResetCreditActionState::Expired => (false, "The selected reset credit has expired."),
        ResetCreditActionState::Pending => (true, "Redemption has been queued."),
    };
    Ok(serde_json::json!({
        "ok": ok,
        "outcome": action.state,
        "action": action,
        "message": message,
    }))
}

fn select_manual_candidate(
    account_key: &str,
    credit_id: &str,
    fresh: &crate::target::codex::quota::FreshRateLimitResetState,
    now: DateTime<Utc>,
) -> Result<ResetCreditActionCandidate, String> {
    let credit_id = opaque_identifier(credit_id, "reset-credit ID")?;
    if fresh.available_credit_count == 0 {
        return Err(
            "the selected reset credit is not currently available for Codex limits".to_string(),
        );
    }
    let credits = fresh.credits.as_ref().ok_or_else(|| {
        "the upstream did not disclose concrete credit IDs; refusing an uncoordinated redemption"
            .to_string()
    })?;
    let credit = credits
        .iter()
        .find(|credit| credit.id == credit_id)
        .filter(|credit| credit_is_available_codex_reset(credit))
        .ok_or_else(|| {
            "the selected reset credit is not currently available for Codex limits".to_string()
        })?;
    let expires_at = match credit.expires_at.as_deref() {
        Some(value) => parse_upstream_timestamp(value).ok_or_else(|| {
            "the selected reset credit has an invalid expiry; refusing unsafe redemption"
                .to_string()
        })?,
        None => {
            return Ok(ResetCreditActionCandidate {
                account_key: account_key.to_string(),
                credit_id,
                reset_type: credit.reset_type.clone(),
                credit_expires_at: None,
                trigger: ResetCreditActionTrigger::Manual,
            })
        }
    };
    if expires_at <= now {
        return Err("the selected reset credit has expired".to_string());
    }
    Ok(ResetCreditActionCandidate {
        account_key: account_key.to_string(),
        credit_id,
        reset_type: credit.reset_type.clone(),
        credit_expires_at: Some(expires_at),
        trigger: ResetCreditActionTrigger::Manual,
    })
}

fn with_claimed_action_at_path(
    path: &Path,
    action_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
    operation: impl FnOnce(&rusqlite::Transaction<'_>, &str) -> Result<(), String>,
) -> Result<(), String> {
    let action_id = normalized_identifier(action_id, "reset-credit action ID")?;
    let worker_id = normalized_identifier(worker_id, "reset-credit worker ID")?;
    let mut connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let owned: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_reset_credit_actions
               WHERE id=?1 AND lease_owner=?2 AND lease_expires_at>?3)",
            params![&action_id, &worker_id, stamp(now)],
            |row| row.get(0),
        )
        .map_err(sql)?;
    if !owned {
        return Err("Codex reset-credit action lease was lost".to_string());
    }
    operation(&transaction, &action_id)?;
    transaction.commit().map_err(sql)
}

fn finish_claimed_action_tx(
    transaction: &rusqlite::Transaction<'_>,
    action_id: &str,
    worker_id: Option<&str>,
    state: ResetCreditActionState,
    outcome: ResetCreditAttemptOutcome,
    now: DateTime<Utc>,
    next_attempt_at: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let now_text = stamp(now);
    let next = next_attempt_at
        .map(stamp)
        .unwrap_or_else(|| now_text.clone());
    let changed = match worker_id {
        Some(worker_id) => transaction.execute(
            "UPDATE codex_reset_credit_actions
             SET state=?3,next_attempt_at=?4,lease_owner=NULL,lease_expires_at=NULL,
                 last_outcome=?5,updated_at=?6
             WHERE id=?1 AND lease_owner=?2",
            params![
                action_id,
                worker_id,
                state.as_db(),
                next,
                outcome.as_db(),
                now_text
            ],
        ),
        None => transaction.execute(
            "UPDATE codex_reset_credit_actions
             SET state=?2,next_attempt_at=?3,lease_owner=NULL,lease_expires_at=NULL,
                 last_outcome=?4,updated_at=?5
             WHERE id=?1",
            params![action_id, state.as_db(), next, outcome.as_db(), now_text],
        ),
    }
    .map_err(sql)?;
    if changed != 1 {
        return Err("Codex reset-credit action changed before settlement".to_string());
    }
    append_attempt(transaction, action_id, now, outcome)
}

fn append_attempt(
    transaction: &rusqlite::Transaction<'_>,
    action_id: &str,
    now: DateTime<Utc>,
    outcome: ResetCreditAttemptOutcome,
) -> Result<(), String> {
    transaction
        .execute(
            "INSERT INTO codex_reset_credit_attempts(action_id,occurred_at,outcome) VALUES(?1,?2,?3)",
            params![action_id, stamp(now), outcome.as_db()],
        )
        .map_err(sql)?;
    Ok(())
}

fn select_policy(
    connection: &Connection,
    account_key: &str,
) -> Result<Option<CodexResetCreditPolicy>, String> {
    connection
        .query_row(
            "SELECT account_key,enabled,scan_interval_seconds,expiry_window_seconds,
                    final_attempt_seconds,min_natural_reset_seconds,next_scan_at,updated_at
             FROM codex_reset_credit_policies WHERE account_key=?1",
            [account_key],
            policy_from_row,
        )
        .optional()
        .map_err(sql)
}

fn policy_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodexResetCreditPolicy> {
    let scan_seconds: i64 = row.get(2)?;
    let expiry_seconds: i64 = row.get(3)?;
    let final_seconds: i64 = row.get(4)?;
    let natural_seconds: i64 = row.get(5)?;
    let final_attempt_minutes = seconds_to_minutes(final_seconds)?;
    if final_attempt_minutes == 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid zero reset-credit final-attempt duration",
            )),
        ));
    }
    Ok(CodexResetCreditPolicy {
        account_key: row.get(0)?,
        enabled: row.get::<_, i64>(1)? != 0,
        scan_interval_minutes: seconds_to_minutes(scan_seconds)?,
        expiry_window_minutes: seconds_to_minutes(expiry_seconds)?,
        final_attempt_minutes,
        min_natural_reset_remaining_minutes: seconds_to_minutes(natural_seconds)?,
        next_scan_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn select_action(
    connection: &Connection,
    id: &str,
) -> Result<Option<CodexResetCreditAction>, String> {
    connection
        .query_row(
            "SELECT id,account_key,credit_id,reset_type,credit_expires_at,idempotency_key,
                    state,trigger,next_attempt_at,attempt_count,last_outcome,created_at,updated_at,
                    submitted_at,verified_at
             FROM codex_reset_credit_actions WHERE id=?1",
            [id],
            action_from_row,
        )
        .optional()
        .map_err(sql)
}

fn select_action_by_account_credit(
    connection: &Connection,
    account_key: &str,
    credit_id: &str,
) -> Result<Option<CodexResetCreditAction>, String> {
    connection
        .query_row(
            "SELECT id,account_key,credit_id,reset_type,credit_expires_at,idempotency_key,
                    state,trigger,next_attempt_at,attempt_count,last_outcome,created_at,updated_at,
                    submitted_at,verified_at
             FROM codex_reset_credit_actions WHERE account_key=?1 AND credit_id=?2",
            params![account_key, credit_id],
            action_from_row,
        )
        .optional()
        .map_err(sql)
}

/// Returns the single durable account-level action that must prevent another
/// credit from being selected. This query runs inside the same immediate
/// transaction as registration, so it also coordinates independent gateway
/// processes using the shared SQLite policy store.
fn select_blocking_action_for_account(
    connection: &Connection,
    account_key: &str,
    now: DateTime<Utc>,
) -> Result<Option<CodexResetCreditAction>, String> {
    select_blocking_action_for_account_except(connection, account_key, None, now)
}

fn select_blocking_action_for_account_except(
    connection: &Connection,
    account_key: &str,
    except_action_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<CodexResetCreditAction>, String> {
    select_account_gate_action(connection, account_key, except_action_id, false, now)
}

/// An expiry-less manual action has never sent a redemption and cannot expire
/// into a second spend.  Only automatic selection may temporarily look past
/// it, so an expiring earned credit is not lost while a client has a dormant
/// manual request.  Manual registration always uses the strict gate above.
fn select_automatic_blocking_action_for_account(
    connection: &Connection,
    account_key: &str,
    now: DateTime<Utc>,
) -> Result<Option<CodexResetCreditAction>, String> {
    select_automatic_blocking_action_for_account_except(connection, account_key, None, now)
}

fn select_automatic_blocking_action_for_account_except(
    connection: &Connection,
    account_key: &str,
    except_action_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<CodexResetCreditAction>, String> {
    select_account_gate_action(connection, account_key, except_action_id, true, now)
}

fn select_account_gate_action(
    connection: &Connection,
    account_key: &str,
    except_action_id: Option<&str>,
    ignore_expiryless_deferred_manual: bool,
    now: DateTime<Utc>,
) -> Result<Option<CodexResetCreditAction>, String> {
    connection
        .query_row(
            "SELECT id,account_key,credit_id,reset_type,credit_expires_at,idempotency_key,
                    state,trigger,next_attempt_at,attempt_count,last_outcome,created_at,updated_at,
                    submitted_at,verified_at
             FROM codex_reset_credit_actions
             WHERE account_key=?1
               AND state IN ('pending','deferred','submitted','manual_review')
               -- An expiry-less manual click can be skipped only while an
               -- automatic policy is selecting a fresh expiring credit. It
               -- is otherwise an account-wide gate.
               AND (?3=0 OR NOT (
                    trigger='manual' AND state='deferred' AND credit_expires_at=''
                    AND (lease_expires_at IS NULL OR lease_expires_at<=?4)
               ))
               AND (?2 IS NULL OR id<>?2)
             ORDER BY CASE state
                 WHEN 'submitted' THEN 0
                 WHEN 'manual_review' THEN 1
                 WHEN 'pending' THEN 2
                 ELSE 3
             END, created_at, id
            LIMIT 1",
            params![
                account_key,
                except_action_id,
                bool_to_sql(ignore_expiryless_deferred_manual),
                stamp(now)
            ],
            action_from_row,
        )
        .optional()
        .map_err(sql)
}

fn action_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodexResetCreditAction> {
    let state: String = row.get(6)?;
    let state = ResetCreditActionState::from_db(&state).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid reset-credit state",
            )),
        )
    })?;
    let attempts: i64 = row.get(9)?;
    let attempt_count = u64::try_from(attempts)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(9, attempts))?;
    Ok(CodexResetCreditAction {
        id: row.get(0)?,
        account_key: row.get(1)?,
        credit_id: row.get(2)?,
        reset_type: row.get(3)?,
        credit_expires_at: row
            .get::<_, String>(4)
            .map(|value| (!value.trim().is_empty()).then_some(value))?,
        idempotency_key: row.get(5)?,
        state,
        trigger: row.get(7)?,
        next_attempt_at: row.get(8)?,
        attempt_count,
        last_outcome: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        submitted_at: row.get(13)?,
        verified_at: row.get(14)?,
    })
}

fn normalized_identifier(value: &str, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(format!("{} must be a non-empty bounded identifier", label));
    }
    Ok(value.to_string())
}

/// Reset-credit IDs are opaque upstream capability identifiers.  Never trim
/// or otherwise canonicalize them: accepting a lookalike value would make the
/// durable ledger and the documented App Server request disagree about which
/// upstream credit is being coordinated.
fn opaque_identifier(value: &str, label: &str) -> Result<String, String> {
    if value.trim() != value {
        return Err(format!(
            "{} must not contain leading or trailing whitespace",
            label
        ));
    }
    normalized_identifier(value, label)
}

fn bool_to_sql(value: bool) -> i64 {
    i64::from(value)
}

fn minutes_to_seconds(minutes: u64) -> Result<i64, String> {
    let seconds = minutes
        .checked_mul(60)
        .ok_or("Codex reset-credit duration overflow")?;
    i64::try_from(seconds)
        .map_err(|_| "Codex reset-credit duration exceeds SQLite range".to_string())
}

fn seconds_to_minutes(seconds: i64) -> rusqlite::Result<u64> {
    if seconds < 0 || seconds % 60 != 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid reset-credit policy duration",
            )),
        ));
    }
    Ok((seconds / 60) as u64)
}

fn add_minutes(now: DateTime<Utc>, minutes: u64) -> Result<DateTime<Utc>, String> {
    let seconds = minutes_to_seconds(minutes)?;
    now.checked_add_signed(ChronoDuration::seconds(seconds))
        .ok_or("Codex reset-credit timestamp overflow".to_string())
}

fn add_std(now: DateTime<Utc>, duration: Duration) -> Result<DateTime<Utc>, String> {
    let duration = ChronoDuration::from_std(duration)
        .map_err(|_| "Codex reset-credit lease duration overflow".to_string())?;
    now.checked_add_signed(duration)
        .ok_or("Codex reset-credit timestamp overflow".to_string())
}

fn stamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_stamp(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| "invalid persisted Codex reset-credit timestamp".to_string())
}

fn sql(error: rusqlite::Error) -> String {
    format!("Codex reset-credit policy database: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{Arc, Barrier, Mutex},
    };
    use uuid::Uuid;

    /// Deterministic stand-in for the narrow upstream boundary.  This makes
    /// the coordinator tests prove which calls would have been made without
    /// involving a credential, network, or real ChatGPT reset credit.
    struct FakeResetCreditUpstream {
        fresh_states:
            Mutex<VecDeque<Result<crate::target::codex::quota::FreshRateLimitResetState, String>>>,
        consume_results: Mutex<
            VecDeque<Result<crate::target::codex::quota::ConsumeRateLimitResetResult, String>>,
        >,
        consume_delay: std::time::Duration,
        consumptions: Mutex<Vec<(String, String, String)>>,
        verified_accounts: Mutex<Vec<String>>,
    }

    impl FakeResetCreditUpstream {
        fn new(
            fresh_states: impl IntoIterator<
                Item = Result<crate::target::codex::quota::FreshRateLimitResetState, String>,
            >,
        ) -> Self {
            Self::with_consume_results(
                fresh_states,
                [Ok(
                    crate::target::codex::quota::ConsumeRateLimitResetResult {
                        outcome: "reset".to_string(),
                    },
                )],
            )
        }

        fn with_consume_results(
            fresh_states: impl IntoIterator<
                Item = Result<crate::target::codex::quota::FreshRateLimitResetState, String>,
            >,
            consume_results: impl IntoIterator<
                Item = Result<crate::target::codex::quota::ConsumeRateLimitResetResult, String>,
            >,
        ) -> Self {
            Self {
                fresh_states: Mutex::new(fresh_states.into_iter().collect()),
                consume_results: Mutex::new(consume_results.into_iter().collect()),
                consume_delay: std::time::Duration::ZERO,
                consumptions: Mutex::new(Vec::new()),
                verified_accounts: Mutex::new(Vec::new()),
            }
        }

        fn with_consume_delay(mut self, delay: std::time::Duration) -> Self {
            self.consume_delay = delay;
            self
        }

        fn consumptions(&self) -> Vec<(String, String, String)> {
            self.consumptions.lock().unwrap().clone()
        }

        fn verified_accounts(&self) -> Vec<String> {
            self.verified_accounts.lock().unwrap().clone()
        }
    }

    impl ResetCreditUpstream for FakeResetCreditUpstream {
        fn fresh_state<'a>(
            &'a self,
            _account_key: &'a str,
        ) -> BoxFuture<'a, Result<crate::target::codex::quota::FreshRateLimitResetState, String>>
        {
            let result = self
                .fresh_states
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("unexpected fresh-state read".to_string()));
            Box::pin(async move { result })
        }

        fn consume<'a>(
            &'a self,
            account_key: &'a str,
            credit_id: &'a str,
            idempotency_key: &'a str,
        ) -> BoxFuture<'a, Result<crate::target::codex::quota::ConsumeRateLimitResetResult, String>>
        {
            self.consumptions.lock().unwrap().push((
                account_key.to_string(),
                credit_id.to_string(),
                idempotency_key.to_string(),
            ));
            let result = self
                .consume_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("unexpected consume".to_string()));
            let delay = self.consume_delay;
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                result
            })
        }

        fn on_verified(&self, account_key: &str) {
            self.verified_accounts
                .lock()
                .unwrap()
                .push(account_key.to_string());
        }
    }

    struct AdvancingTestClock {
        now: Arc<Mutex<DateTime<Utc>>>,
    }

    impl ResetCreditClock for AdvancingTestClock {
        fn now(&self) -> DateTime<Utc> {
            *self.now.lock().unwrap()
        }
    }

    /// Advances its injected wall clock only after returning a fresh state.
    /// This models a slow local App Server child without sleeping in the test.
    struct ExpiringFreshReadUpstream {
        clock: Arc<Mutex<DateTime<Utc>>>,
        fresh: crate::target::codex::quota::FreshRateLimitResetState,
        consumptions: Mutex<usize>,
    }

    impl ResetCreditUpstream for ExpiringFreshReadUpstream {
        fn fresh_state<'a>(
            &'a self,
            _account_key: &'a str,
        ) -> BoxFuture<'a, Result<crate::target::codex::quota::FreshRateLimitResetState, String>>
        {
            let fresh = self.fresh.clone();
            let clock = Arc::clone(&self.clock);
            Box::pin(async move {
                let mut now = clock.lock().unwrap();
                *now = *now + ChronoDuration::seconds(2);
                Ok(fresh)
            })
        }

        fn consume<'a>(
            &'a self,
            _account_key: &'a str,
            _credit_id: &'a str,
            _idempotency_key: &'a str,
        ) -> BoxFuture<'a, Result<crate::target::codex::quota::ConsumeRateLimitResetResult, String>>
        {
            *self.consumptions.lock().unwrap() += 1;
            Box::pin(async {
                Ok(crate::target::codex::quota::ConsumeRateLimitResetResult {
                    outcome: "reset".to_string(),
                })
            })
        }

        fn on_verified(&self, _account_key: &str) {}
    }

    /// Records scheduler order while making an expiry-less submitted action
    /// consume logical wall-clock time.  The urgent action expires during
    /// that slow manual read if the scheduler ever lets the unknown-expiry
    /// work run first.
    struct DeadlineFairnessUpstream {
        clock: Arc<Mutex<DateTime<Utc>>>,
        urgent_account: String,
        scanner_account: String,
        urgent_credit_id: String,
        urgent_expires_at: DateTime<Utc>,
        fresh_calls: Mutex<Vec<String>>,
        urgent_reads: Mutex<usize>,
        consumptions: Mutex<usize>,
    }

    impl DeadlineFairnessUpstream {
        fn fresh_calls(&self) -> Vec<String> {
            self.fresh_calls.lock().unwrap().clone()
        }

        fn consumptions(&self) -> usize {
            *self.consumptions.lock().unwrap()
        }
    }

    impl ResetCreditUpstream for DeadlineFairnessUpstream {
        fn fresh_state<'a>(
            &'a self,
            account_key: &'a str,
        ) -> BoxFuture<'a, Result<crate::target::codex::quota::FreshRateLimitResetState, String>>
        {
            self.fresh_calls
                .lock()
                .unwrap()
                .push(account_key.to_string());
            let result = if account_key == self.urgent_account {
                let read_number = {
                    let mut reads = self.urgent_reads.lock().unwrap();
                    *reads += 1;
                    *reads
                };
                if read_number == 1 {
                    Ok(crate::target::codex::quota::FreshRateLimitResetState {
                        credits: Some(vec![available_credit(
                            &self.urgent_credit_id,
                            self.urgent_expires_at,
                        )]),
                        available_credit_count: 1,
                        rate_limit_reached: true,
                        rate_limit_cleared: false,
                        natural_reset_after_seconds: Some(20 * 60),
                    })
                } else {
                    Ok(fresh_cleared_state())
                }
            } else if account_key == self.scanner_account {
                Ok(fresh_cleared_state())
            } else {
                // A submitted manual action has no known expiry. Model its
                // slow App Server read without sleeping. If this gets to run
                // before the urgent action, the latter becomes expired before
                // it can be claimed.
                *self.clock.lock().unwrap() += ChronoDuration::seconds(2);
                Err("synthetic slow expiry-less manual read".to_string())
            };
            Box::pin(async move { result })
        }

        fn consume<'a>(
            &'a self,
            _account_key: &'a str,
            _credit_id: &'a str,
            _idempotency_key: &'a str,
        ) -> BoxFuture<'a, Result<crate::target::codex::quota::ConsumeRateLimitResetResult, String>>
        {
            *self.consumptions.lock().unwrap() += 1;
            Box::pin(async {
                Ok(crate::target::codex::quota::ConsumeRateLimitResetResult {
                    outcome: "reset".to_string(),
                })
            })
        }

        fn on_verified(&self, _account_key: &str) {}
    }

    struct TestStorage {
        directory: PathBuf,
        path: PathBuf,
    }

    impl Drop for TestStorage {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn storage() -> TestStorage {
        let directory = std::env::temp_dir().join(format!(
            "io-gateway-codex-reset-credit-tests-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("api-key-policy.sqlite3");
        {
            let connection = crate::api_key_policy_store::open_connection_at_path(&path).unwrap();
            initialize(&connection).unwrap();
        }
        TestStorage { directory, path }
    }

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn policy(account_key: &str, enabled: bool) -> CodexResetCreditPolicyUpdate {
        CodexResetCreditPolicyUpdate {
            account_key: account_key.to_string(),
            enabled,
            scan_interval_minutes: 30,
            expiry_window_minutes: 60,
            final_attempt_minutes: 5,
            min_natural_reset_remaining_minutes: 10,
        }
    }

    fn automatic_candidate(
        account_key: &str,
        credit_id: &str,
        expires_at: DateTime<Utc>,
    ) -> ResetCreditActionCandidate {
        ResetCreditActionCandidate {
            account_key: account_key.to_string(),
            credit_id: credit_id.to_string(),
            reset_type: "codexRateLimits".to_string(),
            credit_expires_at: Some(expires_at),
            trigger: ResetCreditActionTrigger::Automatic,
        }
    }

    fn available_credit(
        credit_id: &str,
        expires_at: DateTime<Utc>,
    ) -> crate::target::codex::quota::RateLimitResetCredit {
        crate::target::codex::quota::RateLimitResetCredit {
            id: credit_id.to_string(),
            reset_type: "codexRateLimits".to_string(),
            status: "available".to_string(),
            granted_at: "2026-09-20T00:00:00Z".to_string(),
            expires_at: Some(stamp(expires_at)),
            title: None,
            description: None,
        }
    }

    fn fresh_reached_state(
        credit_id: &str,
        expires_at: DateTime<Utc>,
        natural_reset_after_seconds: Option<u64>,
    ) -> crate::target::codex::quota::FreshRateLimitResetState {
        crate::target::codex::quota::FreshRateLimitResetState {
            credits: Some(vec![available_credit(credit_id, expires_at)]),
            available_credit_count: 1,
            rate_limit_reached: true,
            rate_limit_cleared: false,
            natural_reset_after_seconds,
        }
    }

    fn fresh_cleared_state() -> crate::target::codex::quota::FreshRateLimitResetState {
        crate::target::codex::quota::FreshRateLimitResetState {
            credits: Some(Vec::new()),
            available_credit_count: 0,
            rate_limit_reached: false,
            rate_limit_cleared: true,
            natural_reset_after_seconds: None,
        }
    }

    fn consume_result(outcome: &str) -> crate::target::codex::quota::ConsumeRateLimitResetResult {
        crate::target::codex::quota::ConsumeRateLimitResetResult {
            outcome: outcome.to_string(),
        }
    }

    fn only_action(path: &Path) -> CodexResetCreditAction {
        let connection = crate::api_key_policy_store::open_connection_at_path(path).unwrap();
        let action_id: String = connection
            .query_row("SELECT id FROM codex_reset_credit_actions", [], |row| {
                row.get(0)
            })
            .unwrap();
        action_at_path(path, &action_id).unwrap().unwrap()
    }

    fn action_count(path: &Path) -> i64 {
        let connection = crate::api_key_policy_store::open_connection_at_path(path).unwrap();
        connection
            .query_row(
                "SELECT COUNT(*) FROM codex_reset_credit_actions",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn reset_credit_schema_and_policy_scan_leases_survive_reopen() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";

        {
            let connection =
                crate::api_key_policy_store::open_connection_at_path(&storage.path).unwrap();
            let initialized: i64 = connection
                .query_row(
                    "SELECT codex_reset_credits_initialized FROM managed_registry_metadata WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(initialized, 1);
            let tables: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'codex_reset_credit_%'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(tables, TABLES.len() as i64);
        }

        let saved = upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        assert!(saved.enabled);
        assert_eq!(saved.scan_interval_minutes, DEFAULT_SCAN_INTERVAL_MINUTES);
        assert_eq!(saved.expiry_window_minutes, DEFAULT_EXPIRY_WINDOW_MINUTES);
        assert_eq!(saved.next_scan_at.as_deref(), Some(stamp(now).as_str()));

        let claimed = claim_due_policy_scans_at_path(&storage.path, "worker-a", now).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].account_key, account);
        assert!(
            claim_due_policy_scans_at_path(&storage.path, "worker-b", now)
                .unwrap()
                .is_empty()
        );

        finish_policy_scan_at_path(&storage.path, account, "worker-a", now).unwrap();
        assert!(claim_due_policy_scans_at_path(
            &storage.path,
            "worker-c",
            now + ChronoDuration::minutes(29),
        )
        .unwrap()
        .is_empty());
        let claimed_again = claim_due_policy_scans_at_path(
            &storage.path,
            "worker-c",
            now + ChronoDuration::minutes(30),
        )
        .unwrap();
        assert_eq!(claimed_again.len(), 1);

        let reopened = policy_at_path(&storage.path, account).unwrap().unwrap();
        assert!(reopened.enabled);
        assert_eq!(reopened.scan_interval_minutes, 30);

        let disabled_account = "codex:account_id:disabled";
        let disabled =
            upsert_policy_at_path(&storage.path, &policy(disabled_account, false), now).unwrap();
        assert!(!disabled.enabled);
        assert!(disabled.next_scan_at.is_none());
        assert!(
            claim_due_policy_scans_at_path(&storage.path, "worker-d", now)
                .unwrap()
                .iter()
                .all(|item| item.account_key != disabled_account)
        );
    }

    #[test]
    fn policy_validation_and_schema_loss_fail_closed() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");

        let mut invalid = policy("codex:account_id:bad", true);
        invalid.final_attempt_minutes = invalid.expiry_window_minutes;
        assert!(upsert_policy_at_path(&storage.path, &invalid, now)
            .unwrap_err()
            .contains("final-attempt"));

        let mut zero_final_attempt = policy("codex:account_id:zero-final-attempt", true);
        zero_final_attempt.final_attempt_minutes = 0;
        assert!(
            upsert_policy_at_path(&storage.path, &zero_final_attempt, now)
                .unwrap_err()
                .contains("at least 1")
        );

        let mut too_soon = policy("codex:account_id:too-soon", true);
        too_soon.expiry_window_minutes = MIN_AUTOMATIC_EXPIRY_LEAD_MINUTES - 1;
        assert!(upsert_policy_at_path(&storage.path, &too_soon, now)
            .unwrap_err()
            .contains("expiry window"));

        let connection =
            crate::api_key_policy_store::open_connection_at_path(&storage.path).unwrap();
        connection
            .execute_batch("DROP TABLE codex_reset_credit_attempts")
            .unwrap();
        let error = initialize(&connection).unwrap_err();
        assert!(error.contains("missing"));
    }

    #[test]
    fn a_removed_stable_account_can_still_have_automation_disabled() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let removed = "codex:account_id:removed-account";
        assert!(validate_automation_policy_account_key(removed).is_ok());
        assert!(validate_automation_policy_account_key("manual-1").is_err());
        assert!(validate_automation_policy_account_key("codex:account_id:").is_err());

        let saved = upsert_policy_at_path(&storage.path, &policy(removed, false), now).unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.account_key, removed);
    }

    #[test]
    fn stale_policy_scan_cannot_register_after_an_operator_edit_or_disable() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:operator-edit";
        let original = upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        let claimed = claim_due_policy_scans_at_path(&storage.path, "scan-worker", now).unwrap();
        assert_eq!(claimed, vec![original.clone()]);

        // An edit clears the old scan lease and schedules the new policy
        // immediately. The old App Server response must not register a credit
        // under its superseded terms.
        let mut edited = policy(account, true);
        edited.min_natural_reset_remaining_minutes = 11;
        upsert_policy_at_path(&storage.path, &edited, now + ChronoDuration::seconds(1)).unwrap();
        let candidate = automatic_candidate(
            account,
            "credit-stale-policy",
            now + ChronoDuration::minutes(45),
        );
        assert!(register_automatic_action_for_claimed_policy_at_path(
            &storage.path,
            &original,
            "scan-worker",
            &candidate,
            now + ChronoDuration::seconds(2),
        )
        .unwrap()
        .is_none());
        assert_eq!(action_count(&storage.path), 0);

        // The stale owner finishing afterwards must not overwrite the fresh
        // immediate deadline or resurrect the previous policy.
        finish_policy_scan_at_path(
            &storage.path,
            account,
            "scan-worker",
            now + ChronoDuration::seconds(3),
        )
        .unwrap();
        let current = policy_at_path(&storage.path, account).unwrap().unwrap();
        assert!(current.enabled);
        assert_eq!(current.min_natural_reset_remaining_minutes, 11);
        assert_eq!(
            current.next_scan_at.as_deref(),
            Some(stamp(now + ChronoDuration::seconds(1)).as_str())
        );

        let current_claim = claim_due_policy_scans_at_path(
            &storage.path,
            "new-scan-worker",
            now + ChronoDuration::seconds(3),
        )
        .unwrap()
        .pop()
        .unwrap();
        let disabled = policy(account, false);
        upsert_policy_at_path(&storage.path, &disabled, now + ChronoDuration::seconds(4)).unwrap();
        assert!(register_automatic_action_for_claimed_policy_at_path(
            &storage.path,
            &current_claim,
            "new-scan-worker",
            &candidate,
            now + ChronoDuration::seconds(5),
        )
        .unwrap()
        .is_none());
        assert_eq!(action_count(&storage.path), 0);
    }

    #[test]
    fn action_registration_is_idempotent_and_manual_credit_can_have_no_expiry() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let expires = now + ChronoDuration::minutes(45);
        let candidate = automatic_candidate(account, "credit-near", expires);

        let first = register_action_at_path(&storage.path, &candidate, now).unwrap();
        let first_expiry = stamp(expires);
        assert_eq!(first.state, ResetCreditActionState::Pending);
        assert_eq!(
            first.credit_expires_at.as_deref(),
            Some(first_expiry.as_str())
        );
        assert!(Uuid::parse_str(&first.idempotency_key).is_ok());

        let updated_expiry_at = expires + ChronoDuration::minutes(2);
        let second = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, "credit-near", updated_expiry_at),
            now + ChronoDuration::minutes(1),
        )
        .unwrap();
        let updated_expiry = stamp(updated_expiry_at);
        assert_eq!(second.id, first.id);
        assert_eq!(second.idempotency_key, first.idempotency_key);
        assert_eq!(
            second.credit_expires_at.as_deref(),
            Some(updated_expiry.as_str())
        );

        let manual = register_action_at_path(
            &storage.path,
            &ResetCreditActionCandidate {
                account_key: "codex:account_id:manual-account".to_string(),
                credit_id: "manual-credit".to_string(),
                reset_type: "codexRateLimits".to_string(),
                credit_expires_at: None,
                trigger: ResetCreditActionTrigger::Manual,
            },
            now,
        )
        .unwrap();
        assert_eq!(manual.credit_expires_at, None);
        assert!(matches!(
            claim_action_at_path(
                &storage.path,
                &manual.id,
                "manual-worker",
                now + ChronoDuration::days(365),
                false,
            )
            .unwrap(),
            ResetCreditActionClaim::Acquired(action) if action.id == manual.id
        ));

        let reopened = action_at_path(&storage.path, &first.id).unwrap().unwrap();
        assert_eq!(reopened.id, first.id);
        assert_eq!(reopened.idempotency_key, first.idempotency_key);
    }

    #[test]
    fn action_json_never_serializes_the_durable_idempotency_key() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                "codex:account_id:account-one",
                "credit-redaction",
                now + ChronoDuration::minutes(45),
            ),
            now,
        )
        .unwrap();
        let serialized = serde_json::to_value(&action).unwrap();
        assert!(serialized.get("idempotency_key").is_none());
        assert!(!serialized
            .to_string()
            .contains(action.idempotency_key.as_str()));
    }

    #[test]
    fn competing_claims_and_submitted_restart_preserve_one_idempotency_key() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                "codex:account_id:account-one",
                "credit-near",
                now + ChronoDuration::hours(1),
            ),
            now,
        )
        .unwrap();
        let path = Arc::new(storage.path.clone());
        let barrier = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for worker in ["worker-a", "worker-b"] {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let action_id = action.id.clone();
            let claim_now = now.clone();
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                match claim_action_at_path(&path, &action_id, worker, claim_now, false).unwrap() {
                    ResetCreditActionClaim::Acquired(_) => Some(worker.to_string()),
                    ResetCreditActionClaim::Busy => None,
                    other => panic!("unexpected claim result: {other:?}"),
                }
            }));
        }
        barrier.wait();
        let winners = joins
            .into_iter()
            .filter_map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1);

        let winner = &winners[0];
        mark_action_submitted_at_path(&storage.path, &action.id, winner, now).unwrap();
        let submitted = action_at_path(&storage.path, &action.id).unwrap().unwrap();
        assert_eq!(submitted.state, ResetCreditActionState::Submitted);
        assert_eq!(submitted.idempotency_key, action.idempotency_key);
        assert!(submitted.submitted_at.is_some());

        let restart_at = now + ChronoDuration::seconds(ACTION_LEASE.as_secs() as i64 + 1);
        let recovered = match claim_action_at_path(
            &storage.path,
            &action.id,
            "worker-after-restart",
            restart_at,
            false,
        )
        .unwrap()
        {
            ResetCreditActionClaim::Acquired(action) => action,
            other => panic!("submitted action was not recovered: {other:?}"),
        };
        assert_eq!(recovered.id, action.id);
        assert_eq!(recovered.idempotency_key, action.idempotency_key);
        assert_eq!(recovered.state, ResetCreditActionState::Submitted);

        mark_action_verified_at_path(
            &storage.path,
            &action.id,
            "worker-after-restart",
            restart_at,
        )
        .unwrap();
        assert!(matches!(
            claim_action_at_path(
                &storage.path,
                &action.id,
                "later-worker",
                restart_at + ChronoDuration::seconds(1),
                false,
            )
            .unwrap(),
            ResetCreditActionClaim::Terminal(ref action)
                if action.state == ResetCreditActionState::Verified
        ));
    }

    #[test]
    fn expired_pending_and_submitted_actions_have_safe_distinct_terminal_states() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let pending = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                "codex:account_id:pending",
                "expired-pending",
                now - ChronoDuration::seconds(1),
            ),
            now,
        )
        .unwrap();
        assert!(matches!(
            claim_action_at_path(&storage.path, &pending.id, "worker", now, false).unwrap(),
            ResetCreditActionClaim::Terminal(ref action)
                if action.state == ResetCreditActionState::Expired
        ));

        let submitted = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                "codex:account_id:submitted",
                "expired-submitted",
                now + ChronoDuration::seconds(30),
            ),
            now,
        )
        .unwrap();
        assert!(matches!(
            claim_action_at_path(&storage.path, &submitted.id, "worker", now, false).unwrap(),
            ResetCreditActionClaim::Acquired(_)
        ));
        mark_action_submitted_at_path(&storage.path, &submitted.id, "worker", now).unwrap();
        assert!(matches!(
            claim_action_at_path(
                &storage.path,
                &submitted.id,
                "restart-worker",
                now + ChronoDuration::seconds(31),
                false,
            )
            .unwrap(),
            ResetCreditActionClaim::Terminal(ref action)
                if action.state == ResetCreditActionState::ManualReview
        ));
    }

    #[test]
    fn automatic_candidate_accepts_only_the_configured_30_to_window_boundary() {
        let now = at("2026-09-21T00:00:00Z");
        let update = policy("codex:account_id:account-one", true);
        let policy = CodexResetCreditPolicy {
            account_key: update.account_key,
            enabled: update.enabled,
            scan_interval_minutes: update.scan_interval_minutes,
            expiry_window_minutes: update.expiry_window_minutes,
            final_attempt_minutes: update.final_attempt_minutes,
            min_natural_reset_remaining_minutes: update.min_natural_reset_remaining_minutes,
            next_scan_at: Some(stamp(now)),
            updated_at: stamp(now),
        };

        for (name, expires_at, expected) in [
            (
                "one-second-too-soon",
                now + ChronoDuration::minutes(30) - ChronoDuration::seconds(1),
                false,
            ),
            (
                "at-minimum-lead",
                now + ChronoDuration::minutes(MIN_AUTOMATIC_EXPIRY_LEAD_MINUTES as i64),
                true,
            ),
            ("at-window", now + ChronoDuration::minutes(60), true),
            (
                "one-second-outside",
                now + ChronoDuration::minutes(60) + ChronoDuration::seconds(1),
                false,
            ),
            ("at-now", now, false),
            ("past", now - ChronoDuration::seconds(1), false),
        ] {
            let fresh = fresh_reached_state(name, expires_at, Some(20 * 60));
            assert_eq!(
                select_automatic_candidate(&policy, &fresh, now).is_some(),
                expected,
                "{name} had the wrong automatic-candidate eligibility"
            );
        }
    }

    #[tokio::test]
    async fn unavailable_or_unidentified_credits_never_register_or_post() {
        let now = at("2026-09-21T00:00:00Z");
        let expires_at = now + ChronoDuration::minutes(45);
        let account = "codex:account_id:account-one";

        let states = [
            crate::target::codex::quota::FreshRateLimitResetState {
                // A stale detail row must not override the authoritative
                // available-count value of zero.
                credits: Some(vec![available_credit("stale-detail", expires_at)]),
                available_credit_count: 0,
                rate_limit_reached: true,
                rate_limit_cleared: false,
                natural_reset_after_seconds: Some(20 * 60),
            },
            crate::target::codex::quota::FreshRateLimitResetState {
                // A count without a concrete ID is equally unsafe to spend.
                credits: None,
                available_credit_count: 0,
                rate_limit_reached: true,
                rate_limit_cleared: false,
                natural_reset_after_seconds: Some(20 * 60),
            },
        ];

        for fresh in states {
            let storage = storage();
            upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
            let upstream = FakeResetCreditUpstream::new([Ok(fresh)]);
            let summary = run_once_at_path_with(&storage.path, &upstream, "worker", now)
                .await
                .unwrap();
            assert_eq!(summary.claimed_policy_scans, 1);
            assert_eq!(summary.registered_actions, 0);
            assert_eq!(summary.processed_actions, 0);
            assert!(upstream.consumptions().is_empty());

            let connection =
                crate::api_key_policy_store::open_connection_at_path(&storage.path).unwrap();
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM codex_reset_credit_actions",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[tokio::test]
    async fn automatic_never_posts_when_the_limit_is_not_reached() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let credit_id = "credit-limit-not-reached";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();

        let not_reached = crate::target::codex::quota::FreshRateLimitResetState {
            credits: Some(vec![available_credit(credit_id, expires_at)]),
            available_credit_count: 1,
            rate_limit_reached: false,
            rate_limit_cleared: true,
            natural_reset_after_seconds: Some(20 * 60),
        };
        let upstream = FakeResetCreditUpstream::new([Ok(not_reached.clone()), Ok(not_reached)]);
        let summary = run_once_at_path_with(&storage.path, &upstream, "worker", now)
            .await
            .unwrap();
        assert_eq!(summary.registered_actions, 1);
        assert_eq!(summary.processed_actions, 1);
        assert!(upstream.consumptions().is_empty());
        let action = only_action(&storage.path);
        assert_eq!(action.state, ResetCreditActionState::Deferred);
        assert_eq!(
            action.last_outcome.as_deref(),
            Some("deferred_no_eligible_limit")
        );
        assert!(action.submitted_at.is_none());
    }

    #[tokio::test]
    async fn automatic_obeys_the_natural_reset_threshold_exactly() {
        let now = at("2026-09-21T00:00:00Z");
        let expires_at = now + ChronoDuration::minutes(45);
        let account = "codex:account_id:account-one";

        for (natural_reset_after_seconds, should_consume) in
            [(10 * 60, false), (10 * 60 - 1, false), (10 * 60 + 1, true)]
        {
            let storage = storage();
            let credit_id = format!("credit-natural-reset-{natural_reset_after_seconds}");
            upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
            let reached =
                fresh_reached_state(&credit_id, expires_at, Some(natural_reset_after_seconds));
            let fresh_states = if should_consume {
                vec![Ok(reached.clone()), Ok(reached), Ok(fresh_cleared_state())]
            } else {
                vec![Ok(reached.clone()), Ok(reached)]
            };
            let upstream = FakeResetCreditUpstream::new(fresh_states);

            run_once_at_path_with(&storage.path, &upstream, "worker", now)
                .await
                .unwrap();
            assert_eq!(
                upstream.consumptions().len(),
                usize::from(should_consume),
                "natural reset {natural_reset_after_seconds}s had the wrong POST behavior"
            );
            let action = only_action(&storage.path);
            if should_consume {
                assert_eq!(action.state, ResetCreditActionState::Verified);
            } else {
                assert_eq!(action.state, ResetCreditActionState::Deferred);
                assert_eq!(
                    action.last_outcome.as_deref(),
                    Some("deferred_natural_reset_soon")
                );
                assert!(action.submitted_at.is_none());
            }
        }
    }

    #[tokio::test]
    async fn timeout_retry_reuses_the_exact_credit_and_idempotency_key() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let credit_id = "credit-timeout-retry";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, credit_id, expires_at),
            now,
        )
        .unwrap();
        let reached = fresh_reached_state(credit_id, expires_at, Some(20 * 60));
        let upstream = FakeResetCreditUpstream::with_consume_results(
            [Ok(reached.clone()), Ok(reached), Ok(fresh_cleared_state())],
            [
                Err("dummy transport timeout".to_string()),
                Ok(consume_result("reset")),
            ],
        );

        let first = process_action_at_path_with(
            &storage.path,
            &upstream,
            &action.id,
            "worker-first",
            now,
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.state, ResetCreditActionState::Submitted);
        assert_eq!(first.last_outcome.as_deref(), Some("retry_scheduled"));
        assert_eq!(first.idempotency_key, action.idempotency_key);

        let retried = process_action_at_path_with(
            &storage.path,
            &upstream,
            &action.id,
            "worker-retry",
            now + ChronoDuration::seconds(1),
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(retried.state, ResetCreditActionState::Verified);
        assert_eq!(retried.idempotency_key, action.idempotency_key);
        let consumptions = upstream.consumptions();
        assert_eq!(consumptions.len(), 2);
        assert!(consumptions
            .iter()
            .all(|(_, used_credit, idempotency_key)| {
                used_credit == credit_id && idempotency_key == &action.idempotency_key
            }));
        assert_eq!(upstream.verified_accounts(), vec![account.to_string()]);
    }

    #[tokio::test]
    async fn accepted_redemption_with_a_still_reached_post_read_requires_manual_review() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let credit_id = "credit-post-read-still-reached";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, credit_id, expires_at),
            now,
        )
        .unwrap();
        let reached = fresh_reached_state(credit_id, expires_at, Some(20 * 60));
        let upstream = FakeResetCreditUpstream::new([Ok(reached.clone()), Ok(reached)]);

        let completed =
            process_action_at_path_with(&storage.path, &upstream, &action.id, "worker", now, true)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(completed.state, ResetCreditActionState::ManualReview);
        assert_eq!(
            completed.last_outcome.as_deref(),
            Some("verification_failed")
        );
        assert_eq!(upstream.consumptions().len(), 1);
        assert!(upstream.verified_accounts().is_empty());
    }

    #[tokio::test]
    async fn already_redeemed_is_verified_without_spending_another_credit() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let credit_id = "credit-already-redeemed";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, credit_id, expires_at),
            now,
        )
        .unwrap();
        let reached = fresh_reached_state(credit_id, expires_at, Some(20 * 60));
        let upstream = FakeResetCreditUpstream::with_consume_results(
            [Ok(reached), Ok(fresh_cleared_state())],
            [Ok(consume_result("already_redeemed"))],
        );

        let completed =
            process_action_at_path_with(&storage.path, &upstream, &action.id, "worker", now, true)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(completed.state, ResetCreditActionState::Verified);
        assert_eq!(upstream.consumptions().len(), 1);
        assert_eq!(upstream.verified_accounts(), vec![account.to_string()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_processors_can_send_only_one_redemption_post() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let credit_id = "credit-concurrent";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, credit_id, expires_at),
            now,
        )
        .unwrap();
        let reached = fresh_reached_state(credit_id, expires_at, Some(20 * 60));
        let upstream = Arc::new(
            FakeResetCreditUpstream::new([Ok(reached), Ok(fresh_cleared_state())])
                .with_consume_delay(std::time::Duration::from_millis(100)),
        );

        let first_path = storage.path.clone();
        let first_action_id = action.id.clone();
        let first_upstream = Arc::clone(&upstream);
        let first = tokio::spawn(async move {
            process_action_at_path_with(
                &first_path,
                first_upstream.as_ref(),
                &first_action_id,
                "worker-first",
                now,
                true,
            )
            .await
        });

        // The fake records the intended POST before awaiting its dummy delay.
        // Waiting for that signal makes the competing claim deterministic and
        // proves it observes the durable action lease, rather than merely
        // racing after the first action has already finished.
        for _ in 0..100 {
            if !upstream.consumptions().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            upstream.consumptions().len(),
            1,
            "first worker never reached POST"
        );

        let second = process_action_at_path_with(
            &storage.path,
            upstream.as_ref(),
            &action.id,
            "worker-second",
            now,
            true,
        )
        .await
        .unwrap();
        assert!(
            second.is_none(),
            "competing worker unexpectedly acquired the action"
        );

        let first = first.await.unwrap().unwrap().unwrap();
        assert_eq!(first.state, ResetCreditActionState::Verified);
        assert_eq!(upstream.consumptions().len(), 1);
        assert_eq!(upstream.verified_accounts(), vec![account.to_string()]);
    }

    #[tokio::test]
    async fn automatic_redeems_once_when_the_reached_limit_has_a_safe_natural_reset_time() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let credit_id = "credit-near-expiry";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();

        // The policy's ten-minute threshold permits an action with twenty
        // minutes until the natural reset.  The first read selects the
        // credit, the second authorizes its POST, and the third proves that
        // the reached limit really cleared after the dummy redemption.
        let reached = fresh_reached_state(credit_id, expires_at, Some(20 * 60));
        let upstream = FakeResetCreditUpstream::new([
            Ok(reached.clone()),
            Ok(reached),
            Ok(fresh_cleared_state()),
        ]);

        let summary = run_once_at_path_with(&storage.path, &upstream, "worker", now)
            .await
            .unwrap();
        assert_eq!(summary.claimed_policy_scans, 1);
        assert_eq!(summary.registered_actions, 1);
        assert_eq!(summary.processed_actions, 1);
        assert_eq!(summary.upstream_failures, 0);

        let consumptions = upstream.consumptions();
        assert_eq!(consumptions.len(), 1);
        assert_eq!(consumptions[0].0, account);
        assert_eq!(consumptions[0].1, credit_id);
        assert!(Uuid::parse_str(&consumptions[0].2).is_ok());
        assert_eq!(upstream.verified_accounts(), vec![account.to_string()]);

        let action = only_action(&storage.path);
        assert_eq!(action.state, ResetCreditActionState::Verified);
        assert_eq!(action.last_outcome.as_deref(), Some("verified"));
        assert!(action.submitted_at.is_some());
        assert!(action.verified_at.is_some());
    }

    #[tokio::test]
    async fn automatic_never_posts_when_a_reached_limit_has_no_readable_natural_reset_time() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let credit_id = "credit-no-reset-time";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();

        // A reached limit alone is not sufficient to authorize upstream
        // spending.  The coordinator must retain the credit and wait for a
        // future fresh response with an explicit natural reset time.
        let reached_without_reset_time = fresh_reached_state(credit_id, expires_at, None);
        let upstream = FakeResetCreditUpstream::new([
            Ok(reached_without_reset_time.clone()),
            Ok(reached_without_reset_time),
        ]);

        let summary = run_once_at_path_with(&storage.path, &upstream, "worker", now)
            .await
            .unwrap();
        assert_eq!(summary.claimed_policy_scans, 1);
        assert_eq!(summary.registered_actions, 1);
        assert_eq!(summary.processed_actions, 1);
        assert_eq!(summary.upstream_failures, 0);
        assert!(
            upstream.consumptions().is_empty(),
            "unsafe state sent a POST"
        );
        assert!(upstream.verified_accounts().is_empty());

        let action = only_action(&storage.path);
        assert_eq!(action.state, ResetCreditActionState::Deferred);
        assert_eq!(
            action.last_outcome.as_deref(),
            Some("deferred_natural_reset_unknown")
        );
        assert_eq!(
            parse_stamp(&action.next_attempt_at).unwrap(),
            now + ChronoDuration::minutes(30)
        );
        assert!(action.submitted_at.is_none());
    }

    #[tokio::test]
    async fn submitted_recovery_never_posts_after_the_natural_reset_becomes_too_near() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:submitted-recovery";
        let credit_id = "credit-submitted-before-crash";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, credit_id, expires_at),
            now,
        )
        .unwrap();

        // Model a process dying immediately after its durable pre-POST marker
        // but before its App Server request. A restart may only retry if the
        // fresh state still passes every automatic no-spend guard.
        assert!(matches!(
            claim_action_at_path(&storage.path, &action.id, "crashed-worker", now, false).unwrap(),
            ResetCreditActionClaim::Acquired(_)
        ));
        mark_action_submitted_at_path(&storage.path, &action.id, "crashed-worker", now).unwrap();
        let restart = now + ChronoDuration::seconds(ACTION_LEASE.as_secs() as i64 + 1);
        let upstream = FakeResetCreditUpstream::new([Ok(fresh_reached_state(
            credit_id,
            expires_at,
            Some(5 * 60),
        ))]);

        let settled = process_action_at_path_with(
            &storage.path,
            &upstream,
            &action.id,
            "recovery-worker",
            restart,
            false,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(settled.state, ResetCreditActionState::ManualReview);
        assert!(upstream.consumptions().is_empty());
    }

    #[test]
    fn one_account_gate_blocks_a_second_automatic_credit() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let first = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, "credit-first", now + ChronoDuration::minutes(45)),
            now,
        )
        .unwrap();
        let second = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, "credit-second", now + ChronoDuration::minutes(30)),
            now,
        )
        .unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.credit_id, "credit-first");

        let connection =
            crate::api_key_policy_store::open_connection_at_path(&storage.path).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM codex_reset_credit_actions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn dormant_expiryless_manual_action_yields_only_to_one_automatic_action() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:manual-and-automatic";
        let manual = ResetCreditActionCandidate {
            account_key: account.to_string(),
            credit_id: "manual-without-expiry".to_string(),
            reset_type: "codexRateLimits".to_string(),
            credit_expires_at: None,
            trigger: ResetCreditActionTrigger::Manual,
        };
        let manual_action = register_action_at_path(&storage.path, &manual, now).unwrap();
        assert!(matches!(
            claim_action_at_path(
                &storage.path,
                &manual_action.id,
                "manual-worker",
                now,
                false
            )
            .unwrap(),
            ResetCreditActionClaim::Acquired(_)
        ));
        defer_action_at_path(
            &storage.path,
            &manual_action.id,
            "manual-worker",
            DeferredActionOutcome::NoEligibleLimit,
            now + ChronoDuration::minutes(30),
            now,
        )
        .unwrap();

        // The dormant manual action does not cause an actually expiring
        // automatic credit to be lost.
        let automatic = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                account,
                "automatic-expiring-credit",
                now + ChronoDuration::minutes(45),
            ),
            now + ChronoDuration::seconds(1),
        )
        .unwrap();
        assert_ne!(automatic.id, manual_action.id);
        assert_eq!(
            automatic.trigger,
            ResetCreditActionTrigger::Automatic.as_db()
        );

        // But a second manual action and reactivation of the original manual
        // action are both rejected while the automatic account gate exists.
        let other_manual = ResetCreditActionCandidate {
            credit_id: "other-manual-without-expiry".to_string(),
            ..manual.clone()
        };
        assert!(register_action_at_path(
            &storage.path,
            &other_manual,
            now + ChronoDuration::seconds(2),
        )
        .is_err());
        assert!(
            register_action_at_path(&storage.path, &manual, now + ChronoDuration::seconds(2),)
                .is_err()
        );

        // The durable claim check closes the last interleaving: after the
        // automatic action is registered, a stale manual retry cannot acquire
        // a lease and reach an App Server consume.
        assert!(matches!(
            claim_action_at_path(
                &storage.path,
                &manual_action.id,
                "manual-retry-worker",
                now + ChronoDuration::minutes(31),
                true,
            )
            .unwrap(),
            ResetCreditActionClaim::Busy
        ));
        assert_eq!(action_count(&storage.path), 2);
    }

    #[test]
    fn ambiguous_manual_review_blocks_later_automatic_selection() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                account,
                "credit-ambiguous",
                now + ChronoDuration::minutes(45),
            ),
            now,
        )
        .unwrap();
        assert!(matches!(
            claim_action_at_path(&storage.path, &action.id, "worker", now, false).unwrap(),
            ResetCreditActionClaim::Acquired(_)
        ));
        mark_action_manual_review_at_path(&storage.path, &action.id, "worker", now).unwrap();

        let blocked = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, "credit-newer", now + ChronoDuration::minutes(30)),
            now,
        )
        .unwrap();
        assert_eq!(blocked.id, action.id);
        assert_eq!(blocked.state, ResetCreditActionState::ManualReview);
    }

    #[test]
    fn v1_submitted_wham_action_migrates_to_manual_review() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let submitted = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                "codex:account_id:legacy-submitted",
                "legacy-submitted-credit",
                now + ChronoDuration::minutes(45),
            ),
            now,
        )
        .unwrap();
        assert!(matches!(
            claim_action_at_path(&storage.path, &submitted.id, "worker", now, false).unwrap(),
            ResetCreditActionClaim::Acquired(_)
        ));
        mark_action_submitted_at_path(&storage.path, &submitted.id, "worker", now).unwrap();

        let pending = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                "codex:account_id:legacy-pending",
                "legacy-pending-credit",
                now + ChronoDuration::minutes(45),
            ),
            now,
        )
        .unwrap();
        let connection =
            crate::api_key_policy_store::open_connection_at_path(&storage.path).unwrap();
        connection
            .execute(
                "UPDATE codex_reset_credit_actions SET transport='wham_legacy'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE codex_reset_credit_metadata SET version=1 WHERE singleton=1",
                [],
            )
            .unwrap();

        initialize(&connection).unwrap();
        let migrated = action_at_path(&storage.path, &submitted.id)
            .unwrap()
            .unwrap();
        assert_eq!(migrated.state, ResetCreditActionState::ManualReview);
        assert_eq!(
            migrated.last_outcome.as_deref(),
            Some("legacy_transport_manual_review")
        );
        let pending = action_at_path(&storage.path, &pending.id).unwrap().unwrap();
        assert_eq!(pending.state, ResetCreditActionState::Pending);
        let transport: String = connection
            .query_row(
                "SELECT transport FROM codex_reset_credit_actions WHERE id=?1",
                [&pending.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transport, "app_server");
    }

    #[test]
    fn only_exact_documented_credit_enums_are_automatic_eligible() {
        let now = at("2026-09-21T00:00:00Z");
        let mut credit = available_credit("credit-enum", now + ChronoDuration::minutes(45));
        assert!(credit_is_available_codex_reset(&credit));
        for reset_type in [
            "codex_rate_limits",
            "CodexRateLimits",
            "codexRateLimits!",
            " codexRateLimits",
            "codexRateLimits ",
        ] {
            credit.reset_type = reset_type.to_string();
            assert!(!credit_is_available_codex_reset(&credit), "{reset_type}");
        }
        credit.reset_type = "codexRateLimits".to_string();
        for status in [
            "Available",
            "available!",
            "spent",
            " available",
            "available ",
        ] {
            credit.status = status.to_string();
            assert!(!credit_is_available_codex_reset(&credit), "{status}");
        }
        credit.status = "available".to_string();
        for id in [" credit-enum", "credit-enum ", "\ncredit-enum"] {
            credit.id = id.to_string();
            assert!(!credit_is_available_codex_reset(&credit), "{id:?}");
        }
    }

    #[test]
    fn opaque_credit_identifiers_are_never_whitespace_normalized() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:opaque-credit-id";
        for credit_id in [" credit", "credit ", "\ncredit"] {
            let candidate = ResetCreditActionCandidate {
                account_key: account.to_string(),
                credit_id: credit_id.to_string(),
                reset_type: "codexRateLimits".to_string(),
                credit_expires_at: Some(now + ChronoDuration::minutes(45)),
                trigger: ResetCreditActionTrigger::Manual,
            };
            assert!(register_action_at_path(&storage.path, &candidate, now).is_err());
        }

        let fresh = fresh_reached_state(
            "exact-credit",
            now + ChronoDuration::minutes(45),
            Some(20 * 60),
        );
        assert!(select_manual_candidate(account, " exact-credit", &fresh, now).is_err());
        assert!(select_manual_candidate(account, "exact-credit ", &fresh, now).is_err());
        assert!(select_manual_candidate(account, "exact-credit", &fresh, now).is_ok());
    }

    #[tokio::test]
    async fn disabling_after_an_ambiguous_submission_keeps_the_account_gate() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:account-one";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(account, "credit-disable-after-submit", expires_at),
            now,
        )
        .unwrap();
        assert!(matches!(
            claim_action_at_path(&storage.path, &action.id, "first-worker", now, false).unwrap(),
            ResetCreditActionClaim::Acquired(_)
        ));
        mark_action_submitted_at_path(&storage.path, &action.id, "first-worker", now).unwrap();
        upsert_policy_at_path(
            &storage.path,
            &policy(account, false),
            now + ChronoDuration::seconds(1),
        )
        .unwrap();

        let restart = now + ChronoDuration::seconds(ACTION_LEASE.as_secs() as i64 + 1);
        let upstream = FakeResetCreditUpstream::new([]);
        let deferred = process_action_at_path_with(
            &storage.path,
            &upstream,
            &action.id,
            "restart-worker",
            restart,
            false,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(deferred.state, ResetCreditActionState::Submitted);
        assert!(upstream.consumptions().is_empty());

        assert!(matches!(
            claim_action_at_path(
                &storage.path,
                &action.id,
                "expiry-worker",
                expires_at + ChronoDuration::seconds(1),
                false,
            )
            .unwrap(),
            ResetCreditActionClaim::Terminal(ref action)
                if action.state == ResetCreditActionState::ManualReview
        ));
        let blocked = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                account,
                "credit-replacement",
                expires_at + ChronoDuration::minutes(30),
            ),
            expires_at + ChronoDuration::seconds(1),
        )
        .unwrap();
        assert_eq!(blocked.state, ResetCreditActionState::ManualReview);
    }

    #[tokio::test]
    async fn due_actions_are_reconciled_before_policy_scans() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let action_account = "codex:account_id:action-account";
        let expires_at = now + ChronoDuration::minutes(45);
        upsert_policy_at_path(&storage.path, &policy(action_account, true), now).unwrap();
        upsert_policy_at_path(
            &storage.path,
            &policy("codex:account_id:slow-scan-one", true),
            now,
        )
        .unwrap();
        upsert_policy_at_path(
            &storage.path,
            &policy("codex:account_id:slow-scan-two", true),
            now,
        )
        .unwrap();
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(action_account, "credit-priority", expires_at),
            now,
        )
        .unwrap();
        let reached = fresh_reached_state("credit-priority", expires_at, Some(20 * 60));
        let upstream = FakeResetCreditUpstream::new([
            // The pre-existing due action must consume these first two reads.
            Ok(reached),
            Ok(fresh_cleared_state()),
            // The three policy scans run after the action was reconciled.
            Ok(fresh_cleared_state()),
            Ok(fresh_cleared_state()),
            Ok(fresh_cleared_state()),
        ]);

        let summary = run_once_at_path_with(&storage.path, &upstream, "worker", now)
            .await
            .unwrap();
        assert_eq!(summary.processed_actions, 1);
        assert_eq!(upstream.consumptions().len(), 1);
        assert_eq!(
            action_at_path(&storage.path, &action.id)
                .unwrap()
                .unwrap()
                .state,
            ResetCreditActionState::Verified
        );
    }

    #[tokio::test]
    async fn scheduler_prioritizes_real_deadlines_and_alternates_with_policy_scans() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let urgent_account = "codex:account_id:urgent-deadline";
        let scanner_account = "codex:account_id:ordinary-scan";
        let manual_account = "codex:account_id:manual-no-expiry";
        let urgent_credit = "urgent-credit";
        let urgent_expires_at = now + ChronoDuration::seconds(1);

        // The urgent action needs an enabled policy but that policy's normal
        // scan is not due. Set it up before adding the ordinary policy that
        // should receive the turn between the two due actions.
        upsert_policy_at_path(&storage.path, &policy(urgent_account, true), now).unwrap();
        let setup_claim = claim_due_policy_scans_at_path(&storage.path, "setup", now)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(setup_claim.account_key, urgent_account);
        finish_policy_scan_at_path(&storage.path, urgent_account, "setup", now).unwrap();

        let urgent_action = register_action_at_path(
            &storage.path,
            &automatic_candidate(urgent_account, urgent_credit, urgent_expires_at),
            now,
        )
        .unwrap();

        // A submitted manual request is allowed to have no upstream expiry.
        // It is due now and deliberately takes two logical seconds to read.
        // SQLite's historical empty-string ordering would have run it first.
        let manual_action = register_action_at_path(
            &storage.path,
            &ResetCreditActionCandidate {
                account_key: manual_account.to_string(),
                credit_id: "manual-credit-without-expiry".to_string(),
                reset_type: "codexRateLimits".to_string(),
                credit_expires_at: None,
                trigger: ResetCreditActionTrigger::Manual,
            },
            now,
        )
        .unwrap();
        assert!(matches!(
            claim_action_at_path(&storage.path, &manual_action.id, "setup-manual", now, false)
                .unwrap(),
            ResetCreditActionClaim::Acquired(_)
        ));
        mark_action_submitted_at_path(&storage.path, &manual_action.id, "setup-manual", now)
            .unwrap();
        reschedule_submitted_action_at_path(
            &storage.path,
            &manual_action.id,
            "setup-manual",
            now,
            now,
        )
        .unwrap();

        // This is the policy scan that must run between the urgent action and
        // the expiry-less manual reconciliation. It creates no action.
        upsert_policy_at_path(&storage.path, &policy(scanner_account, true), now).unwrap();

        let shared_now = Arc::new(Mutex::new(now));
        let clock = AdvancingTestClock {
            now: Arc::clone(&shared_now),
        };
        let upstream = DeadlineFairnessUpstream {
            clock: shared_now,
            urgent_account: urgent_account.to_string(),
            scanner_account: scanner_account.to_string(),
            urgent_credit_id: urgent_credit.to_string(),
            urgent_expires_at,
            fresh_calls: Mutex::new(Vec::new()),
            urgent_reads: Mutex::new(0),
            consumptions: Mutex::new(0),
        };

        let summary = run_once_at_path_with_clock(&storage.path, &upstream, "worker", &clock)
            .await
            .unwrap();

        // A real expiry is selected first, an ordinary policy gets the next
        // turn, then the unbounded submitted action runs. This fails if we
        // either restore SQLite's empty-expiry-first order or drain all
        // actions before policies.
        assert_eq!(
            upstream.fresh_calls(),
            vec![
                urgent_account.to_string(),
                urgent_account.to_string(),
                scanner_account.to_string(),
                manual_account.to_string(),
            ]
        );
        assert_eq!(summary.claimed_policy_scans, 1);
        assert_eq!(summary.processed_actions, 2);
        assert_eq!(upstream.consumptions(), 1);
        assert_eq!(
            action_at_path(&storage.path, &urgent_action.id)
                .unwrap()
                .unwrap()
                .state,
            ResetCreditActionState::Verified,
            "the slow expiry-less manual read must not make the real deadline expire"
        );
        assert_eq!(
            action_at_path(&storage.path, &manual_action.id)
                .unwrap()
                .unwrap()
                .state,
            ResetCreditActionState::Submitted
        );
    }

    #[tokio::test]
    async fn slow_fresh_read_cannot_consume_a_credit_that_expired_mid_action() {
        let storage = storage();
        let now = at("2026-09-21T00:00:00Z");
        let account = "codex:account_id:clock-account";
        upsert_policy_at_path(&storage.path, &policy(account, true), now).unwrap();
        let action = register_action_at_path(
            &storage.path,
            &automatic_candidate(
                account,
                "credit-expiring-during-read",
                now + ChronoDuration::seconds(1),
            ),
            now,
        )
        .unwrap();
        let shared_now = Arc::new(Mutex::new(now));
        let clock = AdvancingTestClock {
            now: Arc::clone(&shared_now),
        };
        let upstream = ExpiringFreshReadUpstream {
            clock: shared_now,
            fresh: fresh_reached_state(
                "credit-expiring-during-read",
                now + ChronoDuration::seconds(1),
                Some(20 * 60),
            ),
            consumptions: Mutex::new(0),
        };

        let completed = process_action_at_path_with_clock(
            &storage.path,
            &upstream,
            &action.id,
            "worker",
            &clock,
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(completed.state, ResetCreditActionState::Expired);
        assert_eq!(*upstream.consumptions.lock().unwrap(), 0);
    }
}
