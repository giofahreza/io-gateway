//! Durable, privacy-preserving policy state for managed API keys.
//!
//! This store deliberately keeps request policy/audit information separate
//! from account usage history.  A request can be refused before an account is
//! selected, so it cannot be represented faithfully by `usage_history`.
//! Likewise, a cumulative budget has to be checked and reserved before a
//! request is dispatched; an asynchronously-written usage aggregate cannot
//! provide that guarantee.
//!
//! The schema contains only identifiers and bounded routing/policy metadata.
//! It has no column for request bodies, prompts, tool calls, headers, or
//! upstream error bodies.  Callers must use the typed event enums below rather
//! than attempting to persist free-form diagnostic text.

use crate::api_keys::{ApiKeyBudgetPeriod, ApiKeyInputTokenBudget, MAX_PERSISTED_INPUT_TOKENS};
use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const POLICY_DB_FILE: &str = "api-key-policy.sqlite3";
const POLICY_DB_SCHEMA_VERSION: i64 = 1;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
// First-use initialization includes a WAL mode transition and schema DDL.
// SQLite can return SQLITE_BUSY immediately for that transition even when a
// normal connection busy timeout is configured, so retry a short, bounded
// initialization phase before failing closed.
const SQLITE_INITIALIZATION_BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const SQLITE_INITIALIZATION_RETRY_ATTEMPTS: usize = 8;
const SQLITE_INITIALIZATION_RETRY_BASE_DELAY: Duration = Duration::from_millis(25);

// These are deliberately small.  The fields are operational identifiers, not
// a place to save user input.  Rejecting overlong / control-character-bearing
// values also protects the dashboard from accidentally rendering untrusted
// request content as audit metadata.
const MAX_REQUEST_ID_LEN: usize = 256;
const MAX_API_KEY_ID_LEN: usize = 256;
const MAX_REQUEST_PATH_LEN: usize = 512;
const MAX_IDENTIFIER_LEN: usize = 256;
const MAX_EVENT_QUERY_LIMIT: usize = 1_000;
const MAX_SWEEP_BATCH_SIZE: usize = 1_000;

/// Stage at which a policy event was recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiKeyPolicyEventKind {
    Authorization,
    Dispatch,
    Settlement,
}

impl ApiKeyPolicyEventKind {
    fn as_db(self) -> &'static str {
        match self {
            Self::Authorization => "authorization",
            Self::Dispatch => "dispatch",
            Self::Settlement => "settlement",
        }
    }

    fn from_db(value: &str) -> Result<Self, String> {
        match value {
            "authorization" => Ok(Self::Authorization),
            "dispatch" => Ok(Self::Dispatch),
            "settlement" => Ok(Self::Settlement),
            _ => Err(format!("invalid API-key policy event kind '{}'", value)),
        }
    }
}

/// A safe, stable policy result to retain in the per-key audit trail.
///
/// Do not add a catch-all string variant here: free-form errors are often
/// derived from upstream payloads and can contain user prompts or tool output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiKeyPolicyDecision {
    Allowed,
    ScopeDenied,
    RequestLimitExceeded,
    BudgetExceeded,
    MeasurementRequired,
    NoEligibleAccount,
    UpstreamRateLimited,
    UpstreamUnavailable,
    Completed,
    Failed,
    Cancelled,
}

impl ApiKeyPolicyDecision {
    fn as_db(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::ScopeDenied => "scope_denied",
            Self::RequestLimitExceeded => "request_limit_exceeded",
            Self::BudgetExceeded => "budget_exceeded",
            Self::MeasurementRequired => "measurement_required",
            Self::NoEligibleAccount => "no_eligible_account",
            Self::UpstreamRateLimited => "upstream_rate_limited",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_db(value: &str) -> Result<Self, String> {
        match value {
            "allowed" => Ok(Self::Allowed),
            "scope_denied" => Ok(Self::ScopeDenied),
            "request_limit_exceeded" => Ok(Self::RequestLimitExceeded),
            "budget_exceeded" => Ok(Self::BudgetExceeded),
            "measurement_required" => Ok(Self::MeasurementRequired),
            "no_eligible_account" => Ok(Self::NoEligibleAccount),
            "upstream_rate_limited" => Ok(Self::UpstreamRateLimited),
            "upstream_unavailable" => Ok(Self::UpstreamUnavailable),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid API-key policy decision '{}'", value)),
        }
    }
}

/// How the gateway derived the input units recorded for a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiKeyInputMeasurement {
    Exact,
    Estimated,
    Conservative,
    Unknown,
}

impl ApiKeyInputMeasurement {
    fn as_db(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Estimated => "estimated",
            Self::Conservative => "conservative",
            Self::Unknown => "unknown",
        }
    }

    fn from_db(value: &str) -> Result<Self, String> {
        match value {
            "exact" => Ok(Self::Exact),
            "estimated" => Ok(Self::Estimated),
            "conservative" => Ok(Self::Conservative),
            "unknown" => Ok(Self::Unknown),
            _ => Err(format!("invalid API-key input measurement '{}'", value)),
        }
    }
}

/// Safe metadata for one API-key policy event.
///
/// There is intentionally no prompt, message, tool, schema, media URL, raw
/// request, raw response, or free-form error field. `provider` and
/// `account_key` are gateway-derived routing identifiers only. A client model
/// string is deliberately not retained: model IDs are request-controlled and
/// can carry arbitrary sensitive text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ApiKeyRequestAuditEvent {
    pub request_id: String,
    pub api_key_id: String,
    pub kind: ApiKeyPolicyEventKind,
    pub decision: ApiKeyPolicyDecision,
    pub request_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement: Option<ApiKeyInputMeasurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_id: Option<String>,
}

/// An audit event after the store has assigned it an ID and timestamp.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredApiKeyRequestAuditEvent {
    pub id: i64,
    pub occurred_at: String,
    #[serde(flatten)]
    pub event: ApiKeyRequestAuditEvent,
}

/// Query for the most recent safe request-policy audit records.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ApiKeyRequestAuditQuery {
    pub api_key_id: Option<String>,
    pub request_id: Option<String>,
    pub limit: Option<usize>,
}

/// Identifies a pre-dispatch hold against a whole-key input-token budget.
///
/// `reservation_id` must be stable if the caller retries the same reservation.
/// A UUID generated before dispatch is a good choice.  It is deliberately
/// separate from `request_id`: one client request may need multiple gateway
/// policy/audit events while consuming exactly one budget hold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApiKeyInputTokenReservationRequest {
    pub reservation_id: String,
    pub request_id: String,
    pub api_key_id: String,
    pub budget: ApiKeyInputTokenBudget,
    /// A conservative pre-dispatch amount.  It must be greater than zero.
    pub reserved_input_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiKeyInputTokenReservationState {
    Active,
    Committed,
    Released,
}

impl ApiKeyInputTokenReservationState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Committed => "committed",
            Self::Released => "released",
        }
    }

    fn from_db(value: &str) -> Result<Self, String> {
        match value {
            "active" => Ok(Self::Active),
            "committed" => Ok(Self::Committed),
            "released" => Ok(Self::Released),
            _ => Err(format!(
                "invalid API-key budget reservation state '{}'",
                value
            )),
        }
    }
}

/// A durable reservation, including its fixed accounting window.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ApiKeyInputTokenReservation {
    pub reservation_id: String,
    pub request_id: String,
    pub api_key_id: String,
    pub budget_limit: u64,
    pub period: ApiKeyBudgetPeriod,
    /// `lifetime` or an UTC `YYYY-MM` calendar-month window.
    pub window_start: String,
    pub reserved_input_tokens: u64,
    /// The amount actually committed; `None` while the reservation is active
    /// or released.
    pub committed_input_tokens: Option<u64>,
    pub state: ApiKeyInputTokenReservationState,
    pub created_at: String,
    pub settled_at: Option<String>,
}

/// Successful and denied results are deliberately separate.  A denied result
/// is normal policy flow, not a storage failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ApiKeyInputTokenReservationResult {
    Reserved {
        reservation: ApiKeyInputTokenReservation,
        /// False when a retry found the same still-active reservation.
        created: bool,
    },
    Denied(ApiKeyInputTokenBudgetDenied),
}

/// Enough information to render a useful `429` and dashboard summary without
/// exposing request content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ApiKeyInputTokenBudgetDenied {
    pub api_key_id: String,
    pub budget_limit: u64,
    pub period: ApiKeyBudgetPeriod,
    pub window_start: String,
    pub committed_input_tokens: u64,
    pub reserved_input_tokens: u64,
    pub requested_input_tokens: u64,
}

/// Explicitly determines how an active hold is settled.
///
/// `Commit { actual_input_tokens: None }` intentionally commits the original
/// reservation.  Use it for streaming aborts, ambiguous network failures, or
/// any outcome where the gateway cannot prove the provider did not bill the
/// request.  `Release` is only safe when dispatch definitely did not reach an
/// upstream provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApiKeyInputTokenSettlement {
    Commit { actual_input_tokens: Option<u64> },
    Release,
}

/// Result of settling a hold.  `settled_now` is false for an idempotent repeat
/// of the same final settlement kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApiKeyInputTokenSettlementResult {
    pub reservation: ApiKeyInputTokenReservation,
    pub settled_now: bool,
}

/// Current totals for the active window of a configured budget.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ApiKeyInputTokenBudgetSummary {
    pub api_key_id: String,
    pub budget_limit: u64,
    pub period: ApiKeyBudgetPeriod,
    pub window_start: String,
    pub committed_input_tokens: u64,
    pub reserved_input_tokens: u64,
    pub available_input_tokens: u64,
}

/// Result of conservatively settling abandoned active holds.
///
/// Every reservation in `committed` was active when the sweeper settled it
/// and is now charged for its full originally-reserved amount.  A concurrent
/// request completion can settle a candidate first; those cases are counted
/// in `already_settled` and are never changed by the sweeper.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ApiKeyInputTokenReservationSweepResult {
    pub examined: usize,
    pub committed: Vec<ApiKeyInputTokenReservation>,
    pub already_settled: usize,
}

/// Returns the location of the dedicated security/accounting database.
pub(crate) fn policy_db_path(cfg: &crate::Config) -> PathBuf {
    cfg.auth_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(POLICY_DB_FILE)
}

/// Creates a consistent, private backup of the key registry and both quota
/// ledgers, including committed WAL contents. The destination MUST NOT exist.
/// Provider credentials/configuration are deliberately outside this snapshot.
/// Restoring an older snapshot requires reconciling usage since its creation.
pub(crate) fn backup(cfg: &crate::Config, destination: &Path) -> Result<(), String> {
    {
        let _registry_lock = crate::api_keys::lock_store_exclusive(cfg)?;
        crate::api_keys::load(cfg)?;
    }
    // The potentially large snapshot needs only SQLite's read transaction;
    // do not hold the registry file lock and block authentication throughout.
    let connection = open_connection_at_path(&policy_db_path(cfg))?;
    let database_id = existing_database_id(&connection)?
        .ok_or("established API-key database identity is missing")?;
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(destination).map_err(|err| {
        format!(
            "backup requires a new destination directory '{}': {err}",
            destination.display()
        )
    })?;
    // Failure leaves a visibly incomplete, private snapshot for inspection;
    // do not overwrite or recursively remove anything supplied by an operator.
    let result = (|| -> Result<(), String> {
        let snapshot = destination.join(POLICY_DB_FILE);
        let snapshot_name = snapshot
            .to_str()
            .ok_or("backup destination is not valid UTF-8")?;
        private_file(&snapshot, true)?;
        connection
            .execute("VACUUM INTO ?1", [snapshot_name])
            .map_err(|err| format!("failed to snapshot API-key SQLite/WAL state: {err}"))?;
        let snapshot_connection =
            Connection::open_with_flags(&snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|err| format!("failed to verify API-key backup: {err}"))?;
        let integrity: String = snapshot_connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|err| format!("failed to check API-key backup integrity: {err}"))?;
        if integrity != "ok"
            || existing_database_id(&snapshot_connection)?.as_deref() != Some(database_id.as_str())
        {
            return Err("API-key backup integrity or identity verification failed".to_string());
        }
        drop(snapshot_connection);
        private_file(&snapshot, false)?
            .sync_all()
            .map_err(|err| format!("failed to synchronize API-key backup: {err}"))?;
        persist_identity(
            &identity_path(&snapshot),
            &PolicyDatabaseIdentity {
                version: 1,
                database_id: database_id.clone(),
                initializing: false,
                database_existed: true,
            },
        )?;
        crate::api_keys::write_migration_sentinel(
            &destination.join("api-keys.json"),
            &database_id,
        )?;
        let manifest = serde_json::json!({
            "format": "io-gateway-api-key-policy-backup-v1",
            "database_id": database_id,
            "created_at": Utc::now().to_rfc3339(),
            "includes": ["managed_key_hashes_and_rules", "quota_balances_and_reservations", "policy_audit", "codex_reset_credit_policies_and_actions"],
            "excludes": ["provider_credentials", "gateway_configuration", "reporting_history"],
            "restore_requires_usage_reconciliation": true
        });
        crate::target::atomic_write(
            &destination.join("backup-manifest.json"),
            &serde_json::to_vec_pretty(&manifest).map_err(|err| err.to_string())?,
            true,
        )
        .map_err(|err| format!("failed to complete API-key backup manifest: {err}"))?;
        Ok(())
    })();
    result.map_err(|err| {
        format!(
            "{err}; incomplete private backup retained at '{}'",
            destination.display()
        )
    })
}

/// Appends one privacy-preserving per-key policy event.
pub(crate) fn append_event(
    cfg: &crate::Config,
    event: &ApiKeyRequestAuditEvent,
) -> Result<StoredApiKeyRequestAuditEvent, String> {
    append_event_at_path(&policy_db_path(cfg), event, Utc::now())
}

/// Lists the most recent audit events.  This is intentionally read-only and
/// excludes any raw usage/request payload by schema design.
pub(crate) fn list_events(
    cfg: &crate::Config,
    query: &ApiKeyRequestAuditQuery,
) -> Result<Vec<StoredApiKeyRequestAuditEvent>, String> {
    list_events_at_path(&policy_db_path(cfg), query)
}

/// Atomically reserves conservative input units before dispatching a request.
///
/// The implementation uses `BEGIN IMMEDIATE`, so competing writers cannot
/// both observe the same available capacity.  Callers should append an audit
/// event for both `Reserved` and `Denied` outcomes.
pub(crate) fn reserve_input_tokens(
    cfg: &crate::Config,
    request: &ApiKeyInputTokenReservationRequest,
) -> Result<ApiKeyInputTokenReservationResult, String> {
    reserve_input_tokens_at_path(&policy_db_path(cfg), request, Utc::now())
}

/// Settles an active reservation after dispatch.
pub(crate) fn settle_input_token_reservation(
    cfg: &crate::Config,
    reservation_id: &str,
    settlement: ApiKeyInputTokenSettlement,
) -> Result<ApiKeyInputTokenSettlementResult, String> {
    settle_input_token_reservation_at_path(
        &policy_db_path(cfg),
        reservation_id,
        settlement,
        Utc::now(),
    )
}

/// Returns the current accounting totals for a key's selected budget window.
pub(crate) fn input_token_budget_summary(
    cfg: &crate::Config,
    api_key_id: &str,
    budget: &ApiKeyInputTokenBudget,
) -> Result<ApiKeyInputTokenBudgetSummary, String> {
    input_token_budget_summary_at_path(&policy_db_path(cfg), api_key_id, budget, Utc::now())
}

/// Conservatively settles stale active reservations in bounded batches.
///
/// This is deliberately a *commit*, never a release: after a process crash or
/// a lost completion signal the gateway cannot prove an upstream request was
/// not billed.  Pick `max_age` longer than the longest legitimate request or
/// stream, then invoke this periodically from a single maintenance worker.
/// The returned reservations let that worker append corresponding safe audit
/// records without reconstructing request bodies.
pub(crate) fn sweep_stale_input_token_reservations(
    cfg: &crate::Config,
    max_age: Duration,
    batch_size: usize,
) -> Result<ApiKeyInputTokenReservationSweepResult, String> {
    sweep_stale_input_token_reservations_at_path(
        &policy_db_path(cfg),
        max_age,
        batch_size,
        Utc::now(),
    )
}

fn append_event_at_path(
    path: &Path,
    event: &ApiKeyRequestAuditEvent,
    now: DateTime<Utc>,
) -> Result<StoredApiKeyRequestAuditEvent, String> {
    let event = normalize_event(event)?;
    let occurred_at = timestamp(now);
    let mut connection = open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| format!("failed to start API-key audit transaction: {}", err))?;
    transaction
        .execute(
            "INSERT INTO api_key_request_events (
                occurred_at, request_id, api_key_id, event_kind, decision,
                request_path, provider, model, account_key, status_code,
                estimated_input_tokens, actual_input_tokens, measurement_method,
                reservation_id
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
             )",
            params![
                &occurred_at,
                &event.request_id,
                &event.api_key_id,
                event.kind.as_db(),
                event.decision.as_db(),
                &event.request_path,
                &event.provider,
                &event.model,
                &event.account_key,
                event.status_code.map(i64::from),
                event.estimated_input_tokens.map(sqlite_u64).transpose()?,
                event.actual_input_tokens.map(sqlite_u64).transpose()?,
                event.measurement.map(ApiKeyInputMeasurement::as_db),
                &event.reservation_id,
            ],
        )
        .map_err(|err| format!("failed to append API-key audit event: {}", err))?;
    let id = transaction.last_insert_rowid();
    transaction
        .commit()
        .map_err(|err| format!("failed to commit API-key audit event: {}", err))?;
    Ok(StoredApiKeyRequestAuditEvent {
        id,
        occurred_at,
        event,
    })
}

fn list_events_at_path(
    path: &Path,
    query: &ApiKeyRequestAuditQuery,
) -> Result<Vec<StoredApiKeyRequestAuditEvent>, String> {
    let connection = open_connection_at_path(path)?;
    let api_key_id = query
        .api_key_id
        .as_deref()
        .map(|value| normalize_required(value, "API key ID", MAX_API_KEY_ID_LEN))
        .transpose()?;
    let request_id = query
        .request_id
        .as_deref()
        .map(|value| normalize_required(value, "request ID", MAX_REQUEST_ID_LEN))
        .transpose()?;
    let limit = query.limit.unwrap_or(100).clamp(1, MAX_EVENT_QUERY_LIMIT);

    let mut sql = String::from(
        "SELECT id, occurred_at, request_id, api_key_id, event_kind, decision,
                request_path, provider, model, account_key, status_code,
                estimated_input_tokens, actual_input_tokens, measurement_method,
                reservation_id
         FROM api_key_request_events",
    );
    let mut params = Vec::<String>::new();
    let mut clauses = Vec::new();
    if let Some(value) = api_key_id.as_ref() {
        clauses.push("api_key_id = ?".to_string());
        params.push(value.clone());
    }
    if let Some(value) = request_id.as_ref() {
        clauses.push("request_id = ?".to_string());
        params.push(value.clone());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY id DESC LIMIT ?");
    params.push(limit.to_string());

    let mut statement = connection.prepare(&sql).map_err(|err| err.to_string())?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(params.iter()), row_to_event)
        .map_err(|err| err.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

fn reserve_input_tokens_at_path(
    path: &Path,
    request: &ApiKeyInputTokenReservationRequest,
    now: DateTime<Utc>,
) -> Result<ApiKeyInputTokenReservationResult, String> {
    let request = normalize_reservation_request(request)?;
    let window_start = budget_window_start(request.budget.period, now);
    let created_at = timestamp(now);
    let mut connection = open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| format!("failed to start API-key budget transaction: {}", err))?;

    if let Some(existing) = load_reservation(&transaction, &request.reservation_id)? {
        ensure_matching_retry(&existing, &request, &window_start)?;
        if existing.state != ApiKeyInputTokenReservationState::Active {
            return Err(format!(
                "API-key budget reservation '{}' has already been {}",
                existing.reservation_id,
                existing.state.as_db()
            ));
        }
        transaction
            .commit()
            .map_err(|err| format!("failed to commit API-key budget retry: {}", err))?;
        return Ok(ApiKeyInputTokenReservationResult::Reserved {
            reservation: existing,
            created: false,
        });
    }

    let (committed, reserved) = load_window_totals(
        &transaction,
        &request.api_key_id,
        request.budget.period,
        &window_start,
    )?;
    let occupied = committed
        .checked_add(reserved)
        .ok_or_else(|| "API-key budget accounting overflow".to_string())?;
    let needed = occupied
        .checked_add(request.reserved_input_tokens)
        .ok_or_else(|| "API-key budget accounting overflow".to_string())?;
    if needed > request.budget.limit {
        transaction
            .commit()
            .map_err(|err| format!("failed to commit API-key budget denial: {}", err))?;
        return Ok(ApiKeyInputTokenReservationResult::Denied(
            ApiKeyInputTokenBudgetDenied {
                api_key_id: request.api_key_id,
                budget_limit: request.budget.limit,
                period: request.budget.period,
                window_start,
                committed_input_tokens: committed,
                reserved_input_tokens: reserved,
                requested_input_tokens: request.reserved_input_tokens,
            },
        ));
    }

    let reserved_delta = sqlite_u64(request.reserved_input_tokens)?;
    transaction
        .execute(
            "INSERT INTO api_key_budget_windows (
                api_key_id, budget_period, window_start, committed_input_tokens,
                reserved_input_tokens, updated_at
             ) VALUES (?1, ?2, ?3, 0, ?4, ?5)
             ON CONFLICT(api_key_id, budget_period, window_start) DO UPDATE SET
                reserved_input_tokens = reserved_input_tokens + excluded.reserved_input_tokens,
                updated_at = excluded.updated_at",
            params![
                &request.api_key_id,
                request.budget.period.as_db(),
                &window_start,
                reserved_delta,
                &created_at,
            ],
        )
        .map_err(|err| format!("failed to reserve API-key budget: {}", err))?;
    transaction
        .execute(
            "INSERT INTO api_key_budget_reservations (
                reservation_id, request_id, api_key_id, budget_limit,
                budget_period, window_start, reserved_input_tokens,
                committed_input_tokens, state, created_at, settled_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'active', ?8, NULL)",
            params![
                &request.reservation_id,
                &request.request_id,
                &request.api_key_id,
                sqlite_u64(request.budget.limit)?,
                request.budget.period.as_db(),
                &window_start,
                reserved_delta,
                &created_at,
            ],
        )
        .map_err(|err| format!("failed to record API-key budget reservation: {}", err))?;
    transaction
        .commit()
        .map_err(|err| format!("failed to commit API-key budget reservation: {}", err))?;

    Ok(ApiKeyInputTokenReservationResult::Reserved {
        reservation: ApiKeyInputTokenReservation {
            reservation_id: request.reservation_id,
            request_id: request.request_id,
            api_key_id: request.api_key_id,
            budget_limit: request.budget.limit,
            period: request.budget.period,
            window_start,
            reserved_input_tokens: request.reserved_input_tokens,
            committed_input_tokens: None,
            state: ApiKeyInputTokenReservationState::Active,
            created_at,
            settled_at: None,
        },
        created: true,
    })
}

fn settle_input_token_reservation_at_path(
    path: &Path,
    reservation_id: &str,
    settlement: ApiKeyInputTokenSettlement,
    now: DateTime<Utc>,
) -> Result<ApiKeyInputTokenSettlementResult, String> {
    settle_input_token_reservation_at_path_with_terminal_tolerance(
        path,
        reservation_id,
        settlement,
        now,
        false,
    )
}

/// The stale-reservation sweeper intentionally tolerates a terminal state
/// winning a race after it selected an active ID. Ordinary callers retain
/// strict mismatched-settlement detection so a programming error cannot turn a
/// released request into a silently ignored commit.
fn settle_input_token_reservation_at_path_with_terminal_tolerance(
    path: &Path,
    reservation_id: &str,
    settlement: ApiKeyInputTokenSettlement,
    now: DateTime<Utc>,
    tolerate_terminal_state: bool,
) -> Result<ApiKeyInputTokenSettlementResult, String> {
    let reservation_id = normalize_required(reservation_id, "reservation ID", MAX_REQUEST_ID_LEN)?;
    let mut connection = open_connection_at_path(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| format!("failed to start API-key budget settlement: {}", err))?;
    let existing = load_reservation(&transaction, &reservation_id)?.ok_or_else(|| {
        format!(
            "API-key budget reservation '{}' was not found",
            reservation_id
        )
    })?;

    if existing.state != ApiKeyInputTokenReservationState::Active {
        if !tolerate_terminal_state {
            ensure_matching_settlement_retry(&existing, settlement)?;
        }
        transaction
            .commit()
            .map_err(|err| format!("failed to commit API-key settlement retry: {}", err))?;
        return Ok(ApiKeyInputTokenSettlementResult {
            reservation: existing,
            settled_now: false,
        });
    }

    let settled_at = timestamp(now);
    let (new_state, committed_input_tokens) = match settlement {
        ApiKeyInputTokenSettlement::Commit {
            actual_input_tokens,
        } => {
            let charged = actual_input_tokens
                .unwrap_or(existing.reserved_input_tokens)
                .max(existing.reserved_input_tokens);
            (ApiKeyInputTokenReservationState::Committed, Some(charged))
        }
        ApiKeyInputTokenSettlement::Release => (ApiKeyInputTokenReservationState::Released, None),
    };
    let (window_committed, window_reserved) = load_window_totals(
        &transaction,
        &existing.api_key_id,
        existing.period,
        &existing.window_start,
    )?;
    if window_reserved < existing.reserved_input_tokens {
        return Err(format!(
            "API-key budget reservation '{}' is inconsistent with its window",
            existing.reservation_id
        ));
    }
    let new_window_reserved = window_reserved - existing.reserved_input_tokens;
    let new_window_committed = match committed_input_tokens {
        Some(charged) => window_committed
            .checked_add(charged)
            .ok_or_else(|| "API-key budget accounting overflow".to_string())?,
        None => window_committed,
    };
    transaction
        .execute(
            "UPDATE api_key_budget_windows
             SET committed_input_tokens = ?1,
                 reserved_input_tokens = ?2,
                 updated_at = ?3
             WHERE api_key_id = ?4 AND budget_period = ?5 AND window_start = ?6",
            params![
                sqlite_u64(new_window_committed)?,
                sqlite_u64(new_window_reserved)?,
                &settled_at,
                &existing.api_key_id,
                existing.period.as_db(),
                &existing.window_start,
            ],
        )
        .map_err(|err| format!("failed to settle API-key budget window: {}", err))?;
    transaction
        .execute(
            "UPDATE api_key_budget_reservations
             SET committed_input_tokens = ?1, state = ?2, settled_at = ?3
             WHERE reservation_id = ?4 AND state = 'active'",
            params![
                committed_input_tokens.map(sqlite_u64).transpose()?,
                new_state.as_db(),
                &settled_at,
                &existing.reservation_id,
            ],
        )
        .map_err(|err| format!("failed to settle API-key budget reservation: {}", err))?;
    transaction
        .commit()
        .map_err(|err| format!("failed to commit API-key budget settlement: {}", err))?;

    Ok(ApiKeyInputTokenSettlementResult {
        reservation: ApiKeyInputTokenReservation {
            committed_input_tokens,
            state: new_state,
            settled_at: Some(settled_at),
            ..existing
        },
        settled_now: true,
    })
}

fn input_token_budget_summary_at_path(
    path: &Path,
    api_key_id: &str,
    budget: &ApiKeyInputTokenBudget,
    now: DateTime<Utc>,
) -> Result<ApiKeyInputTokenBudgetSummary, String> {
    let api_key_id = normalize_required(api_key_id, "API key ID", MAX_API_KEY_ID_LEN)?;
    validate_budget(budget)?;
    let window_start = budget_window_start(budget.period, now);
    let connection = open_connection_at_path(path)?;
    let (committed, reserved) =
        load_window_totals(&connection, &api_key_id, budget.period, &window_start)?;
    let occupied = committed
        .checked_add(reserved)
        .ok_or_else(|| "API-key budget accounting overflow".to_string())?;
    Ok(ApiKeyInputTokenBudgetSummary {
        api_key_id,
        budget_limit: budget.limit,
        period: budget.period,
        window_start,
        committed_input_tokens: committed,
        reserved_input_tokens: reserved,
        available_input_tokens: budget.limit.saturating_sub(occupied),
    })
}

fn sweep_stale_input_token_reservations_at_path(
    path: &Path,
    max_age: Duration,
    batch_size: usize,
    now: DateTime<Utc>,
) -> Result<ApiKeyInputTokenReservationSweepResult, String> {
    if batch_size == 0 {
        return Ok(ApiKeyInputTokenReservationSweepResult::default());
    }
    let batch_size = batch_size.min(MAX_SWEEP_BATCH_SIZE);
    let chrono_age = chrono::Duration::from_std(max_age)
        .map_err(|_| "API-key reservation sweep age is too large".to_string())?;
    let cutoff = now
        .checked_sub_signed(chrono_age)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let cutoff = timestamp(cutoff);
    let connection = open_connection_at_path(path)?;
    let reservation_ids = {
        let mut statement = connection
            .prepare(
                "SELECT reservation_id
                 FROM api_key_budget_reservations
                 WHERE state = 'active' AND created_at <= ?1
                 ORDER BY created_at ASC, reservation_id ASC
                 LIMIT ?2",
            )
            .map_err(|err| format!("failed to query stale API-key reservations: {}", err))?;
        let rows = statement
            .query_map(params![&cutoff, batch_size as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|err| format!("failed to read stale API-key reservations: {}", err))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("failed to decode stale API-key reservations: {}", err))?
    };
    drop(connection);

    let mut result = ApiKeyInputTokenReservationSweepResult {
        examined: reservation_ids.len(),
        ..Default::default()
    };
    for reservation_id in reservation_ids {
        let settlement = settle_input_token_reservation_at_path_with_terminal_tolerance(
            path,
            &reservation_id,
            ApiKeyInputTokenSettlement::Commit {
                actual_input_tokens: None,
            },
            now,
            true,
        )?;
        if settlement.settled_now {
            result.committed.push(settlement.reservation);
        } else {
            result.already_settled = result.already_settled.saturating_add(1);
        }
    }
    Ok(result)
}

trait PolicyDbConnection {
    fn policy_query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> rusqlite::Result<T>
    where
        P: rusqlite::Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>;
}

impl PolicyDbConnection for Connection {
    fn policy_query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> rusqlite::Result<T>
    where
        P: rusqlite::Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        Connection::query_row(self, sql, params, f)
    }
}

impl<'connection> PolicyDbConnection for rusqlite::Transaction<'connection> {
    fn policy_query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> rusqlite::Result<T>
    where
        P: rusqlite::Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        std::ops::Deref::deref(self).query_row(sql, params, f)
    }
}

fn load_window_totals(
    connection: &impl PolicyDbConnection,
    api_key_id: &str,
    period: ApiKeyBudgetPeriod,
    window_start: &str,
) -> Result<(u64, u64), String> {
    let row = connection
        .policy_query_row(
            "SELECT committed_input_tokens, reserved_input_tokens
             FROM api_key_budget_windows
             WHERE api_key_id = ?1 AND budget_period = ?2 AND window_start = ?3",
            params![api_key_id, period.as_db(), window_start],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    match row {
        Some((committed, reserved)) => Ok((
            sqlite_nonnegative_u64(committed)?,
            sqlite_nonnegative_u64(reserved)?,
        )),
        None => Ok((0, 0)),
    }
}

fn load_reservation(
    connection: &impl PolicyDbConnection,
    reservation_id: &str,
) -> Result<Option<ApiKeyInputTokenReservation>, String> {
    connection
        .policy_query_row(
            "SELECT reservation_id, request_id, api_key_id, budget_limit,
                    budget_period, window_start, reserved_input_tokens,
                    committed_input_tokens, state, created_at, settled_at
             FROM api_key_budget_reservations WHERE reservation_id = ?1",
            params![reservation_id],
            row_to_reservation,
        )
        .optional()
        .map_err(|err| err.to_string())
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredApiKeyRequestAuditEvent> {
    let kind = row
        .get::<_, String>(4)
        .and_then(|value| ApiKeyPolicyEventKind::from_db(&value).map_err(conversion_error))?;
    let decision = row
        .get::<_, String>(5)
        .and_then(|value| ApiKeyPolicyDecision::from_db(&value).map_err(conversion_error))?;
    let measurement = row
        .get::<_, Option<String>>(13)?
        .map(|value| ApiKeyInputMeasurement::from_db(&value).map_err(conversion_error))
        .transpose()?;
    let status_code = row
        .get::<_, Option<i64>>(10)?
        .map(|value| {
            u16::try_from(value)
                .map_err(|_| conversion_error(format!("invalid stored HTTP status {}", value)))
        })
        .transpose()?;
    let estimated_input_tokens = row
        .get::<_, Option<i64>>(11)?
        .map(|value| sqlite_nonnegative_u64(value).map_err(conversion_error))
        .transpose()?;
    let actual_input_tokens = row
        .get::<_, Option<i64>>(12)?
        .map(|value| sqlite_nonnegative_u64(value).map_err(conversion_error))
        .transpose()?;
    Ok(StoredApiKeyRequestAuditEvent {
        id: row.get(0)?,
        occurred_at: row.get(1)?,
        event: ApiKeyRequestAuditEvent {
            request_id: row.get(2)?,
            api_key_id: row.get(3)?,
            kind,
            decision,
            request_path: row.get(6)?,
            provider: row.get(7)?,
            // Old installations may contain model values from before the
            // privacy migration. Never expose them even if a database was
            // copied or manually modified with a newer schema version.
            model: None,
            account_key: row.get(9)?,
            status_code,
            estimated_input_tokens,
            actual_input_tokens,
            measurement,
            reservation_id: row.get(14)?,
        },
    })
}

fn row_to_reservation(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKeyInputTokenReservation> {
    let period = row
        .get::<_, String>(4)
        .and_then(|value| ApiKeyBudgetPeriod::from_db(&value).map_err(conversion_error))?;
    let state = row.get::<_, String>(8).and_then(|value| {
        ApiKeyInputTokenReservationState::from_db(&value).map_err(conversion_error)
    })?;
    let budget_limit = sqlite_nonnegative_u64(row.get(3)?).map_err(conversion_error)?;
    let reserved_input_tokens = sqlite_nonnegative_u64(row.get(6)?).map_err(conversion_error)?;
    let committed_input_tokens = row
        .get::<_, Option<i64>>(7)?
        .map(|value| sqlite_nonnegative_u64(value).map_err(conversion_error))
        .transpose()?;
    Ok(ApiKeyInputTokenReservation {
        reservation_id: row.get(0)?,
        request_id: row.get(1)?,
        api_key_id: row.get(2)?,
        budget_limit,
        period,
        window_start: row.get(5)?,
        reserved_input_tokens,
        committed_input_tokens,
        state,
        created_at: row.get(9)?,
        settled_at: row.get(10)?,
    })
}

impl ApiKeyBudgetPeriod {
    fn as_db(self) -> &'static str {
        match self {
            Self::Lifetime => "lifetime",
            Self::CalendarMonth => "calendar_month",
        }
    }

    fn from_db(value: &str) -> Result<Self, String> {
        match value {
            "lifetime" => Ok(Self::Lifetime),
            "calendar_month" => Ok(Self::CalendarMonth),
            _ => Err(format!("invalid API-key budget period '{}'", value)),
        }
    }
}

fn ensure_matching_retry(
    existing: &ApiKeyInputTokenReservation,
    request: &ApiKeyInputTokenReservationRequest,
    window_start: &str,
) -> Result<(), String> {
    if existing.request_id == request.request_id
        && existing.api_key_id == request.api_key_id
        && existing.budget_limit == request.budget.limit
        && existing.period == request.budget.period
        && existing.window_start == window_start
        && existing.reserved_input_tokens == request.reserved_input_tokens
    {
        Ok(())
    } else {
        Err(format!(
            "API-key budget reservation '{}' was retried with different parameters",
            request.reservation_id
        ))
    }
}

fn ensure_matching_settlement_retry(
    existing: &ApiKeyInputTokenReservation,
    settlement: ApiKeyInputTokenSettlement,
) -> Result<(), String> {
    match (existing.state, settlement) {
        (
            ApiKeyInputTokenReservationState::Committed,
            ApiKeyInputTokenSettlement::Commit { .. },
        )
        | (ApiKeyInputTokenReservationState::Released, ApiKeyInputTokenSettlement::Release) => {
            Ok(())
        }
        _ => Err(format!(
            "API-key budget reservation '{}' is already {} and cannot be settled differently",
            existing.reservation_id,
            existing.state.as_db()
        )),
    }
}

fn normalize_event(event: &ApiKeyRequestAuditEvent) -> Result<ApiKeyRequestAuditEvent, String> {
    Ok(ApiKeyRequestAuditEvent {
        request_id: normalize_required(&event.request_id, "request ID", MAX_REQUEST_ID_LEN)?,
        api_key_id: normalize_required(&event.api_key_id, "API key ID", MAX_API_KEY_ID_LEN)?,
        kind: event.kind,
        decision: event.decision,
        request_path: normalize_required(
            &event.request_path,
            "request path",
            MAX_REQUEST_PATH_LEN,
        )?,
        provider: normalize_optional(event.provider.as_deref(), "provider", MAX_IDENTIFIER_LEN)?,
        // Model IDs are supplied by API clients and are not a trusted routing
        // identifier. Keeping even a bounded string would make this audit a
        // covert prompt/secret retention channel, so discard it at the sole
        // persistence boundary.
        model: None,
        account_key: normalize_optional(
            event.account_key.as_deref(),
            "account key",
            MAX_IDENTIFIER_LEN,
        )?,
        status_code: event.status_code,
        estimated_input_tokens: event
            .estimated_input_tokens
            .map(sqlite_u64)
            .transpose()?
            .map(|value| value as u64),
        actual_input_tokens: event
            .actual_input_tokens
            .map(sqlite_u64)
            .transpose()?
            .map(|value| value as u64),
        measurement: event.measurement,
        reservation_id: normalize_optional(
            event.reservation_id.as_deref(),
            "reservation ID",
            MAX_REQUEST_ID_LEN,
        )?,
    })
}

fn normalize_reservation_request(
    request: &ApiKeyInputTokenReservationRequest,
) -> Result<ApiKeyInputTokenReservationRequest, String> {
    validate_budget(&request.budget)?;
    if request.reserved_input_tokens == 0 {
        return Err("API-key input-token reservation must be greater than zero".to_string());
    }
    sqlite_u64(request.reserved_input_tokens)?;
    Ok(ApiKeyInputTokenReservationRequest {
        reservation_id: normalize_required(
            &request.reservation_id,
            "reservation ID",
            MAX_REQUEST_ID_LEN,
        )?,
        request_id: normalize_required(&request.request_id, "request ID", MAX_REQUEST_ID_LEN)?,
        api_key_id: normalize_required(&request.api_key_id, "API key ID", MAX_API_KEY_ID_LEN)?,
        budget: request.budget.clone(),
        reserved_input_tokens: request.reserved_input_tokens,
    })
}

fn validate_budget(budget: &ApiKeyInputTokenBudget) -> Result<(), String> {
    if budget.limit == 0 {
        return Err("API-key input-token budget limit must be greater than zero".to_string());
    }
    if budget.limit > MAX_PERSISTED_INPUT_TOKENS {
        return Err(format!(
            "API-key input-token budget limit must not exceed {} (SQLite signed integer range)",
            MAX_PERSISTED_INPUT_TOKENS
        ));
    }
    sqlite_u64(budget.limit)?;
    Ok(())
}

fn normalize_required(value: &str, field: &str, max_len: usize) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{} must not be empty", field));
    }
    validate_safe_metadata(value, field, max_len)?;
    Ok(value.to_string())
}

fn normalize_optional(
    value: Option<&str>,
    field: &str,
    max_len: usize,
) -> Result<Option<String>, String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            validate_safe_metadata(value, field, max_len)?;
            Ok(value.to_string())
        })
        .transpose()
}

fn validate_safe_metadata(value: &str, field: &str, max_len: usize) -> Result<(), String> {
    if value.chars().count() > max_len {
        return Err(format!("{} exceeds {} characters", field, max_len));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{} must not contain control characters", field));
    }
    Ok(())
}

fn sqlite_u64(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "token count exceeds SQLite integer range".to_string())
}

fn sqlite_nonnegative_u64(value: i64) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| "negative token count in API-key policy database".to_string())
}

fn conversion_error(error: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
    )
}

fn timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn budget_window_start(period: ApiKeyBudgetPeriod, now: DateTime<Utc>) -> String {
    match period {
        ApiKeyBudgetPeriod::Lifetime => "lifetime".to_string(),
        ApiKeyBudgetPeriod::CalendarMonth => format!("{:04}-{:02}", now.year(), now.month()),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDatabaseIdentity {
    version: u32,
    database_id: String,
    /// Only an unfinished first initialization may recover without a DB.
    /// Normal access must never recreate an established accounting ledger.
    initializing: bool,
    database_existed: bool,
}

fn identity_path(path: &Path) -> PathBuf {
    path.with_extension("sqlite3.identity")
}

/// The first initialization records whether a previous ledger existed. Keep
/// this evidence after restart so JSON migration cannot mistake a newly
/// created, empty database for the missing original accounting state.
pub(crate) fn database_existed_before_initialization(path: &Path) -> Result<bool, String> {
    let data = std::fs::read(identity_path(path))
        .map_err(|err| format!("failed to read API-key migration accounting evidence: {err}"))?;
    let identity: PolicyDatabaseIdentity = serde_json::from_slice(&data)
        .map_err(|err| format!("invalid API-key migration accounting evidence: {err}"))?;
    if identity.version != 1 || identity.initializing {
        return Err("API-key accounting initialization is not complete".to_string());
    }
    Ok(identity.database_existed)
}

fn private_file(path: &Path, create_new: bool) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|err| {
        format!(
            "failed to open private API-key policy file '{}': {err}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("failed to protect API-key policy file: {err}"))?;
    }
    Ok(file)
}

fn persist_identity(path: &Path, identity: &PolicyDatabaseIdentity) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(identity)
        .map_err(|err| format!("failed to encode API-key database identity: {err}"))?;
    crate::target::atomic_write(path, &data, true)
        .map_err(|err| format!("failed to persist API-key database identity: {err}"))
}

fn existing_database_id(connection: &Connection) -> Result<Option<String>, String> {
    let has_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'managed_registry_metadata')",
        [], |row| row.get(0),
    ).map_err(|err| format!("failed to inspect API-key database identity: {err}"))?;
    if !has_table {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT database_id FROM managed_registry_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| format!("failed to read API-key database identity: {err}"))
}

/// An existing path is not evidence that the previous accounting survived.
/// In particular, SQLite accepts an empty file as a new database. Inspect a
/// pre-registry database before pinning it or executing any CREATE statements,
/// otherwise an incomplete restore can silently recreate empty balances.
fn validate_legacy_accounting(connection: &Connection) -> Result<(), String> {
    let integrity: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|err| format!("unable to verify existing legacy API-key accounting: {err}"))?;
    if integrity != "ok" {
        return Err(
            "existing legacy API-key accounting failed SQLite integrity checks; restore the original ledger before migration"
                .to_string(),
        );
    }
    for query in [
        "SELECT api_key_id,budget_period,window_start,committed_input_tokens,
                reserved_input_tokens,updated_at FROM api_key_budget_windows LIMIT 0",
        "SELECT reservation_id,request_id,api_key_id,budget_limit,budget_period,
                window_start,reserved_input_tokens,committed_input_tokens,state,
                created_at,settled_at FROM api_key_budget_reservations LIMIT 0",
    ] {
        connection.prepare(query).map_err(|_| {
            "existing API-key policy database has no complete legacy accounting schema; refusing to recreate empty usage. Restore the original ledger before migration"
                .to_string()
        })?;
    }
    Ok(())
}

/// Shared durable connection factory for legacy budgets, registry, and new
/// renewable quotas. A companion identity pins the established DB: losing or
/// substituting that file fails closed instead of granting an empty allowance.
pub(crate) fn open_connection_at_path(path: &Path) -> Result<Connection, String> {
    if rusqlite::version_number() < 3_051_003 {
        return Err(
            "SQLite 3.51.3 or newer is required for durable quota accounting (WAL reset fix)"
                .to_string(),
        );
    }
    if let Some(parent) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .map_err(|err| format!("failed to create API-key policy directory: {}", err))?;
    }
    let marker_path = identity_path(path);
    // Serialize the short identity handshake, including its filesystem side
    // effects. Normal quota work releases this before starting a transaction.
    let identity_lock = private_file(&path.with_extension("sqlite3.identity-lock"), false)?;
    let lock_started = std::time::Instant::now();
    loop {
        match identity_lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock)
                if lock_started.elapsed() < SQLITE_BUSY_TIMEOUT =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                return Err(format!(
                    "failed to lock API-key database identity within bounded wait: {err}"
                ))
            }
        }
    }
    let database_existed = path
        .try_exists()
        .map_err(|err| format!("failed to inspect API-key database path: {err}"))?;
    let mut identity = match std::fs::read(&marker_path) {
        Ok(data) => {
            let marker: PolicyDatabaseIdentity = serde_json::from_slice(&data)
                .map_err(|err| format!("invalid API-key database identity marker: {err}"))?;
            if marker.version != 1 || uuid::Uuid::parse_str(&marker.database_id).is_err() {
                return Err("invalid API-key database identity marker".to_string());
            }
            if !database_existed && (!marker.initializing || marker.database_existed) {
                return Err(
                    "established API-key policy database is missing; refusing to reset usage"
                        .to_string(),
                );
            }
            marker
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if database_existed {
                let existing = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|err| format!("failed to inspect existing API-key database: {err}"))?;
                if existing_database_id(&existing)?.is_some() {
                    return Err("established API-key database identity marker is missing; refusing to repin usage".to_string());
                }
                validate_legacy_accounting(&existing)?;
            }
            let marker = PolicyDatabaseIdentity {
                version: 1,
                database_id: uuid::Uuid::new_v4().to_string(),
                initializing: true,
                database_existed,
            };
            persist_identity(&marker_path, &marker)?;
            marker
        }
        Err(err) => {
            return Err(format!(
                "failed to read API-key database identity marker: {err}"
            ))
        }
    };
    if !database_existed {
        private_file(path, true)?
            .sync_all()
            .map_err(|err| format!("failed to initialize API-key database file: {err}"))?;
    }
    // No CREATE flag: disappearance after the identity check is a failure.
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|err| format!("failed to open API-key policy database: {}", err))?;
    connection
        .busy_timeout(SQLITE_INITIALIZATION_BUSY_TIMEOUT)
        .map_err(|err| format!("failed to configure API-key policy database: {}", err))?;
    // Check identity before schema setup: never mutate or initialize a
    // substituted/missing established store while merely trying to read it.
    match existing_database_id(&connection)? {
        Some(database_id) if database_id != identity.database_id => {
            return Err(
                "API-key database identity mismatch; refusing substituted accounting state"
                    .to_string(),
            );
        }
        None if !identity.initializing => {
            return Err(
                "established API-key database identity is missing from the database".to_string(),
            );
        }
        None if identity.initializing && identity.database_existed => {
            // A crash after writing the initial marker must not make a
            // missing/truncated pre-upgrade ledger look like a fresh store.
            validate_legacy_accounting(&connection)?;
        }
        _ => {}
    }
    if !identity.initializing {
        for table in [
            "managed_api_keys",
            "managed_api_key_events",
            "api_key_budget_windows",
            "api_key_budget_reservations",
            "api_key_request_events",
        ] {
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    params![table],
                    |row| row.get(0),
                )
                .map_err(|err| {
                    format!("failed to validate established API-key accounting schema: {err}")
                })?;
            if !exists {
                return Err("established API-key accounting table is missing; refusing to recreate empty usage".to_string());
            }
        }
        let renewable_initialized: bool = connection
            .query_row(
                "SELECT renewable_initialized FROM managed_registry_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|err| format!("failed to validate renewable accounting identity: {err}"))?;
        if renewable_initialized {
            for table in [
                "renewable_quota_metadata",
                "renewable_quota_schedules",
                "renewable_quota_windows",
                "renewable_quota_requests",
                "renewable_quota_attempts",
                "renewable_quota_holds",
                "renewable_quota_ledger",
            ] {
                let exists: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    params![table], |row| row.get(0),
                ).map_err(|err| format!("failed to validate established renewable accounting schema: {err}"))?;
                if !exists {
                    return Err("established renewable quota table is missing; refusing to recreate empty usage".to_string());
                }
            }
        }
    }
    initialize_connection(&connection)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| format!("failed to begin API-key identity check: {err}"))?;
    match existing_database_id(&transaction)? {
        Some(database_id) if database_id != identity.database_id => {
            return Err(
                "API-key database identity mismatch; refusing substituted accounting state"
                    .to_string(),
            );
        }
        Some(_) => {}
        None if !identity.initializing => {
            return Err(
                "established API-key database identity is missing from the database".to_string(),
            );
        }
        None => {
            transaction.execute(
                "INSERT INTO managed_registry_metadata(singleton, database_id, keys_initialized) VALUES(1, ?1, 0)",
                params![identity.database_id],
            ).map_err(|err| format!("failed to establish API-key database identity: {err}"))?;
        }
    }
    // Pin renewable schema existence in this same initialization transaction.
    // Reads must never recreate a dropped usage table and restore allowance.
    crate::api_key_quota::initialize(&transaction)?;
    // The reset-credit action ledger carries an upstream spending idempotency
    // key.  Its initialized bit makes a missing established table a hard
    // failure rather than an opportunity to create an empty ledger and spend
    // a credit twice after restart.
    crate::codex_reset_credit::initialize(&transaction)?;
    transaction
        .commit()
        .map_err(|err| format!("failed to commit API-key database identity: {err}"))?;
    if identity.initializing {
        identity.initializing = false;
        persist_identity(&marker_path, &identity)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for protected_path in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            match std::fs::set_permissions(&protected_path, std::fs::Permissions::from_mode(0o600))
            {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(format!("failed to protect API-key policy database: {err}"))
                }
            }
        }
    }
    connection
        .busy_timeout(SQLITE_BUSY_TIMEOUT)
        .map_err(|err| format!("failed to configure API-key policy database: {}", err))?;
    Ok(connection)
}

fn initialize_connection(connection: &Connection) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 0..SQLITE_INITIALIZATION_RETRY_ATTEMPTS {
        match connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS managed_registry_metadata (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                database_id TEXT NOT NULL,
                keys_initialized INTEGER NOT NULL DEFAULT 0 CHECK(keys_initialized IN (0, 1)),
                renewable_initialized INTEGER NOT NULL DEFAULT 0 CHECK(renewable_initialized IN (0, 1)),
                codex_reset_credits_initialized INTEGER NOT NULL DEFAULT 0 CHECK(codex_reset_credits_initialized IN (0, 1))
             );
             CREATE TABLE IF NOT EXISTS managed_api_keys (
                id TEXT PRIMARY KEY,
                record_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS managed_api_key_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                api_key_id TEXT NOT NULL,
                event_kind TEXT NOT NULL CHECK(event_kind IN ('created', 'migrated', 'revoked', 'credential_rotated', 'policy_changed')),
                occurred_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS api_key_request_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                occurred_at TEXT NOT NULL,
                request_id TEXT NOT NULL,
                api_key_id TEXT NOT NULL,
                event_kind TEXT NOT NULL,
                decision TEXT NOT NULL,
                request_path TEXT NOT NULL,
                provider TEXT,
                model TEXT,
                account_key TEXT,
                status_code INTEGER,
                estimated_input_tokens INTEGER,
                actual_input_tokens INTEGER,
                measurement_method TEXT,
                reservation_id TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_api_key_policy_events_key_id
                ON api_key_request_events(api_key_id, id DESC);
             CREATE INDEX IF NOT EXISTS idx_api_key_policy_events_request_id
                ON api_key_request_events(request_id, id DESC);
             CREATE INDEX IF NOT EXISTS idx_api_key_policy_events_reservation_id
                ON api_key_request_events(reservation_id, id DESC);
             CREATE TABLE IF NOT EXISTS api_key_budget_windows (
                api_key_id TEXT NOT NULL,
                budget_period TEXT NOT NULL,
                window_start TEXT NOT NULL,
                committed_input_tokens INTEGER NOT NULL DEFAULT 0
                    CHECK(committed_input_tokens >= 0),
                reserved_input_tokens INTEGER NOT NULL DEFAULT 0
                    CHECK(reserved_input_tokens >= 0),
                updated_at TEXT NOT NULL,
                PRIMARY KEY (api_key_id, budget_period, window_start)
             );
             CREATE TABLE IF NOT EXISTS api_key_budget_reservations (
                reservation_id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL,
                api_key_id TEXT NOT NULL,
                budget_limit INTEGER NOT NULL CHECK(budget_limit > 0),
                budget_period TEXT NOT NULL,
                window_start TEXT NOT NULL,
                reserved_input_tokens INTEGER NOT NULL
                    CHECK(reserved_input_tokens > 0),
                committed_input_tokens INTEGER,
                state TEXT NOT NULL CHECK(state IN ('active', 'committed', 'released')),
                created_at TEXT NOT NULL,
                settled_at TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_api_key_policy_reservations_window
                ON api_key_budget_reservations(api_key_id, budget_period, window_start, state);",
            )
            .and_then(|()| scrub_preexisting_audit_models(connection))
        {
            Ok(()) => return Ok(()),
            Err(err) if sqlite_error_is_busy_or_locked(&err) => {
                last_error = Some(err);
                if attempt + 1 < SQLITE_INITIALIZATION_RETRY_ATTEMPTS {
                    let multiplier = 1_u32 << attempt.min(6);
                    std::thread::sleep(
                        SQLITE_INITIALIZATION_RETRY_BASE_DELAY.saturating_mul(multiplier),
                    );
                }
            }
            Err(err) => {
                return Err(format!(
                    "failed to initialize API-key policy database: {}",
                    err
                ));
            }
        }
    }
    let error = last_error.expect("initialization retry loop records its final SQLite error");
    Err(format!(
        "failed to initialize API-key policy database after {} bounded retries: {}",
        SQLITE_INITIALIZATION_RETRY_ATTEMPTS, error
    ))
}

/// The first audit implementation accepted `model` directly from a client
/// request. Erase that historical data exactly once under the same bounded
/// initialization retry used for schema setup. New writes discard this field
/// in `normalize_event`, and reads suppress it defensively as well.
fn scrub_preexisting_audit_models(connection: &Connection) -> rusqlite::Result<()> {
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if version >= POLICY_DB_SCHEMA_VERSION {
        return Ok(());
    }

    let migration = format!(
        "BEGIN IMMEDIATE;
         UPDATE api_key_request_events SET model = NULL WHERE model IS NOT NULL;
         PRAGMA user_version = {};
         COMMIT;",
        POLICY_DB_SCHEMA_VERSION
    );
    let result = connection.execute_batch(&migration);
    if result.is_err() {
        // `execute_batch` does not automatically roll back a transaction it
        // successfully began before a later statement failed. Do not leave a
        // connection wedged before the bounded retry loop gets another turn.
        let _ = connection.execute_batch("ROLLBACK;");
    }
    result
}

fn sqlite_error_is_busy_or_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(sqlite_error, _)
            if matches!(
                sqlite_error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn bundled_sqlite_contains_wal_reset_fix() {
        assert!(
            rusqlite::version_number() >= 3_051_003,
            "unsafe SQLite runtime {}",
            rusqlite::version()
        );
    }

    #[test]
    fn identity_refuses_missing_substituted_or_schema_damaged_database() {
        let directory = temp_dir();
        let first = directory.join("first.sqlite3");
        let second = directory.join("second.sqlite3");
        drop(open_connection_at_path(&first).unwrap());
        drop(open_connection_at_path(&second).unwrap());
        let original = directory.join("first-original.sqlite3");
        std::fs::rename(&first, &original).unwrap();
        assert!(open_connection_at_path(&first)
            .unwrap_err()
            .contains("refusing to reset usage"));
        assert!(!first.exists());
        std::fs::copy(&second, &first).unwrap();
        assert!(open_connection_at_path(&first)
            .unwrap_err()
            .contains("identity mismatch"));
        std::fs::rename(&original, &first).unwrap();
        let connection = open_connection_at_path(&first).unwrap();
        connection
            .execute_batch("DROP TABLE api_key_budget_windows;")
            .unwrap();
        drop(connection);
        assert!(open_connection_at_path(&first)
            .unwrap_err()
            .contains("accounting table is missing"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_identity_marker_never_repins_established_database() {
        let directory = temp_dir();
        let path = directory.join(POLICY_DB_FILE);
        drop(open_connection_at_path(&path).unwrap());
        std::fs::rename(identity_path(&path), directory.join("identity.backup")).unwrap();
        assert!(open_connection_at_path(&path)
            .unwrap_err()
            .contains("identity marker is missing"));
        assert!(!identity_path(&path).exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unfinished_first_identity_initialization_is_restartable() {
        let directory = temp_dir();
        let path = directory.join(POLICY_DB_FILE);
        let identity = PolicyDatabaseIdentity {
            version: 1,
            database_id: uuid::Uuid::new_v4().to_string(),
            initializing: true,
            database_existed: false,
        };
        persist_identity(&identity_path(&path), &identity).unwrap();
        // A crash may occur either before or after creating the fresh file;
        // the marker proves that this empty file held no historical usage.
        private_file(&path, true).unwrap();
        let connection = open_connection_at_path(&path).unwrap();
        assert_eq!(
            existing_database_id(&connection).unwrap().as_deref(),
            Some(identity.database_id.as_str())
        );
        let completed: PolicyDatabaseIdentity =
            serde_json::from_slice(&std::fs::read(identity_path(&path)).unwrap()).unwrap();
        assert!(!completed.initializing);
        drop(connection);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unfinished_legacy_identity_initialization_cannot_adopt_an_empty_ledger() {
        let directory = temp_dir();
        let path = directory.join(POLICY_DB_FILE);
        let identity = PolicyDatabaseIdentity {
            version: 1,
            database_id: uuid::Uuid::new_v4().to_string(),
            initializing: true,
            database_existed: true,
        };
        persist_identity(&identity_path(&path), &identity).unwrap();
        private_file(&path, true).unwrap();
        let original_marker = std::fs::read(identity_path(&path)).unwrap();
        for _ in 0..2 {
            assert!(open_connection_at_path(&path)
                .unwrap_err()
                .contains("no complete legacy accounting schema"));
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
            assert_eq!(
                std::fs::read(identity_path(&path)).unwrap(),
                original_marker
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn backup_includes_live_wal_rules_holds_and_identity_without_overwriting() {
        let directory = temp_dir();
        let source = directory.join("source");
        let destination = directory.join("backup");
        std::fs::create_dir(&source).unwrap();
        let cfg: crate::Config = serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:0", "upstream_base": "https://example.test",
            "proxy_api_key": "", "tokens": [], "auth_dir": source,
        }))
        .unwrap();
        let quota = crate::api_key_quota::QuotaPolicy {
            timezone: "Asia/Jakarta".to_string(),
            rules: vec![crate::api_key_quota::QuotaRule {
                metric: crate::api_key_quota::QuotaMetric::Requests,
                period: crate::api_key_quota::QuotaPeriod::Weekly,
                limit: 1,
            }],
        };
        let store = crate::api_keys::ApiKeyStore {
            keys: vec![crate::api_keys::ApiKeyRecord {
                id: "backed-up-key".to_string(),
                access: crate::api_keys::ApiKeyAccess {
                    quota: Some(quota),
                    ..Default::default()
                },
                ..Default::default()
            }],
        };
        crate::api_keys::save(&cfg, &store).unwrap();
        // An open connection with automatic checkpointing disabled keeps
        // committed changes in WAL while the backup is taken.
        let connection = open_connection_at_path(&policy_db_path(&cfg)).unwrap();
        connection
            .execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let request = crate::api_key_quota::ReserveRequest {
            api_key_id: "backed-up-key".into(),
            request_id: "request".into(),
            attempt_id: "attempt".into(),
            provider: Some("claude".into()),
            account_key: None,
            expected_legacy_budget: None,
            bounds: Default::default(),
        };
        let now = at("2026-09-15T08:00:00Z");
        assert!(matches!(
            crate::api_key_quota::reserve_at_path(&policy_db_path(&cfg), &request, now).unwrap(),
            crate::api_key_quota::Admission::Reserved(_)
        ));
        assert!(
            std::fs::metadata(source.join("api-key-policy.sqlite3-wal"))
                .unwrap()
                .len()
                > 32
        );
        backup(&cfg, &destination).unwrap();
        let snapshot = destination.join(POLICY_DB_FILE);
        let copied = crate::api_key_quota::summary_at_path(&snapshot, "backed-up-key", now)
            .unwrap()
            .unwrap();
        assert_eq!(copied.rules[0].reserved, 1);
        assert_eq!(copied.rules[0].remaining, 0);
        assert_eq!(
            copied.anchor.as_deref(),
            Some("2026-09-15T08:00:00.000000000Z")
        );
        let restored_cfg: crate::Config = serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:0", "upstream_base": "https://example.test",
            "proxy_api_key": "", "tokens": [], "auth_dir": destination,
        }))
        .unwrap();
        assert_eq!(crate::api_keys::load(&restored_cfg).unwrap(), store);
        // Settling the source cannot change the already completed snapshot.
        crate::api_key_quota::settle_at_path(
            &policy_db_path(&cfg),
            "attempt",
            crate::api_key_quota::Settlement::Released,
            now,
        )
        .unwrap();
        assert!(backup(&cfg, &destination)
            .unwrap_err()
            .contains("new destination directory"));
        assert_eq!(
            crate::api_key_quota::summary_at_path(&snapshot, "backed-up-key", now)
                .unwrap()
                .unwrap()
                .rules[0]
                .reserved,
            1
        );
        assert!(destination.join("backup-manifest.json").is_file());
        assert!(!destination.join("gateway-usage-history.sqlite3").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            for file in [
                snapshot,
                destination.join("api-key-policy.sqlite3.identity"),
                destination.join("api-keys.json"),
                destination.join("backup-manifest.json"),
            ] {
                assert_eq!(
                    std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        drop(connection);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "io-gateway-api-key-policy-store-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn budget(limit: u64, period: ApiKeyBudgetPeriod) -> ApiKeyInputTokenBudget {
        ApiKeyInputTokenBudget { limit, period }
    }

    fn request(
        id: &str,
        tokens: u64,
        budget: ApiKeyInputTokenBudget,
    ) -> ApiKeyInputTokenReservationRequest {
        ApiKeyInputTokenReservationRequest {
            reservation_id: id.to_string(),
            request_id: format!("request-{}", id),
            api_key_id: "key-one".to_string(),
            budget,
            reserved_input_tokens: tokens,
        }
    }

    fn reserved(result: ApiKeyInputTokenReservationResult) -> ApiKeyInputTokenReservation {
        match result {
            ApiKeyInputTokenReservationResult::Reserved { reservation, .. } => reservation,
            ApiKeyInputTokenReservationResult::Denied(denied) => {
                panic!("unexpected budget denial: {:?}", denied)
            }
        }
    }

    #[test]
    fn appends_only_safe_audit_metadata_and_lists_it() {
        let dir = temp_dir();
        let path = dir.join(POLICY_DB_FILE);
        let saved = append_event_at_path(
            &path,
            &ApiKeyRequestAuditEvent {
                request_id: "request-1".to_string(),
                api_key_id: "key-one".to_string(),
                kind: ApiKeyPolicyEventKind::Authorization,
                decision: ApiKeyPolicyDecision::BudgetExceeded,
                request_path: "/v1/responses".to_string(),
                provider: Some("codex".to_string()),
                model: Some("model-carries-a-synthetic-secret".to_string()),
                account_key: None,
                status_code: Some(429),
                estimated_input_tokens: Some(17),
                actual_input_tokens: None,
                measurement: Some(ApiKeyInputMeasurement::Conservative),
                reservation_id: Some("reservation-1".to_string()),
            },
            at("2026-09-19T01:02:03Z"),
        )
        .unwrap();
        assert_eq!(saved.occurred_at, "2026-09-19T01:02:03.000Z");
        assert_eq!(saved.event.model, None);

        let events = list_events_at_path(
            &path,
            &ApiKeyRequestAuditQuery {
                api_key_id: Some("key-one".to_string()),
                request_id: None,
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(events, vec![saved.clone()]);

        let connection = Connection::open(path).unwrap();
        let mut statement = connection
            .prepare("PRAGMA table_info(api_key_request_events)")
            .unwrap();
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!columns.iter().any(|column| column.contains("prompt")));
        assert!(!columns.iter().any(|column| column.contains("body")));
        assert!(!columns.iter().any(|column| column.contains("raw")));
        assert_eq!(
            connection
                .query_row(
                    "SELECT model FROM api_key_request_events WHERE id = ?1",
                    params![saved.id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn migration_erases_preexisting_untrusted_model_metadata() {
        let dir = temp_dir();
        let path = dir.join(POLICY_DB_FILE);
        let connection = open_connection_at_path(&path).unwrap();
        connection
            .execute(
                "INSERT INTO api_key_request_events (
                    occurred_at, request_id, api_key_id, event_kind, decision,
                    request_path, provider, model
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    "2026-09-19T01:02:03.000Z",
                    "request-legacy",
                    "key-one",
                    "authorization",
                    "allowed",
                    "/v1/responses",
                    "codex",
                    "model-carries-a-synthetic-secret"
                ],
            )
            .unwrap();
        connection
            .execute_batch("PRAGMA user_version = 0;")
            .unwrap();
        drop(connection);

        let reopened = open_connection_at_path(&path).unwrap();
        assert_eq!(
            reopened
                .query_row(
                    "SELECT model FROM api_key_request_events WHERE request_id = ?1",
                    params!["request-legacy"],
                    |row| row.get::<_, Option<String>>(0),
                )
                .unwrap(),
            None
        );
        let event = list_events_at_path(
            &path,
            &ApiKeyRequestAuditQuery {
                api_key_id: Some("key-one".to_string()),
                request_id: Some("request-legacy".to_string()),
                limit: Some(1),
            },
        )
        .unwrap()
        .pop()
        .unwrap();
        assert_eq!(event.event.model, None);
    }

    #[test]
    fn reservation_is_atomic_and_settlement_reconciles_to_actual_charge() {
        let dir = temp_dir();
        let path = dir.join(POLICY_DB_FILE);
        let now = at("2026-09-19T01:02:03Z");
        let configured_budget = budget(10, ApiKeyBudgetPeriod::Lifetime);

        let first = reserved(
            reserve_input_tokens_at_path(
                &path,
                &request("hold-1", 7, configured_budget.clone()),
                now,
            )
            .unwrap(),
        );
        assert_eq!(first.state, ApiKeyInputTokenReservationState::Active);

        let denied = reserve_input_tokens_at_path(
            &path,
            &request("hold-2", 4, configured_budget.clone()),
            now,
        )
        .unwrap();
        assert_eq!(
            denied,
            ApiKeyInputTokenReservationResult::Denied(ApiKeyInputTokenBudgetDenied {
                api_key_id: "key-one".to_string(),
                budget_limit: 10,
                period: ApiKeyBudgetPeriod::Lifetime,
                window_start: "lifetime".to_string(),
                committed_input_tokens: 0,
                reserved_input_tokens: 7,
                requested_input_tokens: 4,
            })
        );

        let settled = settle_input_token_reservation_at_path(
            &path,
            "hold-1",
            ApiKeyInputTokenSettlement::Commit {
                actual_input_tokens: Some(5),
            },
            at("2026-09-19T01:04:00Z"),
        )
        .unwrap();
        // The pre-dispatch reservation is the safety floor even if a provider
        // reports a lower value. This prevents a low/partial report from
        // making capacity appear that may have been billed.
        assert_eq!(settled.reservation.committed_input_tokens, Some(7));
        assert_eq!(
            input_token_budget_summary_at_path(&path, "key-one", &configured_budget, now).unwrap(),
            ApiKeyInputTokenBudgetSummary {
                api_key_id: "key-one".to_string(),
                budget_limit: 10,
                period: ApiKeyBudgetPeriod::Lifetime,
                window_start: "lifetime".to_string(),
                committed_input_tokens: 7,
                reserved_input_tokens: 0,
                available_input_tokens: 3,
            }
        );

        reserved(
            reserve_input_tokens_at_path(&path, &request("hold-3", 3, configured_budget), now)
                .unwrap(),
        );
    }

    #[test]
    fn release_restores_capacity_unknown_outcome_charges_the_hold_and_months_reset() {
        let dir = temp_dir();
        let path = dir.join(POLICY_DB_FILE);
        let month_budget = budget(10, ApiKeyBudgetPeriod::CalendarMonth);
        let september = at("2026-09-30T23:59:00Z");

        reserved(
            reserve_input_tokens_at_path(
                &path,
                &request("release-me", 8, month_budget.clone()),
                september,
            )
            .unwrap(),
        );
        settle_input_token_reservation_at_path(
            &path,
            "release-me",
            ApiKeyInputTokenSettlement::Release,
            september,
        )
        .unwrap();
        assert_eq!(
            input_token_budget_summary_at_path(&path, "key-one", &month_budget, september)
                .unwrap()
                .available_input_tokens,
            10
        );

        reserved(
            reserve_input_tokens_at_path(
                &path,
                &request("ambiguous", 10, month_budget.clone()),
                september,
            )
            .unwrap(),
        );
        settle_input_token_reservation_at_path(
            &path,
            "ambiguous",
            ApiKeyInputTokenSettlement::Commit {
                actual_input_tokens: None,
            },
            september,
        )
        .unwrap();
        assert_eq!(
            input_token_budget_summary_at_path(&path, "key-one", &month_budget, september)
                .unwrap()
                .available_input_tokens,
            0
        );
        // A new UTC calendar month starts an independent window.
        assert_eq!(
            input_token_budget_summary_at_path(
                &path,
                "key-one",
                &month_budget,
                at("2026-10-01T00:00:00Z")
            )
            .unwrap()
            .available_input_tokens,
            10
        );
    }

    #[test]
    fn stale_sweeper_commits_old_holds_and_never_refunds_them() {
        let dir = temp_dir();
        let path = dir.join(POLICY_DB_FILE);
        let configured_budget = budget(20, ApiKeyBudgetPeriod::Lifetime);
        let created_at = at("2026-09-19T00:00:00Z");
        let recent_at = at("2026-09-19T01:40:00Z");
        let sweep_at = at("2026-09-19T02:00:00Z");

        reserved(
            reserve_input_tokens_at_path(
                &path,
                &request("stale", 7, configured_budget.clone()),
                created_at,
            )
            .unwrap(),
        );
        reserved(
            reserve_input_tokens_at_path(
                &path,
                &request("recent", 5, configured_budget.clone()),
                recent_at,
            )
            .unwrap(),
        );

        let swept = sweep_stale_input_token_reservations_at_path(
            &path,
            Duration::from_secs(60 * 60),
            32,
            sweep_at,
        )
        .unwrap();
        assert_eq!(swept.examined, 1);
        assert_eq!(swept.already_settled, 0);
        assert_eq!(swept.committed.len(), 1);
        assert_eq!(swept.committed[0].reservation_id, "stale");
        assert_eq!(
            swept.committed[0].state,
            ApiKeyInputTokenReservationState::Committed
        );
        assert_eq!(swept.committed[0].committed_input_tokens, Some(7));

        // The old hold moved from reserved to committed; it did not free any
        // capacity. The still-live request remains reserved.
        assert_eq!(
            input_token_budget_summary_at_path(&path, "key-one", &configured_budget, sweep_at)
                .unwrap(),
            ApiKeyInputTokenBudgetSummary {
                api_key_id: "key-one".to_string(),
                budget_limit: 20,
                period: ApiKeyBudgetPeriod::Lifetime,
                window_start: "lifetime".to_string(),
                committed_input_tokens: 7,
                reserved_input_tokens: 5,
                available_input_tokens: 8,
            }
        );

        // A repeat has nothing active to charge and cannot refund the old
        // reservation accidentally.
        assert_eq!(
            sweep_stale_input_token_reservations_at_path(
                &path,
                Duration::from_secs(60 * 60),
                32,
                sweep_at,
            )
            .unwrap(),
            ApiKeyInputTokenReservationSweepResult::default()
        );
    }

    #[test]
    fn stale_sweeper_tolerates_a_concurrent_release_without_charging_it() {
        let dir = temp_dir();
        let path = dir.join(POLICY_DB_FILE);
        let configured_budget = budget(10, ApiKeyBudgetPeriod::Lifetime);
        let now = at("2026-09-19T00:00:00Z");
        reserved(
            reserve_input_tokens_at_path(
                &path,
                &request("release-race", 7, configured_budget.clone()),
                now,
            )
            .unwrap(),
        );
        settle_input_token_reservation_at_path(
            &path,
            "release-race",
            ApiKeyInputTokenSettlement::Release,
            now,
        )
        .unwrap();

        // This models the interval after a sweeper selected an active ID and
        // before it acquired its own immediate transaction. A normal release
        // may win that race; the sweeper must skip it rather than erroring or
        // converting it into a billed request.
        let raced = settle_input_token_reservation_at_path_with_terminal_tolerance(
            &path,
            "release-race",
            ApiKeyInputTokenSettlement::Commit {
                actual_input_tokens: None,
            },
            now,
            true,
        )
        .unwrap();
        assert!(!raced.settled_now);
        assert_eq!(
            raced.reservation.state,
            ApiKeyInputTokenReservationState::Released
        );
        assert_eq!(
            input_token_budget_summary_at_path(&path, "key-one", &configured_budget, now)
                .unwrap()
                .available_input_tokens,
            10
        );
    }

    #[test]
    fn concurrent_reservations_cannot_both_spend_one_remaining_slot() {
        let dir = temp_dir();
        let path = Arc::new(dir.join(POLICY_DB_FILE));
        let barrier = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for id in ["concurrent-a", "concurrent-b"] {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                reserve_input_tokens_at_path(
                    &path,
                    &request(id, 10, budget(10, ApiKeyBudgetPeriod::Lifetime)),
                    at("2026-09-19T01:02:03Z"),
                )
                .unwrap()
            }));
        }
        barrier.wait();
        let results = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    ApiKeyInputTokenReservationResult::Reserved { .. }
                ))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, ApiKeyInputTokenReservationResult::Denied(_)))
                .count(),
            1
        );
    }

    #[test]
    fn retries_are_idempotent_but_parameter_and_terminal_state_mismatches_fail() {
        let dir = temp_dir();
        let path = dir.join(POLICY_DB_FILE);
        let now = at("2026-09-19T01:02:03Z");
        let req = request("retry", 5, budget(10, ApiKeyBudgetPeriod::Lifetime));
        assert!(matches!(
            reserve_input_tokens_at_path(&path, &req, now).unwrap(),
            ApiKeyInputTokenReservationResult::Reserved { created: true, .. }
        ));
        assert!(matches!(
            reserve_input_tokens_at_path(&path, &req, now).unwrap(),
            ApiKeyInputTokenReservationResult::Reserved { created: false, .. }
        ));
        let mut mismatched = req.clone();
        mismatched.reserved_input_tokens = 6;
        assert!(reserve_input_tokens_at_path(&path, &mismatched, now).is_err());

        settle_input_token_reservation_at_path(
            &path,
            "retry",
            ApiKeyInputTokenSettlement::Release,
            now,
        )
        .unwrap();
        assert!(settle_input_token_reservation_at_path(
            &path,
            "retry",
            ApiKeyInputTokenSettlement::Commit {
                actual_input_tokens: Some(5),
            },
            now,
        )
        .is_err());
    }
}
