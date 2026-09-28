//! Durable, first-use-anchored, multi-resource API-key quotas.
//!
//! Reporting is intentionally not the accounting authority. All holds are
//! committed before dispatch and remain charged through crashes and restarts.
//! A missing provider measurement is never interpreted as zero.

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path};

const MAX_AMOUNT: u64 = i64::MAX as u64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuotaMetric {
    InputTokens,
    UncachedInputTokens,
    OutputTokens,
    CacheReadTokens,
    CacheWriteTokens,
    CacheTokens,
    Requests,
}

impl QuotaMetric {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InputTokens => "input_tokens",
            Self::UncachedInputTokens => "uncached_input_tokens",
            Self::OutputTokens => "output_tokens",
            Self::CacheReadTokens => "cache_read_tokens",
            Self::CacheWriteTokens => "cache_write_tokens",
            Self::CacheTokens => "cache_tokens",
            Self::Requests => "requests",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuotaPeriod {
    Daily,
    Weekly,
    Monthly,
}

impl QuotaPeriod {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuotaRule {
    pub metric: QuotaMetric,
    pub period: QuotaPeriod,
    pub limit: u64,
}

fn default_timezone() -> String {
    "UTC".to_string()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuotaPolicy {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub rules: Vec<QuotaRule>,
}

impl QuotaPolicy {
    pub(crate) fn normalized(&self) -> Result<Self, String> {
        let timezone: Tz = self.timezone.trim().parse().map_err(|_| {
            "quota timezone must be an IANA timezone, such as UTC or Asia/Jakarta".to_string()
        })?;
        if self.rules.is_empty() {
            return Err("quota must contain at least one rule; use null to disable it".to_string());
        }
        if self.rules.len() > 21 {
            return Err("quota supports at most 21 distinct metric/period rules".to_string());
        }
        let mut rules = self.rules.clone();
        let mut identities = BTreeSet::new();
        for rule in &rules {
            if rule.limit == 0 || rule.limit > MAX_AMOUNT {
                return Err(format!(
                    "{} {} quota limit must be between 1 and {}",
                    rule.metric.as_str(),
                    rule.period.as_str(),
                    MAX_AMOUNT
                ));
            }
            if !identities.insert((rule.metric, rule.period)) {
                return Err(format!(
                    "duplicate {} {} quota rule",
                    rule.metric.as_str(),
                    rule.period.as_str()
                ));
            }
        }
        rules.sort_by_key(|rule| (rule.metric, rule.period));
        Ok(Self {
            timezone: timezone.to_string(),
            rules,
        })
    }

    pub(crate) fn needs_input_measurement(&self) -> bool {
        self.rules.iter().any(|rule| {
            !matches!(
                rule.metric,
                QuotaMetric::Requests | QuotaMetric::OutputTokens
            )
        })
    }

    pub(crate) fn needs_output_bound(&self) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.metric == QuotaMetric::OutputTokens)
    }

    fn shape(&self) -> Result<String, String> {
        let normalized = self.normalized()?;
        serde_json::to_string(&(
            normalized.timezone,
            normalized
                .rules
                .iter()
                .map(|rule| (rule.metric, rule.period))
                .collect::<Vec<_>>(),
        ))
        .map_err(|error| error.to_string())
    }
}

/// Optional values are deliberate: explicit zero is different from unknown.
/// Values supplied at admission must be defensible upper bounds. Values in
/// `Reported` settlement must be complete, trustworthy native-provider counts.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Usage {
    pub input_tokens: Option<u64>,
    pub uncached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_tokens: Option<u64>,
}

impl Usage {
    fn get(&self, metric: QuotaMetric) -> Option<u64> {
        match metric {
            QuotaMetric::InputTokens => self.input_tokens,
            QuotaMetric::UncachedInputTokens => self.uncached_input_tokens,
            QuotaMetric::OutputTokens => self.output_tokens,
            QuotaMetric::CacheReadTokens => self.cache_read_tokens,
            QuotaMetric::CacheWriteTokens => self.cache_write_tokens,
            QuotaMetric::CacheTokens => self.cache_tokens.or_else(|| {
                self.cache_read_tokens?
                    .checked_add(self.cache_write_tokens?)
            }),
            QuotaMetric::Requests => None,
        }
    }

    fn validate(&self) -> Result<(), String> {
        for value in [
            self.input_tokens,
            self.uncached_input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.cache_tokens,
        ]
        .into_iter()
        .flatten()
        {
            checked_amount(value)?;
        }
        if let (Some(read), Some(write)) = (self.cache_read_tokens, self.cache_write_tokens) {
            checked_amount(
                read.checked_add(write)
                    .ok_or("cache token count overflow")?,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReserveRequest {
    pub api_key_id: String,
    pub request_id: String,
    pub attempt_id: String,
    pub provider: Option<String>,
    pub account_key: Option<String>,
    /// The legacy ledger holds were admitted under this authenticated policy
    /// snapshot. A concurrent edit requires a fresh client admission rather
    /// than silently dispatching against an obsolete or missing legacy hold.
    pub expected_legacy_budget: Option<crate::api_keys::ApiKeyInputTokenBudget>,
    pub bounds: Usage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Reservation {
    pub id: String,
    pub api_key_id: String,
    pub request_id: String,
    pub anchor: String,
    /// Replay is accounting-idempotent, not permission for another HTTP call.
    pub replay: bool,
    pub state: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Denial {
    pub code: String,
    pub message: String,
    pub metric: Option<QuotaMetric>,
    pub period: Option<QuotaPeriod>,
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset_at: Option<String>,
    pub retry_after_seconds: Option<u64>,
}

impl Denial {
    fn policy(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
            metric: None,
            period: None,
            limit: None,
            remaining: None,
            reset_at: None,
            retry_after_seconds: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    NotConfigured,
    Reserved(Reservation),
    Denied(Denial),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Settlement {
    Reported(Usage),
    Unknown,
    /// Only use when dispatch is known not to have happened.
    Released,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SettlementResult {
    pub reservation_id: String,
    pub changed: bool,
    pub state: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct RuleSummary {
    pub metric: QuotaMetric,
    pub period: QuotaPeriod,
    pub limit: u64,
    pub window_start: Option<String>,
    pub reset_at: Option<String>,
    pub confirmed: u64,
    pub reserved: u64,
    pub uncertain: u64,
    pub remaining: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct QuotaSummary {
    pub timezone: String,
    pub anchor: Option<String>,
    pub revision: u64,
    pub rules: Vec<RuleSummary>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct RecoveryResult {
    pub examined: usize,
    pub recovered: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Window {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// Pure calendar calculation. Persisted high-water time is applied by callers.
pub(crate) fn window_at(
    anchor: DateTime<Utc>,
    now: DateTime<Utc>,
    period: QuotaPeriod,
    timezone: &str,
) -> Result<Window, String> {
    let now = now.max(anchor);
    match period {
        QuotaPeriod::Daily | QuotaPeriod::Weekly => {
            let seconds = if period == QuotaPeriod::Daily {
                86_400
            } else {
                604_800
            };
            let index = now.signed_duration_since(anchor).num_seconds() / seconds;
            let start = anchor
                .checked_add_signed(Duration::seconds(
                    index.checked_mul(seconds).ok_or("quota window overflow")?,
                ))
                .ok_or("quota window overflow")?;
            let end = start
                .checked_add_signed(Duration::seconds(seconds))
                .ok_or("quota window overflow")?;
            Ok(Window { start, end })
        }
        QuotaPeriod::Monthly => {
            let timezone: Tz = timezone
                .parse()
                .map_err(|_| "invalid stored quota timezone")?;
            let local_anchor = anchor.with_timezone(&timezone);
            let local_now = now.with_timezone(&timezone);
            let mut index = ((local_now.year() as i64 - local_anchor.year() as i64) * 12
                + local_now.month0() as i64
                - local_anchor.month0() as i64)
                .max(0);
            let mut start = month_boundary(anchor, timezone, index)?;
            if start > now && index > 0 {
                index -= 1;
                start = month_boundary(anchor, timezone, index)?;
            }
            let mut end = month_boundary(anchor, timezone, index + 1)?;
            if end <= now {
                index += 1;
                start = end;
                end = month_boundary(anchor, timezone, index + 1)?;
            }
            Ok(Window { start, end })
        }
    }
}

fn month_boundary(
    anchor: DateTime<Utc>,
    timezone: Tz,
    index: i64,
) -> Result<DateTime<Utc>, String> {
    if index == 0 {
        return Ok(anchor);
    }
    let local = anchor.with_timezone(&timezone);
    let month = (local.year() as i64)
        .checked_mul(12)
        .and_then(|year| year.checked_add(local.month0() as i64))
        .and_then(|value| value.checked_add(index))
        .ok_or("quota calendar overflow")?;
    let year = i32::try_from(month.div_euclid(12)).map_err(|_| "quota calendar overflow")?;
    let month = month.rem_euclid(12) as u32 + 1;
    let date = (1..=local.day())
        .rev()
        .find_map(|day| NaiveDate::from_ymd_opt(year, month, day))
        .ok_or("quota calendar overflow")?;
    let mut wall_time = date.and_time(local.time());
    // Includes skipped dates (e.g. Pacific/Apia), not merely one-hour DST gaps.
    for _ in 0..=172_800 {
        match timezone.from_local_datetime(&wall_time) {
            LocalResult::Single(value) => return Ok(value.with_timezone(&Utc)),
            LocalResult::Ambiguous(a, b) => return Ok(a.min(b).with_timezone(&Utc)),
            LocalResult::None => {
                wall_time = wall_time
                    .with_nanosecond(0)
                    .ok_or("quota calendar overflow")?
                    .checked_add_signed(Duration::seconds(1))
                    .ok_or("quota calendar overflow")?;
            }
        }
    }
    Err("unable to resolve quota renewal in configured timezone".to_string())
}

fn stamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}
fn parse_stamp(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| "invalid persisted quota timestamp".to_string())
}
fn checked_amount(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "quota amount exceeds SQLite signed-integer range".to_string())
}
fn nonnegative(value: i64) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| "negative persisted quota amount".to_string())
}
fn sql(error: rusqlite::Error) -> String {
    format!("renewable API-key quota database: {}", error)
}

const TABLES: &[&str] = &[
    "renewable_quota_metadata",
    "renewable_quota_schedules",
    "renewable_quota_windows",
    "renewable_quota_requests",
    "renewable_quota_attempts",
    "renewable_quota_holds",
    "renewable_quota_ledger",
];

/// Schema creation and its durable initialized bit must commit together. Once
/// established, missing tables or the singleton metadata row are corruption,
/// never permission to manufacture empty balances.
pub(crate) fn initialize(connection: &Connection) -> Result<(), String> {
    if connection.is_autocommit() {
        let tx =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).map_err(sql)?;
        initialize_in_transaction(&tx)?;
        return tx.commit().map_err(sql);
    }
    initialize_in_transaction(connection)
}

fn initialize_in_transaction(connection: &Connection) -> Result<(), String> {
    let established: bool = connection
        .query_row(
            "SELECT renewable_initialized FROM managed_registry_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(sql)?;
    let mut tables_present = 0;
    for table in TABLES {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .map_err(sql)?;
        tables_present += usize::from(exists);
    }
    if (established && tables_present != TABLES.len())
        || (tables_present != 0 && tables_present != TABLES.len())
    {
        return Err(
            "established renewable quota table is missing; refusing to recreate empty usage"
                .to_string(),
        );
    }
    if tables_present == TABLES.len() {
        let version: i64 = connection
            .query_row(
                "SELECT version FROM renewable_quota_metadata WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if version == 1 {
            // Attempts retain their original provisional anchor after a
            // wholly undispatched first-use schedule is abandoned.
            connection.execute_batch(
                "ALTER TABLE renewable_quota_attempts ADD COLUMN anchor TEXT NOT NULL DEFAULT '';
                 UPDATE renewable_quota_attempts SET anchor=COALESCE(
                    (SELECT anchor FROM renewable_quota_schedules s
                     WHERE s.api_key_id=renewable_quota_attempts.api_key_id), '');",
            ).map_err(sql)?;
            let missing_anchor: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM renewable_quota_attempts WHERE anchor='')",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if missing_anchor {
                return Err("existing quota attempt has no schedule anchor; refusing lossy accounting migration".to_string());
            }
            connection
                .execute(
                    "UPDATE renewable_quota_metadata SET version=2 WHERE singleton=1",
                    [],
                )
                .map_err(sql)?;
        } else if version != 2 {
            return Err(
                "unsupported renewable quota schema version; refusing accounting access"
                    .to_string(),
            );
        }
        if !established {
            connection.execute("UPDATE managed_registry_metadata SET renewable_initialized=1 WHERE singleton=1", []).map_err(sql)?;
        }
        return Ok(());
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS renewable_quota_metadata (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL
         );
         INSERT INTO renewable_quota_metadata(singleton,version) VALUES(1,2);
         CREATE TABLE IF NOT EXISTS renewable_quota_schedules (
            api_key_id TEXT PRIMARY KEY, anchor TEXT NOT NULL, high_water TEXT NOT NULL,
            shape TEXT NOT NULL, policy_json TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>0)
         );
         CREATE TABLE IF NOT EXISTS renewable_quota_windows (
            api_key_id TEXT NOT NULL, metric TEXT NOT NULL, period TEXT NOT NULL,
            window_start TEXT NOT NULL, window_end TEXT NOT NULL,
            confirmed INTEGER NOT NULL DEFAULT 0 CHECK(confirmed>=0),
            reserved INTEGER NOT NULL DEFAULT 0 CHECK(reserved>=0),
            uncertain INTEGER NOT NULL DEFAULT 0 CHECK(uncertain>=0),
            PRIMARY KEY(api_key_id,metric,period,window_start)
         );
         CREATE TABLE IF NOT EXISTS renewable_quota_requests (
            api_key_id TEXT NOT NULL, request_id TEXT NOT NULL, created_at TEXT NOT NULL,
            PRIMARY KEY(api_key_id,request_id)
         );
         CREATE TABLE IF NOT EXISTS renewable_quota_attempts (
            id TEXT PRIMARY KEY, api_key_id TEXT NOT NULL, request_id TEXT NOT NULL,
            bounds_json TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('active','settled','uncertain','released')),
            created_at TEXT NOT NULL, settled_at TEXT, anchor TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS renewable_quota_attempts_request
            ON renewable_quota_attempts(api_key_id,request_id,state);
         CREATE INDEX IF NOT EXISTS renewable_quota_attempts_stale
            ON renewable_quota_attempts(state,created_at);
         CREATE TABLE IF NOT EXISTS renewable_quota_holds (
            attempt_id TEXT NOT NULL REFERENCES renewable_quota_attempts(id),
            metric TEXT NOT NULL, period TEXT NOT NULL, window_start TEXT NOT NULL,
            window_end TEXT NOT NULL, amount INTEGER NOT NULL CHECK(amount>=0),
            confirmed INTEGER CHECK(confirmed>=0), uncertain INTEGER NOT NULL DEFAULT 0 CHECK(uncertain>=0),
            PRIMARY KEY(attempt_id,metric,period)
         );
         CREATE TABLE IF NOT EXISTS renewable_quota_ledger (
            id INTEGER PRIMARY KEY AUTOINCREMENT, occurred_at TEXT NOT NULL,
            api_key_id TEXT NOT NULL, attempt_id TEXT, kind TEXT NOT NULL,
            details_json TEXT NOT NULL
         );"
    ).map_err(sql)?;
    connection
        .execute(
            "UPDATE managed_registry_metadata SET renewable_initialized=1 WHERE singleton=1",
            [],
        )
        .map_err(sql)?;
    Ok(())
}

fn open(path: &Path) -> Result<Connection, String> {
    let connection = crate::api_key_policy_store::open_connection_at_path(path)?;
    initialize(&connection)?;
    Ok(connection)
}

/// Called within the registry mutation transaction. Limits can change without
/// erasing spend. Deactivation preserves the schedule and every historical hold.
pub(crate) fn validate_policy_change(
    tx: &Transaction<'_>,
    key_id: &str,
    _old: Option<&QuotaPolicy>,
    new: Option<&QuotaPolicy>,
) -> Result<(), String> {
    initialize(tx)?;
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT shape,policy_json FROM renewable_quota_schedules WHERE api_key_id=?1",
            [key_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql)?;
    let Some((shape, old_json)) = prior else {
        return Ok(());
    };
    let policy_json = if let Some(policy) = new {
        if shape != policy.shape()? {
            return Err("an activated quota's timezone and metric/period rules cannot change; change limits or explicitly migrate the schedule instead".to_string());
        }
        serde_json::to_string(&policy.normalized()?).map_err(|error| error.to_string())?
    } else {
        "null".to_string()
    };
    if policy_json != old_json {
        if tx.execute("UPDATE renewable_quota_schedules SET policy_json=?2,revision=revision+1 WHERE api_key_id=?1 AND revision < 9223372036854775807", params![key_id, policy_json]).map_err(sql)? != 1 {
            return Err("quota policy revision overflow; refusing an unauditable policy edit".to_string());
        }
        tx.execute("INSERT INTO renewable_quota_ledger(occurred_at,api_key_id,kind,details_json) VALUES(?1,?2,'policy_changed',?3)", params![stamp(Utc::now()), key_id, policy_json]).map_err(sql)?;
    }
    Ok(())
}

fn current_key(
    connection: &Connection,
    key_id: &str,
) -> Result<Option<crate::api_keys::ApiKeyRecord>, String> {
    let json: Option<String> = connection
        .query_row(
            "SELECT record_json FROM managed_api_keys WHERE id=?1",
            [key_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)?;
    json.map(|json| {
        let record: crate::api_keys::ApiKeyRecord = serde_json::from_str(&json)
            .map_err(|_| "invalid authoritative API-key record".to_string())?;
        if record.id != key_id {
            return Err("authoritative API-key record identity mismatch".to_string());
        }
        Ok(record)
    })
    .transpose()
}

fn check_current_access(
    record: &crate::api_keys::ApiKeyRecord,
    request: &ReserveRequest,
) -> Result<Option<Denial>, String> {
    if record.revoked_at.is_some() {
        return Ok(Some(Denial::policy(
            "key_revoked",
            "API key has been revoked",
        )));
    }
    let access = record.access.normalized()?;
    if access.input_token_budget != request.expected_legacy_budget {
        return Ok(Some(Denial::policy(
            "api_key_policy_changed",
            "API-key input budget changed during admission; retry the request under the current policy",
        )));
    }
    let mut cap = access.prompt_token_limit;
    if !access.all {
        let Some(rule) = request
            .provider
            .as_deref()
            .and_then(|provider| access.provider_rule(provider))
        else {
            return Ok(Some(Denial::policy(
                "scope_denied",
                "API key does not permit this provider",
            )));
        };
        if !rule.account_scope.is_all()
            && !request
                .account_key
                .as_deref()
                .is_some_and(|account| rule.accounts.iter().any(|allowed| allowed == account))
        {
            return Ok(Some(Denial::policy(
                "scope_denied",
                "API key does not permit this account",
            )));
        }
        if let Some(limit) = rule.prompt_token_limit {
            cap = Some(cap.map_or(limit, |previous| previous.min(limit)));
        }
        for limit in &rule.account_limits {
            if request.account_key.as_deref() == Some(limit.account.as_str()) {
                if let Some(limit) = limit.prompt_token_limit {
                    cap = Some(cap.map_or(limit, |previous| previous.min(limit)));
                }
            }
        }
    }
    if let Some(cap) = cap {
        match request.bounds.input_tokens {
            Some(amount) if amount <= cap => {}
            Some(_) => {
                return Ok(Some(Denial::policy(
                    "token_limit_exceeded",
                    "Final prepared request exceeds the current API-key input-token cap",
                )))
            }
            None => {
                return Ok(Some(Denial::policy(
                    "prompt_measurement_required",
                    "Current API-key policy requires a safely measurable input",
                )))
            }
        }
    }
    Ok(None)
}

struct PlannedHold {
    rule: QuotaRule,
    window: Window,
    amount: u64,
}

/// Only a genuinely unused key (or a wholly released provisional first use)
/// may establish a new schedule. Losing one schedule row must not reset intact
/// historical attempts/balances even when every schema table still exists.
fn validate_unanchored_key(connection: &Connection, key: &str) -> Result<(), String> {
    let has_accounting: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM renewable_quota_attempts
                WHERE api_key_id=?1 AND state!='released')
             OR EXISTS(SELECT 1 FROM renewable_quota_windows
                WHERE api_key_id=?1 AND (confirmed!=0 OR reserved!=0 OR uncertain!=0))",
            [key],
            |row| row.get(0),
        )
        .map_err(sql)?;
    if has_accounting {
        return Err(
            "established quota schedule is missing; refusing to reset persisted usage".to_string(),
        );
    }
    Ok(())
}

/// Returns only after every applicable hold is durable. No network operation
/// belongs inside this transaction. Existing attempts are replayed, not charged
/// again; callers MUST NOT dispatch again when Reservation::replay is true.
pub(crate) fn reserve_at_path(
    path: &Path,
    request: &ReserveRequest,
    now: DateTime<Utc>,
) -> Result<Admission, String> {
    for (label, value) in [
        ("key", &request.api_key_id),
        ("request", &request.request_id),
        ("attempt", &request.attempt_id),
    ] {
        if value.is_empty() || value.len() > 512 {
            return Err(format!("quota {} ID must contain 1 to 512 bytes", label));
        }
    }
    request.bounds.validate()?;
    let mut connection = open(path)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let Some(record) = current_key(&tx, &request.api_key_id)? else {
        return Ok(Admission::Denied(Denial::policy(
            "key_revoked",
            "API key is no longer registered",
        )));
    };
    if let Some(denial) = check_current_access(&record, request)? {
        return Ok(Admission::Denied(denial));
    }
    let bounds_json = serde_json::to_string(&request.bounds).map_err(|error| error.to_string())?;
    let prior_attempt: Option<(String, String, String, String, String)> = tx.query_row("SELECT api_key_id,request_id,bounds_json,state,anchor FROM renewable_quota_attempts WHERE id=?1", [&request.attempt_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).optional().map_err(sql)?;
    if let Some((key_id, request_id, prior_bounds, state, anchor)) = prior_attempt {
        if key_id != request.api_key_id
            || request_id != request.request_id
            || prior_bounds != bounds_json
        {
            return Err("quota attempt ID was reused with different execution details".to_string());
        }
        parse_stamp(&anchor)?;
        return Ok(Admission::Reserved(Reservation {
            id: request.attempt_id.clone(),
            api_key_id: key_id,
            request_id,
            anchor,
            replay: true,
            state,
        }));
    }
    let Some(policy) = record.access.quota.as_ref() else {
        return Ok(Admission::NotConfigured);
    };
    let policy = policy.normalized()?;
    let schedule: Option<(String, String, String)> = tx
        .query_row(
            "SELECT anchor,high_water,shape FROM renewable_quota_schedules WHERE api_key_id=?1",
            [&request.api_key_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql)?;
    let (anchor, effective_now) = match &schedule {
        Some((anchor, high_water, shape)) => {
            if *shape != policy.shape()? {
                return Err(
                    "quota policy shape changed without an explicit schedule migration".to_string(),
                );
            }
            (parse_stamp(anchor)?, now.max(parse_stamp(high_water)?))
        }
        None => {
            validate_unanchored_key(&tx, &request.api_key_id)?;
            (now, now)
        }
    };
    let counted_request = tx
        .query_row(
            "SELECT 1 FROM renewable_quota_requests WHERE api_key_id=?1 AND request_id=?2",
            params![request.api_key_id, request.request_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql)?
        .is_some();
    let mut planned = Vec::with_capacity(policy.rules.len());
    for rule in &policy.rules {
        let amount = if rule.metric == QuotaMetric::Requests {
            if counted_request {
                0
            } else {
                1
            }
        } else {
            let Some(bound) = request.bounds.get(rule.metric) else {
                let mut denial = Denial::policy(
                    "quota_measurement_required",
                    "This provider/request cannot guarantee a bound for an active strict quota",
                );
                denial.metric = Some(rule.metric);
                denial.period = Some(rule.period);
                return Ok(Admission::Denied(denial));
            };
            bound
        };
        checked_amount(amount)?;
        let window = window_at(anchor, effective_now, rule.period, &policy.timezone)?;
        let (confirmed, reserved, uncertain) = window_balances(
            &tx,
            &request.api_key_id,
            rule.metric,
            rule.period,
            &stamp(window.start),
        )?;
        let used = confirmed
            .checked_add(reserved)
            .and_then(|value| value.checked_add(uncertain))
            .ok_or("quota balance overflow")?;
        let remaining = rule.limit.saturating_sub(used);
        // A zero additional request charge is permitted across retry attempts,
        // even when this request itself filled its request-count allowance.
        if amount > remaining || (used > rule.limit && amount > 0) {
            return Ok(Admission::Denied(Denial {
                code: "quota_exceeded".to_string(),
                message: format!(
                    "{} {} quota exhausted",
                    rule.period.as_str(),
                    rule.metric.as_str()
                ),
                metric: Some(rule.metric),
                period: Some(rule.period),
                limit: Some(rule.limit),
                remaining: Some(remaining),
                reset_at: Some(stamp(window.end)),
                retry_after_seconds: Some(
                    window
                        .end
                        .signed_duration_since(now)
                        .num_milliseconds()
                        .max(0)
                        .div_euclid(1000) as u64
                        + 1,
                ),
            }));
        }
        planned.push(PlannedHold {
            rule: rule.clone(),
            window,
            amount,
        });
    }
    let policy_json = serde_json::to_string(&policy).map_err(|error| error.to_string())?;
    if schedule.is_none() {
        tx.execute("INSERT INTO renewable_quota_schedules(api_key_id,anchor,high_water,shape,policy_json,revision) VALUES(?1,?2,?3,?4,?5,1)", params![request.api_key_id, stamp(anchor), stamp(effective_now), policy.shape()?, policy_json]).map_err(sql)?;
    } else {
        tx.execute(
            "UPDATE renewable_quota_schedules SET high_water=?2 WHERE api_key_id=?1",
            params![request.api_key_id, stamp(effective_now)],
        )
        .map_err(sql)?;
    }
    tx.execute("INSERT OR IGNORE INTO renewable_quota_requests(api_key_id,request_id,created_at) VALUES(?1,?2,?3)", params![request.api_key_id, request.request_id, stamp(effective_now)]).map_err(sql)?;
    tx.execute("INSERT INTO renewable_quota_attempts(id,api_key_id,request_id,bounds_json,state,created_at,anchor) VALUES(?1,?2,?3,?4,'active',?5,?6)", params![request.attempt_id, request.api_key_id, request.request_id, bounds_json, stamp(effective_now), stamp(anchor)]).map_err(sql)?;
    for hold in planned {
        let start = stamp(hold.window.start);
        tx.execute("INSERT INTO renewable_quota_windows(api_key_id,metric,period,window_start,window_end,reserved) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(api_key_id,metric,period,window_start) DO UPDATE SET reserved=reserved+excluded.reserved,window_end=excluded.window_end", params![request.api_key_id, hold.rule.metric.as_str(), hold.rule.period.as_str(), start, stamp(hold.window.end), checked_amount(hold.amount)?]).map_err(sql)?;
        tx.execute("INSERT INTO renewable_quota_holds(attempt_id,metric,period,window_start,window_end,amount) VALUES(?1,?2,?3,?4,?5,?6)", params![request.attempt_id, hold.rule.metric.as_str(), hold.rule.period.as_str(), start, stamp(hold.window.end), checked_amount(hold.amount)?]).map_err(sql)?;
    }
    tx.execute("INSERT INTO renewable_quota_ledger(occurred_at,api_key_id,attempt_id,kind,details_json) VALUES(?1,?2,?3,'reserved',?4)", params![stamp(effective_now), request.api_key_id, request.attempt_id, bounds_json]).map_err(sql)?;
    tx.commit().map_err(sql)?;
    Ok(Admission::Reserved(Reservation {
        id: request.attempt_id.clone(),
        api_key_id: request.api_key_id.clone(),
        request_id: request.request_id.clone(),
        anchor: stamp(anchor),
        replay: false,
        state: "active".to_string(),
    }))
}

fn window_balances(
    connection: &Connection,
    key: &str,
    metric: QuotaMetric,
    period: QuotaPeriod,
    start: &str,
) -> Result<(u64, u64, u64), String> {
    let values = connection.query_row("SELECT confirmed,reserved,uncertain FROM renewable_quota_windows WHERE api_key_id=?1 AND metric=?2 AND period=?3 AND window_start=?4", params![key, metric.as_str(), period.as_str(), start], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))).optional().map_err(sql)?.unwrap_or((0, 0, 0));
    Ok((
        nonnegative(values.0)?,
        nonnegative(values.1)?,
        nonnegative(values.2)?,
    ))
}

#[derive(Debug)]
struct StoredHold {
    metric: QuotaMetric,
    period: QuotaPeriod,
    start: String,
    amount: u64,
    confirmed: Option<u64>,
    uncertain: u64,
}

fn decode_enum<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .map_err(|_| "invalid persisted quota metric/period".to_string())
}

pub(crate) fn settle_at_path(
    path: &Path,
    id: &str,
    settlement: Settlement,
    now: DateTime<Utc>,
) -> Result<SettlementResult, String> {
    if let Settlement::Reported(usage) = &settlement {
        usage.validate()?;
    }
    let mut connection = open(path)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let result = settle_in_transaction(&tx, id, &settlement, now)?;
    tx.commit().map_err(sql)?;
    Ok(result)
}

fn settle_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    settlement: &Settlement,
    now: DateTime<Utc>,
) -> Result<SettlementResult, String> {
    let attempt: Option<(String, String, String)> = tx
        .query_row(
            "SELECT api_key_id,request_id,state FROM renewable_quota_attempts WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql)?;
    let Some((key, request, state)) = attempt else {
        return Err(
            "unknown quota reservation; refusing to manufacture accounting state".to_string(),
        );
    };
    if state == "released" {
        return if matches!(settlement, Settlement::Released) {
            Ok(SettlementResult {
                reservation_id: id.to_string(),
                changed: false,
                state,
            })
        } else {
            Err("cannot charge a reservation previously released as undispatched".to_string())
        };
    }
    if state != "active" && matches!(settlement, Settlement::Released) {
        return Err(
            "cannot release a settled/uncertain upstream attempt without authoritative usage"
                .to_string(),
        );
    }
    let mut statement = tx.prepare("SELECT metric,period,window_start,amount,confirmed,uncertain FROM renewable_quota_holds WHERE attempt_id=?1 ORDER BY metric,period").map_err(sql)?;
    let rows = statement
        .query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(sql)?;
    let mut holds = Vec::new();
    for row in rows {
        let (metric, period, start, amount, confirmed, uncertain) = row.map_err(sql)?;
        holds.push(StoredHold {
            metric: decode_enum(&metric)?,
            period: decode_enum(&period)?,
            start,
            amount: nonnegative(amount)?,
            confirmed: confirmed.map(nonnegative).transpose()?,
            uncertain: nonnegative(uncertain)?,
        });
    }
    drop(statement);
    let other_attempt = tx.query_row("SELECT 1 FROM renewable_quota_attempts WHERE api_key_id=?1 AND request_id=?2 AND id!=?3 AND state!='released' LIMIT 1", params![key, request, id], |_| Ok(())).optional().map_err(sql)?.is_some();
    let mut changed = state == "active";
    let mut has_unknown = false;
    for hold in holds {
        let proposed = if hold.metric == QuotaMetric::Requests {
            Some(
                if matches!(settlement, Settlement::Released) && !other_attempt {
                    0
                } else {
                    hold.amount
                },
            )
        } else {
            match settlement {
                Settlement::Reported(usage) => usage.get(hold.metric),
                Settlement::Unknown => None,
                Settlement::Released => Some(0),
            }
        };
        let confirmed = match (hold.confirmed, proposed) {
            (Some(old), Some(new)) if old != new => {
                return Err(
                    "conflicting final usage for an already settled quota component".to_string(),
                )
            }
            (Some(old), _) => Some(old),
            (None, value) => value,
        };
        let uncertain = if confirmed.is_none() { hold.amount } else { 0 };
        has_unknown |= confirmed.is_none();
        if confirmed != hold.confirmed || uncertain != hold.uncertain {
            changed = true;
        }
        let (old_confirmed, old_reserved, old_uncertain) =
            window_balances(tx, &key, hold.metric, hold.period, &hold.start)?;
        let new_confirmed = old_confirmed
            .checked_sub(hold.confirmed.unwrap_or(0))
            .and_then(|amount| amount.checked_add(confirmed.unwrap_or(0)))
            .ok_or("quota confirmed accounting overflow")?;
        let new_reserved = old_reserved
            .checked_sub(if state == "active" { hold.amount } else { 0 })
            .ok_or("quota reservation accounting underflow")?;
        let new_uncertain = old_uncertain
            .checked_sub(hold.uncertain)
            .and_then(|amount| amount.checked_add(uncertain))
            .ok_or("quota uncertain accounting overflow")?;
        tx.execute("UPDATE renewable_quota_windows SET confirmed=?5,reserved=?6,uncertain=?7 WHERE api_key_id=?1 AND metric=?2 AND period=?3 AND window_start=?4", params![key, hold.metric.as_str(), hold.period.as_str(), hold.start, checked_amount(new_confirmed)?, checked_amount(new_reserved)?, checked_amount(new_uncertain)?]).map_err(sql)?;
        tx.execute("UPDATE renewable_quota_holds SET confirmed=?4,uncertain=?5 WHERE attempt_id=?1 AND metric=?2 AND period=?3", params![id, hold.metric.as_str(), hold.period.as_str(), confirmed.map(checked_amount).transpose()?, checked_amount(uncertain)?]).map_err(sql)?;
    }
    let new_state = if matches!(settlement, Settlement::Released) {
        "released"
    } else if has_unknown {
        "uncertain"
    } else {
        "settled"
    };
    if matches!(settlement, Settlement::Released) && !other_attempt {
        // The first attempt owned the single logical-request charge. It may
        // already have been released while a concurrent retry still held it
        // alive. Once every attempt is proven undispatched, refund that charge
        // in its original window too, regardless of release order.
        release_prior_request_charges(tx, &key, &request, id)?;
        tx.execute(
            "DELETE FROM renewable_quota_requests WHERE api_key_id=?1 AND request_id=?2",
            params![key, request],
        )
        .map_err(sql)?;
    }
    if changed {
        tx.execute(
            "UPDATE renewable_quota_attempts SET state=?2,settled_at=?3 WHERE id=?1",
            params![id, new_state, stamp(now)],
        )
        .map_err(sql)?;
        let details = match settlement {
            Settlement::Reported(usage) => {
                serde_json::to_string(usage).map_err(|error| error.to_string())?
            }
            _ => "{}".to_string(),
        };
        tx.execute("INSERT INTO renewable_quota_ledger(occurred_at,api_key_id,attempt_id,kind,details_json) VALUES(?1,?2,?3,?4,?5)", params![stamp(now), key, id, new_state, details]).map_err(sql)?;
        if matches!(settlement, Settlement::Released) {
            abandon_wholly_undispatched_anchor(tx, &key, now)?;
        }
    }
    Ok(SettlementResult {
        reservation_id: id.to_string(),
        changed,
        state: if changed {
            new_state.to_string()
        } else {
            state
        },
    })
}

fn abandon_wholly_undispatched_anchor(
    tx: &Transaction<'_>,
    key: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let ever_possibly_dispatched: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM renewable_quota_attempts WHERE api_key_id=?1 AND state!='released')",
        [key], |row| row.get(0),
    ).map_err(sql)?;
    if ever_possibly_dispatched {
        return Ok(());
    }
    let has_charge: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM renewable_quota_windows WHERE api_key_id=?1 AND (confirmed!=0 OR reserved!=0 OR uncertain!=0))",
        [key], |row| row.get(0),
    ).map_err(sql)?;
    if has_charge {
        return Err(
            "wholly undispatched quota schedule retains charges; refusing to reset its anchor"
                .to_string(),
        );
    }
    let anchor: Option<String> = tx
        .query_row(
            "SELECT anchor FROM renewable_quota_schedules WHERE api_key_id=?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)?;
    if let Some(anchor) = anchor {
        tx.execute(
            "DELETE FROM renewable_quota_schedules WHERE api_key_id=?1",
            [key],
        )
        .map_err(sql)?;
        tx.execute(
            "INSERT INTO renewable_quota_ledger(occurred_at,api_key_id,kind,details_json) VALUES(?1,?2,'unused_anchor_released',?3)",
            params![stamp(now), key, serde_json::json!({"anchor": anchor}).to_string()],
        ).map_err(sql)?;
    }
    Ok(())
}

fn release_prior_request_charges(
    tx: &Transaction<'_>,
    key: &str,
    request: &str,
    current: &str,
) -> Result<(), String> {
    let mut statement = tx
        .prepare(
            "SELECT h.attempt_id,h.period,h.window_start,h.confirmed,h.uncertain
         FROM renewable_quota_holds h JOIN renewable_quota_attempts a ON a.id=h.attempt_id
         WHERE a.api_key_id=?1 AND a.request_id=?2 AND a.id!=?3
         AND a.state='released' AND h.metric='requests'",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map(params![key, request, current], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(sql)?;
    let prior = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?;
    drop(statement);
    for (attempt, period, start, charge, uncertainty) in prior {
        let period: QuotaPeriod = decode_enum(&period)?;
        let (confirmed, _, uncertain) =
            window_balances(tx, key, QuotaMetric::Requests, period, &start)?;
        let confirmed = confirmed
            .checked_sub(nonnegative(charge.unwrap_or(0))?)
            .ok_or("quota request refund underflow")?;
        let uncertain = uncertain
            .checked_sub(nonnegative(uncertainty)?)
            .ok_or("quota request uncertainty underflow")?;
        tx.execute("UPDATE renewable_quota_windows SET confirmed=?4,uncertain=?5 WHERE api_key_id=?1 AND metric='requests' AND period=?2 AND window_start=?3", params![key, period.as_str(), start, checked_amount(confirmed)?, checked_amount(uncertain)?]).map_err(sql)?;
        tx.execute("UPDATE renewable_quota_holds SET confirmed=0,uncertain=0 WHERE attempt_id=?1 AND metric='requests' AND period=?2", params![attempt, period.as_str()]).map_err(sql)?;
    }
    Ok(())
}

/// Abandoned holds become uncertainty, never new spendable allowance. A later
/// complete provider report can reconcile an uncertain attempt exactly once.
pub(crate) fn recover_at_path(
    path: &Path,
    cutoff: DateTime<Utc>,
    now: DateTime<Utc>,
    limit: usize,
) -> Result<RecoveryResult, String> {
    let mut connection = open(path)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let ids: Vec<String> = {
        let mut statement = tx.prepare("SELECT id FROM renewable_quota_attempts WHERE state='active' AND created_at<=?1 ORDER BY created_at,id LIMIT ?2").map_err(sql)?;
        let rows = statement
            .query_map(
                params![
                    stamp(cutoff),
                    i64::try_from(limit.min(10_000)).unwrap_or(10_000)
                ],
                |row| row.get(0),
            )
            .map_err(sql)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?
    };
    let mut result = RecoveryResult {
        examined: ids.len(),
        recovered: 0,
    };
    for id in ids {
        if settle_in_transaction(&tx, &id, &Settlement::Unknown, now)?.changed {
            result.recovered += 1;
        }
    }
    tx.commit().map_err(sql)?;
    Ok(result)
}

pub(crate) fn summary_at_path(
    path: &Path,
    key_id: &str,
    now: DateTime<Utc>,
) -> Result<Option<QuotaSummary>, String> {
    let mut connection = open(path)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(sql)?;
    let Some(record) = current_key(&tx, key_id)? else {
        return Ok(None);
    };
    let Some(policy) = record.access.quota else {
        return Ok(None);
    };
    let policy = policy.normalized()?;
    let schedule: Option<(String, String, i64)> = tx
        .query_row(
            "SELECT anchor,high_water,revision FROM renewable_quota_schedules WHERE api_key_id=?1",
            [key_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql)?;
    let (anchor, effective, revision) = if let Some((anchor, high_water, revision)) = schedule {
        (
            Some(parse_stamp(&anchor)?),
            now.max(parse_stamp(&high_water)?),
            nonnegative(revision)?,
        )
    } else {
        validate_unanchored_key(&tx, key_id)?;
        (None, now, 0)
    };
    let mut rules = Vec::with_capacity(policy.rules.len());
    for rule in policy.rules {
        let window = anchor
            .map(|anchor| window_at(anchor, effective, rule.period, &policy.timezone))
            .transpose()?;
        let (confirmed, reserved, uncertain) = if let Some(window) = &window {
            window_balances(&tx, key_id, rule.metric, rule.period, &stamp(window.start))?
        } else {
            (0, 0, 0)
        };
        let used = confirmed
            .checked_add(reserved)
            .and_then(|value| value.checked_add(uncertain))
            .ok_or("quota balance overflow")?;
        rules.push(RuleSummary {
            metric: rule.metric,
            period: rule.period,
            limit: rule.limit,
            window_start: window.as_ref().map(|window| stamp(window.start)),
            reset_at: window.as_ref().map(|window| stamp(window.end)),
            confirmed,
            reserved,
            uncertain,
            remaining: rule.limit.saturating_sub(used),
        });
    }
    tx.commit().map_err(sql)?;
    Ok(Some(QuotaSummary {
        timezone: policy.timezone,
        anchor: anchor.map(stamp),
        revision,
        rules,
    }))
}

pub(crate) fn reserve(cfg: &crate::Config, request: &ReserveRequest) -> Result<Admission, String> {
    reserve_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        request,
        Utc::now(),
    )
}

pub(crate) fn settle(
    cfg: &crate::Config,
    id: &str,
    settlement: Settlement,
) -> Result<SettlementResult, String> {
    settle_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        id,
        settlement,
        Utc::now(),
    )
}

pub(crate) fn summary(cfg: &crate::Config, key_id: &str) -> Result<Option<QuotaSummary>, String> {
    summary_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        key_id,
        Utc::now(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::{Arc, Barrier},
    };

    struct Fixture {
        directory: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new(policy: Option<QuotaPolicy>) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "io-gateway-renewable-quota-test-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let fixture = Self {
                path: directory.join("api-key-policy.sqlite3"),
                directory,
            };
            let record = crate::api_keys::ApiKeyRecord {
                id: "key-1".to_string(),
                access: crate::api_keys::ApiKeyAccess {
                    quota: policy,
                    ..Default::default()
                },
                ..Default::default()
            };
            fixture.write_record(&record);
            fixture
        }
        fn write_record(&self, record: &crate::api_keys::ApiKeyRecord) {
            let connection = open(&self.path).unwrap();
            connection.execute("INSERT INTO managed_api_keys(id,record_json) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record_json=excluded.record_json", params![record.id, serde_json::to_string(record).unwrap()]).unwrap();
        }
        fn record(&self) -> crate::api_keys::ApiKeyRecord {
            current_key(&open(&self.path).unwrap(), "key-1")
                .unwrap()
                .unwrap()
        }
        fn summary(&self, now: &str) -> QuotaSummary {
            summary_at_path(&self.path, "key-1", at(now))
                .unwrap()
                .unwrap()
        }
        fn reserve(&self, id: &str, amount: u64, now: &str) -> Admission {
            reserve_at_path(&self.path, &request(id, amount), at(now)).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }
    fn rule(metric: QuotaMetric, period: QuotaPeriod, limit: u64) -> QuotaRule {
        QuotaRule {
            metric,
            period,
            limit,
        }
    }
    fn policy(rules: Vec<QuotaRule>) -> QuotaPolicy {
        QuotaPolicy {
            timezone: "UTC".to_string(),
            rules,
        }
    }
    fn request(id: &str, input: u64) -> ReserveRequest {
        ReserveRequest {
            api_key_id: "key-1".to_string(),
            request_id: format!("request-{id}"),
            attempt_id: id.to_string(),
            provider: Some("claude".to_string()),
            account_key: Some("claude:canonical-account".to_string()),
            expected_legacy_budget: None,
            bounds: Usage {
                input_tokens: Some(input),
                uncached_input_tokens: Some(input),
                output_tokens: Some(10),
                cache_read_tokens: Some(input),
                cache_write_tokens: Some(input),
                cache_tokens: Some(input),
                ..Default::default()
            },
        }
    }
    fn reserved(value: Admission) -> Reservation {
        match value {
            Admission::Reserved(value) => value,
            other => panic!("expected reservation, got {other:?}"),
        }
    }

    #[test]
    fn policy_validation_is_strict_and_order_independent() {
        assert!(policy(vec![]).normalized().is_err());
        assert!(
            policy(vec![rule(QuotaMetric::Requests, QuotaPeriod::Daily, 0)])
                .normalized()
                .is_err()
        );
        assert!(policy(vec![rule(
            QuotaMetric::InputTokens,
            QuotaPeriod::Monthly,
            u64::MAX
        )])
        .normalized()
        .is_err());
        let duplicate = rule(QuotaMetric::Requests, QuotaPeriod::Daily, 10);
        assert!(policy(vec![duplicate.clone(), duplicate])
            .normalized()
            .is_err());
        let mut bad_zone = policy(vec![rule(QuotaMetric::Requests, QuotaPeriod::Weekly, 10)]);
        bad_zone.timezone = "not/a_timezone".to_string();
        assert!(bad_zone.normalized().is_err());
        let a = policy(vec![
            rule(QuotaMetric::Requests, QuotaPeriod::Daily, 1),
            rule(QuotaMetric::InputTokens, QuotaPeriod::Monthly, 50),
        ]);
        let b = policy(vec![
            rule(QuotaMetric::InputTokens, QuotaPeriod::Monthly, 50),
            rule(QuotaMetric::Requests, QuotaPeriod::Daily, 1),
        ]);
        assert_eq!(a.normalized().unwrap(), b.normalized().unwrap());
        assert!(a.needs_input_measurement());
        assert!(!a.needs_output_bound());
        assert!(serde_json::from_str::<QuotaPolicy>(
            r#"{"rules":[{"metric":"requests","period":"daily","limit":1}],"typo":true}"#
        )
        .is_err());
    }

    #[test]
    fn weekly_window_is_first_use_anchored_half_open_and_preserves_idle_cadence() {
        let anchor = at("2026-09-15T15:00:00+07:00");
        let before = window_at(
            anchor,
            at("2026-09-22T14:59:59.999+07:00"),
            QuotaPeriod::Weekly,
            "Asia/Jakarta",
        )
        .unwrap();
        assert_eq!(before.start, anchor);
        assert_eq!(before.end, at("2026-09-22T15:00:00+07:00"));
        let boundary = window_at(anchor, before.end, QuotaPeriod::Weekly, "Asia/Jakarta").unwrap();
        assert_eq!(boundary.start, before.end);
        let idle = window_at(
            anchor,
            at("2026-10-07T11:00:00+07:00"),
            QuotaPeriod::Weekly,
            "Asia/Jakarta",
        )
        .unwrap();
        assert_eq!(idle.start, at("2026-10-06T15:00:00+07:00"));
        assert_eq!(idle.end, at("2026-10-13T15:00:00+07:00"));
    }

    #[test]
    fn monthly_boundaries_use_original_day_and_handle_leap_year() {
        for (year, feb_end) in [(2024, 29), (2025, 28)] {
            let anchor = at(&format!("{year}-01-31T15:00:00+07:00"));
            let feb = window_at(
                anchor,
                at(&format!("{year}-02-{feb_end}T15:00:00+07:00")),
                QuotaPeriod::Monthly,
                "Asia/Jakarta",
            )
            .unwrap();
            assert_eq!(
                feb.start,
                at(&format!("{year}-02-{feb_end}T15:00:00+07:00"))
            );
            assert_eq!(feb.end, at(&format!("{year}-03-31T15:00:00+07:00")));
            let march = window_at(anchor, feb.end, QuotaPeriod::Monthly, "Asia/Jakarta").unwrap();
            assert_eq!(march.end, at(&format!("{year}-04-30T15:00:00+07:00")));
        }
    }

    #[test]
    fn monthly_dst_gap_advances_and_overlap_chooses_earlier_instant() {
        let gap_anchor = at("2026-02-08T02:30:00-05:00");
        let gap = window_at(
            gap_anchor,
            gap_anchor,
            QuotaPeriod::Monthly,
            "America/New_York",
        )
        .unwrap();
        assert_eq!(gap.end, at("2026-03-08T03:00:00-04:00"));
        let overlap_anchor = at("2026-10-01T01:30:00-04:00");
        let overlap = window_at(
            overlap_anchor,
            overlap_anchor,
            QuotaPeriod::Monthly,
            "America/New_York",
        )
        .unwrap();
        assert_eq!(overlap.end, at("2026-11-01T01:30:00-04:00"));
        let daily = window_at(
            gap_anchor,
            gap_anchor,
            QuotaPeriod::Daily,
            "America/New_York",
        )
        .unwrap();
        assert_eq!(
            daily.end.signed_duration_since(daily.start),
            Duration::hours(24)
        );
    }

    #[test]
    fn rejected_first_request_creates_no_anchor_or_partial_holds() {
        let fixture = Fixture::new(Some(policy(vec![
            rule(QuotaMetric::InputTokens, QuotaPeriod::Weekly, 100),
            rule(QuotaMetric::OutputTokens, QuotaPeriod::Daily, 9),
            rule(QuotaMetric::Requests, QuotaPeriod::Daily, 10),
        ])));
        assert!(matches!(
            fixture.reserve("first", 50, "2026-09-15T08:00:00Z"),
            Admission::Denied(Denial {
                metric: Some(QuotaMetric::OutputTokens),
                ..
            })
        ));
        let summary = fixture.summary("2026-09-15T08:00:00Z");
        assert_eq!(summary.anchor, None);
        assert!(summary
            .rules
            .iter()
            .all(|rule| rule.confirmed == 0 && rule.reserved == 0 && rule.uncertain == 0));
        let connection = open(&fixture.path).unwrap();
        for table in [
            "renewable_quota_attempts",
            "renewable_quota_requests",
            "renewable_quota_windows",
            "renewable_quota_ledger",
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }

    #[test]
    fn missing_bound_is_not_zero_and_does_not_start_a_window() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::CacheTokens,
            QuotaPeriod::Monthly,
            100,
        )])));
        let mut request = request("unknown-cache", 50);
        request.bounds.cache_tokens = None;
        request.bounds.cache_read_tokens = None;
        assert!(
            matches!(reserve_at_path(&fixture.path, &request, at("2026-01-01T00:00:00Z")).unwrap(), Admission::Denied(Denial { code, .. }) if code == "quota_measurement_required")
        );
        assert_eq!(fixture.summary("2026-01-01T00:00:00Z").anchor, None);
        request.bounds.cache_tokens = Some(0);
        reserved(reserve_at_path(&fixture.path, &request, at("2026-01-02T00:00:00Z")).unwrap());
        assert_eq!(
            fixture.summary("2026-01-02T00:00:00Z").anchor,
            Some(stamp(at("2026-01-02T00:00:00Z")))
        );
    }

    #[test]
    fn final_report_refunds_unused_upper_bound_and_duplicate_settlement_is_idempotent() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::InputTokens,
            QuotaPeriod::Weekly,
            100,
        )])));
        let id = reserved(fixture.reserve("one", 80, "2026-09-15T08:00:00Z")).id;
        assert_eq!(
            fixture.summary("2026-09-15T08:00:00Z").rules[0].remaining,
            20
        );
        let settlement = Settlement::Reported(Usage {
            input_tokens: Some(35),
            ..Default::default()
        });
        assert!(
            settle_at_path(
                &fixture.path,
                &id,
                settlement.clone(),
                at("2026-09-15T08:00:01Z")
            )
            .unwrap()
            .changed
        );
        assert!(
            !settle_at_path(&fixture.path, &id, settlement, at("2026-09-15T08:00:02Z"))
                .unwrap()
                .changed
        );
        let summary = fixture.summary("2026-09-15T08:00:03Z");
        assert_eq!(
            (
                summary.rules[0].confirmed,
                summary.rules[0].reserved,
                summary.rules[0].remaining
            ),
            (35, 0, 65)
        );
        assert!(settle_at_path(
            &fixture.path,
            &id,
            Settlement::Reported(Usage {
                input_tokens: Some(34),
                ..Default::default()
            }),
            at("2026-09-15T08:00:04Z")
        )
        .is_err());
    }

    #[test]
    fn reported_overage_is_recorded_not_clamped_or_refunded() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::OutputTokens,
            QuotaPeriod::Daily,
            15,
        )])));
        reserved(fixture.reserve("one", 1, "2026-09-15T08:00:00Z"));
        settle_at_path(
            &fixture.path,
            "one",
            Settlement::Reported(Usage {
                output_tokens: Some(20),
                ..Default::default()
            }),
            at("2026-09-15T08:01:00Z"),
        )
        .unwrap();
        assert_eq!(
            fixture.summary("2026-09-15T08:01:00Z").rules[0].confirmed,
            20
        );
        assert_eq!(
            fixture.summary("2026-09-15T08:01:00Z").rules[0].remaining,
            0
        );
        assert!(matches!(
            fixture.reserve("two", 1, "2026-09-15T08:02:00Z"),
            Admission::Denied(_)
        ));
    }

    #[test]
    fn unknown_usage_and_crash_recovery_retain_allowance_then_reconcile() {
        let fixture = Fixture::new(Some(policy(vec![
            rule(QuotaMetric::InputTokens, QuotaPeriod::Daily, 100),
            rule(QuotaMetric::OutputTokens, QuotaPeriod::Daily, 100),
        ])));
        reserved(fixture.reserve("lost", 80, "2026-09-15T08:00:00Z"));
        let recovery = recover_at_path(
            &fixture.path,
            at("2026-09-15T08:30:00Z"),
            at("2026-09-15T08:31:00Z"),
            100,
        )
        .unwrap();
        assert_eq!(recovery.recovered, 1);
        let summary = fixture.summary("2026-09-15T08:31:00Z");
        assert_eq!(
            (
                summary.rules[0].reserved,
                summary.rules[0].uncertain,
                summary.rules[0].remaining
            ),
            (0, 80, 20)
        );
        assert_eq!(
            recover_at_path(
                &fixture.path,
                at("2026-09-15T08:30:00Z"),
                at("2026-09-15T08:32:00Z"),
                100
            )
            .unwrap()
            .recovered,
            0
        );
        settle_at_path(
            &fixture.path,
            "lost",
            Settlement::Reported(Usage {
                input_tokens: Some(30),
                ..Default::default()
            }),
            at("2026-09-15T08:33:00Z"),
        )
        .unwrap();
        let summary = fixture.summary("2026-09-15T08:33:00Z");
        assert_eq!(
            (
                summary.rules[0].confirmed,
                summary.rules[0].uncertain,
                summary.rules[0].remaining
            ),
            (30, 0, 70)
        );
        assert_eq!(summary.rules[1].uncertain, 10);
        settle_at_path(
            &fixture.path,
            "lost",
            Settlement::Reported(Usage {
                output_tokens: Some(0),
                ..Default::default()
            }),
            at("2026-09-15T08:34:00Z"),
        )
        .unwrap();
        let summary = fixture.summary("2026-09-15T08:34:00Z");
        assert_eq!(summary.rules[0].confirmed, 30);
        assert_eq!(summary.rules[1].uncertain, 0);
        assert_eq!(summary.rules[1].remaining, 100);
        assert!(settle_at_path(
            &fixture.path,
            "lost",
            Settlement::Released,
            at("2026-09-15T08:35:00Z")
        )
        .is_err());
    }

    #[test]
    fn request_counts_once_but_every_fallback_reserves_tokens() {
        let fixture = Fixture::new(Some(policy(vec![
            rule(QuotaMetric::InputTokens, QuotaPeriod::Daily, 100),
            rule(QuotaMetric::Requests, QuotaPeriod::Daily, 1),
        ])));
        let first = request("first", 40);
        reserved(reserve_at_path(&fixture.path, &first, at("2026-09-15T08:00:00Z")).unwrap());
        settle_at_path(
            &fixture.path,
            "first",
            Settlement::Unknown,
            at("2026-09-15T08:00:01Z"),
        )
        .unwrap();
        let mut retry = request("second", 40);
        retry.request_id = first.request_id.clone();
        reserved(reserve_at_path(&fixture.path, &retry, at("2026-09-15T08:00:02Z")).unwrap());
        settle_at_path(
            &fixture.path,
            "second",
            Settlement::Reported(Usage {
                input_tokens: Some(30),
                ..Default::default()
            }),
            at("2026-09-15T08:00:03Z"),
        )
        .unwrap();
        let summary = fixture.summary("2026-09-15T08:00:03Z");
        assert_eq!(
            (
                summary.rules[0].confirmed,
                summary.rules[0].uncertain,
                summary.rules[0].remaining
            ),
            (30, 40, 30)
        );
        assert_eq!(summary.rules[1].confirmed, 1);
        assert_eq!(summary.rules[1].remaining, 0);
        assert!(matches!(
            fixture.reserve("new-client-request", 1, "2026-09-15T08:01:00Z"),
            Admission::Denied(Denial {
                metric: Some(QuotaMetric::Requests),
                ..
            })
        ));
    }

    #[test]
    fn proven_undispatched_release_refunds_both_tokens_and_request() {
        let fixture = Fixture::new(Some(policy(vec![
            rule(QuotaMetric::InputTokens, QuotaPeriod::Daily, 100),
            rule(QuotaMetric::Requests, QuotaPeriod::Daily, 1),
        ])));
        reserved(fixture.reserve("local-error", 100, "2026-09-15T08:00:00Z"));
        settle_at_path(
            &fixture.path,
            "local-error",
            Settlement::Released,
            at("2026-09-15T08:00:01Z"),
        )
        .unwrap();
        assert!(
            !settle_at_path(
                &fixture.path,
                "local-error",
                Settlement::Released,
                at("2026-09-15T08:00:02Z")
            )
            .unwrap()
            .changed
        );
        let summary = fixture.summary("2026-09-15T08:00:03Z");
        assert_eq!(summary.rules[0].remaining, 100);
        assert_eq!(summary.rules[1].remaining, 1);
        reserved(fixture.reserve("good-request", 100, "2026-09-15T08:00:04Z"));
    }

    #[test]
    fn unused_first_anchor_is_abandoned_and_released_attempt_replay_keeps_its_original_anchor() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::Requests,
            QuotaPeriod::Weekly,
            1,
        )])));
        let original = reserved(fixture.reserve("local-failure", 1, "2026-09-15T08:00:00Z"));
        settle_at_path(
            &fixture.path,
            "local-failure",
            Settlement::Released,
            at("2026-09-15T08:00:01Z"),
        )
        .unwrap();
        assert_eq!(fixture.summary("2026-09-15T08:00:02Z").anchor, None);
        let replay = reserved(fixture.reserve("local-failure", 1, "2026-09-16T08:00:00Z"));
        assert!(replay.replay);
        assert_eq!(replay.state, "released");
        assert_eq!(replay.anchor, original.anchor);
        assert_eq!(fixture.summary("2026-09-16T08:00:00Z").anchor, None);
        let dispatched = reserved(fixture.reserve("actual-first-use", 1, "2026-09-18T15:00:00Z"));
        assert_eq!(dispatched.anchor, stamp(at("2026-09-18T15:00:00Z")));
        assert_eq!(
            fixture.summary("2026-09-18T15:00:01Z").rules[0].reset_at,
            Some(stamp(at("2026-09-25T15:00:00Z")))
        );
        let old_replay = reserved(fixture.reserve("local-failure", 1, "2026-09-18T15:00:02Z"));
        assert_eq!(old_replay.anchor, original.anchor);
        assert!(
            !settle_at_path(
                &fixture.path,
                "local-failure",
                Settlement::Released,
                at("2026-09-18T15:00:03Z")
            )
            .unwrap()
            .changed
        );
        assert_eq!(
            fixture.summary("2026-09-18T15:00:04Z").anchor,
            Some(dispatched.anchor)
        );
        // Disabling quotas cannot turn a replay into permission to execute.
        let mut record = fixture.record();
        record.access.quota = None;
        fixture.write_record(&record);
        assert!(reserved(fixture.reserve("local-failure", 1, "2026-09-18T15:00:05Z")).replay);
    }

    #[test]
    fn an_active_or_possibly_dispatched_request_keeps_first_anchor() {
        for settlement in [
            None,
            Some(Settlement::Unknown),
            Some(Settlement::Reported(Usage {
                input_tokens: Some(0),
                ..Default::default()
            })),
        ] {
            let fixture = Fixture::new(Some(policy(vec![rule(
                QuotaMetric::InputTokens,
                QuotaPeriod::Daily,
                100,
            )])));
            let original = reserved(fixture.reserve("first", 20, "2026-09-15T08:00:00Z"));
            reserved(fixture.reserve("second", 20, "2026-09-15T08:00:01Z"));
            if let Some(settlement) = settlement {
                settle_at_path(
                    &fixture.path,
                    "second",
                    settlement,
                    at("2026-09-15T08:00:02Z"),
                )
                .unwrap();
            }
            settle_at_path(
                &fixture.path,
                "first",
                Settlement::Released,
                at("2026-09-15T08:00:03Z"),
            )
            .unwrap();
            assert_eq!(
                fixture.summary("2026-09-15T08:00:04Z").anchor,
                Some(original.anchor)
            );
        }
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::Requests,
            QuotaPeriod::Daily,
            2,
        )])));
        reserved(fixture.reserve("one", 1, "2026-09-15T08:00:00Z"));
        reserved(fixture.reserve("two", 1, "2026-09-15T08:00:01Z"));
        settle_at_path(
            &fixture.path,
            "one",
            Settlement::Released,
            at("2026-09-15T08:00:02Z"),
        )
        .unwrap();
        assert!(fixture.summary("2026-09-15T08:00:03Z").anchor.is_some());
        settle_at_path(
            &fixture.path,
            "two",
            Settlement::Released,
            at("2026-09-15T08:00:04Z"),
        )
        .unwrap();
        assert!(fixture.summary("2026-09-15T08:00:05Z").anchor.is_none());
    }

    #[test]
    fn version_one_migration_preserves_holds_and_stores_original_attempt_anchor() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::Requests,
            QuotaPeriod::Weekly,
            1,
        )])));
        let original = reserved(fixture.reserve("old-attempt", 1, "2026-09-15T08:00:00Z"));
        let connection = open(&fixture.path).unwrap();
        connection.execute_batch("ALTER TABLE renewable_quota_attempts DROP COLUMN anchor; UPDATE renewable_quota_metadata SET version=1 WHERE singleton=1;").unwrap();
        drop(connection);
        let migrated = reserved(fixture.reserve("old-attempt", 1, "2026-09-16T08:00:00Z"));
        assert!(migrated.replay);
        assert_eq!(migrated.anchor, original.anchor);
        assert_eq!(fixture.summary("2026-09-16T08:00:00Z").rules[0].reserved, 1);
        settle_at_path(
            &fixture.path,
            "old-attempt",
            Settlement::Released,
            at("2026-09-16T08:00:01Z"),
        )
        .unwrap();
        assert!(fixture.summary("2026-09-16T08:00:02Z").anchor.is_none());
        assert_eq!(
            reserved(fixture.reserve("old-attempt", 1, "2026-09-16T08:00:03Z")).anchor,
            original.anchor
        );
    }

    #[test]
    fn all_undispatched_attempts_release_one_request_charge_in_either_order() {
        for reversed in [false, true] {
            let fixture = Fixture::new(Some(policy(vec![
                rule(QuotaMetric::InputTokens, QuotaPeriod::Daily, 100),
                rule(QuotaMetric::Requests, QuotaPeriod::Daily, 1),
                rule(QuotaMetric::Requests, QuotaPeriod::Weekly, 1),
            ])));
            let first = request("first", 40);
            let mut second = request("second", 40);
            second.request_id = first.request_id.clone();
            reserved(reserve_at_path(&fixture.path, &first, at("2026-09-15T08:00:00Z")).unwrap());
            reserved(reserve_at_path(&fixture.path, &second, at("2026-09-15T08:00:01Z")).unwrap());
            let order = if reversed {
                ["second", "first"]
            } else {
                ["first", "second"]
            };
            settle_at_path(
                &fixture.path,
                order[0],
                Settlement::Released,
                at("2026-09-15T08:00:02Z"),
            )
            .unwrap();
            let summary = fixture.summary("2026-09-15T08:00:02Z");
            assert!(summary
                .rules
                .iter()
                .filter(|rule| rule.metric == QuotaMetric::Requests)
                .all(|rule| rule.remaining == 0));
            settle_at_path(
                &fixture.path,
                order[1],
                Settlement::Released,
                at("2026-09-15T08:00:03Z"),
            )
            .unwrap();
            let summary = fixture.summary("2026-09-15T08:00:03Z");
            assert!(summary
                .rules
                .iter()
                .all(|rule| rule.remaining == rule.limit));
            assert!(
                !settle_at_path(
                    &fixture.path,
                    order[0],
                    Settlement::Released,
                    at("2026-09-15T08:00:04Z")
                )
                .unwrap()
                .changed
            );
            reserved(fixture.reserve("new-request", 100, "2026-09-15T08:00:05Z"));
        }
    }

    #[test]
    fn released_first_attempt_retains_request_charge_when_retry_was_dispatched() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::Requests,
            QuotaPeriod::Daily,
            1,
        )])));
        let first = request("first", 1);
        let mut second = request("second", 1);
        second.request_id = first.request_id.clone();
        reserved(reserve_at_path(&fixture.path, &first, at("2026-09-15T08:00:00Z")).unwrap());
        reserved(reserve_at_path(&fixture.path, &second, at("2026-09-15T08:00:01Z")).unwrap());
        settle_at_path(
            &fixture.path,
            "first",
            Settlement::Released,
            at("2026-09-15T08:00:02Z"),
        )
        .unwrap();
        settle_at_path(
            &fixture.path,
            "second",
            Settlement::Unknown,
            at("2026-09-15T08:00:03Z"),
        )
        .unwrap();
        let summary = fixture.summary("2026-09-15T08:00:04Z");
        assert_eq!(summary.rules[0].confirmed, 1);
        assert_eq!(summary.rules[0].remaining, 0);
    }

    #[test]
    fn reservation_replay_remains_in_original_window_and_cannot_change_request() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::InputTokens,
            QuotaPeriod::Daily,
            100,
        )])));
        reserved(fixture.reserve("long-stream", 80, "2026-09-15T08:00:00Z"));
        let replay = reserved(fixture.reserve("long-stream", 80, "2026-09-16T08:00:00Z"));
        assert!(replay.replay);
        assert_eq!(
            fixture.summary("2026-09-16T08:00:00Z").rules[0].remaining,
            100
        );
        settle_at_path(
            &fixture.path,
            "long-stream",
            Settlement::Reported(Usage {
                input_tokens: Some(60),
                ..Default::default()
            }),
            at("2026-09-16T08:00:01Z"),
        )
        .unwrap();
        assert_eq!(
            fixture.summary("2026-09-16T08:00:02Z").rules[0].confirmed,
            0
        );
        let old = window_balances(
            &open(&fixture.path).unwrap(),
            "key-1",
            QuotaMetric::InputTokens,
            QuotaPeriod::Daily,
            &stamp(at("2026-09-15T08:00:00Z")),
        )
        .unwrap();
        assert_eq!(old, (60, 0, 0));
        assert!(reserve_at_path(
            &fixture.path,
            &request("long-stream", 79),
            at("2026-09-16T08:00:03Z")
        )
        .is_err());
    }

    #[test]
    fn key_rule_edits_preserve_balances_and_schedule_deactivation_is_not_reset() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::InputTokens,
            QuotaPeriod::Weekly,
            100,
        )])));
        reserved(fixture.reserve("one", 80, "2026-09-15T08:00:00Z"));
        let mut record = fixture.record();
        let old = record.access.quota.clone();
        record.access.quota.as_mut().unwrap().rules[0].limit = 200;
        let mut connection = open(&fixture.path).unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        validate_policy_change(&tx, "key-1", old.as_ref(), record.access.quota.as_ref()).unwrap();
        tx.commit().unwrap();
        fixture.write_record(&record);
        let summary = fixture.summary("2026-09-15T08:01:00Z");
        assert_eq!(summary.rules[0].remaining, 120);
        assert_eq!(summary.revision, 2);
        let activated = record.access.quota.clone();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        validate_policy_change(&tx, "key-1", activated.as_ref(), None).unwrap();
        tx.commit().unwrap();
        record.access.quota = None;
        fixture.write_record(&record);
        assert!(matches!(
            fixture.reserve("unlimited", 500, "2026-09-15T08:01:01Z"),
            Admission::NotConfigured
        ));
        let incompatible = policy(vec![rule(
            QuotaMetric::InputTokens,
            QuotaPeriod::Daily,
            200,
        )]);
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(validate_policy_change(&tx, "key-1", None, Some(&incompatible)).is_err());
        validate_policy_change(&tx, "key-1", None, activated.as_ref()).unwrap();
        tx.commit().unwrap();
        record.access.quota = activated;
        fixture.write_record(&record);
        assert_eq!(
            fixture.summary("2026-09-15T08:02:00Z").rules[0].remaining,
            120
        );
    }

    #[test]
    fn reservation_reads_latest_limits_and_revocation_in_transaction() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::InputTokens,
            QuotaPeriod::Daily,
            100,
        )])));
        let mut record = fixture.record();
        record.access.quota.as_mut().unwrap().rules[0].limit = 10;
        fixture.write_record(&record);
        assert!(matches!(
            fixture.reserve("stale-100", 11, "2026-09-15T08:00:00Z"),
            Admission::Denied(_)
        ));
        record.revoked_at = Some("2026-09-15T08:00:01Z".to_string());
        fixture.write_record(&record);
        assert!(
            matches!(fixture.reserve("revoked", 1, "2026-09-15T08:00:02Z"), Admission::Denied(Denial { code, .. }) if code == "key_revoked")
        );
    }

    #[test]
    fn concurrent_legacy_budget_add_lower_remove_or_period_edit_requires_fresh_admission() {
        use crate::api_keys::{ApiKeyBudgetPeriod, ApiKeyInputTokenBudget};
        let lifetime = |limit| {
            Some(ApiKeyInputTokenBudget {
                limit,
                period: ApiKeyBudgetPeriod::Lifetime,
            })
        };
        for (snapshot, current) in [
            (None, lifetime(100)),
            (lifetime(100), lifetime(10)),
            (lifetime(100), None),
            (
                lifetime(100),
                Some(ApiKeyInputTokenBudget {
                    limit: 100,
                    period: ApiKeyBudgetPeriod::CalendarMonth,
                }),
            ),
        ] {
            let fixture = Fixture::new(Some(policy(vec![rule(
                QuotaMetric::Requests,
                QuotaPeriod::Daily,
                1,
            )])));
            let mut record = fixture.record();
            record.access.input_token_budget = current.clone();
            fixture.write_record(&record);
            let mut stale = request("stale-budget", 1);
            stale.expected_legacy_budget = snapshot;
            assert!(
                matches!(reserve_at_path(&fixture.path, &stale, at("2026-09-15T08:00:00Z")).unwrap(), Admission::Denied(Denial { code, .. }) if code == "api_key_policy_changed")
            );
            assert_eq!(fixture.summary("2026-09-15T08:00:00Z").anchor, None);
            assert_eq!(
                fixture.summary("2026-09-15T08:00:00Z").rules[0].remaining,
                1
            );
            let mut fresh = request("current-budget", 1);
            fresh.expected_legacy_budget = current;
            reserved(reserve_at_path(&fixture.path, &fresh, at("2026-09-15T08:00:01Z")).unwrap());
        }
        // The guard also applies when no renewable quota is configured.
        let fixture = Fixture::new(None);
        let mut record = fixture.record();
        record.access.input_token_budget = lifetime(100);
        fixture.write_record(&record);
        assert!(
            matches!(fixture.reserve("nonquota-stale-budget", 1, "2026-09-15T08:00:00Z"), Admission::Denied(Denial { code, .. }) if code == "api_key_policy_changed")
        );
    }

    #[test]
    fn even_nonquota_keys_recheck_current_scope_and_per_request_cap() {
        let fixture = Fixture::new(None);
        let mut record = fixture.record();
        record.access.prompt_token_limit = Some(5);
        fixture.write_record(&record);
        assert!(
            matches!(fixture.reserve("too-big", 6, "2026-09-15T08:00:00Z"), Admission::Denied(Denial { code, .. }) if code == "token_limit_exceeded")
        );
        record.access.all = false;
        record.access.providers = vec![crate::api_keys::ApiKeyProviderAccess {
            provider: "claude".to_string(),
            account_scope: crate::api_keys::ApiKeyAccountScope::Selected,
            accounts: vec!["claude:different".to_string()],
            ..Default::default()
        }];
        fixture.write_record(&record);
        assert!(
            matches!(fixture.reserve("wrong-account", 1, "2026-09-15T08:00:01Z"), Admission::Denied(Denial { code, .. }) if code == "scope_denied")
        );
        record.access.providers[0].accounts[0] = "claude:canonical-account".to_string();
        fixture.write_record(&record);
        assert!(matches!(
            fixture.reserve("permitted", 1, "2026-09-15T08:00:02Z"),
            Admission::NotConfigured
        ));
    }

    #[test]
    fn backward_clock_jump_never_reopens_old_window() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::Requests,
            QuotaPeriod::Daily,
            1,
        )])));
        reserved(fixture.reserve("day-one", 1, "2026-09-15T08:00:00Z"));
        reserved(fixture.reserve("day-two", 1, "2026-09-16T08:00:00Z"));
        assert!(matches!(
            fixture.reserve("clock-back", 1, "2026-09-15T12:00:00Z"),
            Admission::Denied(_)
        ));
        assert_eq!(
            fixture.summary("2026-09-15T12:00:00Z").rules[0].window_start,
            Some(stamp(at("2026-09-16T08:00:00Z")))
        );
    }

    #[test]
    fn concurrent_first_use_has_one_anchor_and_no_overadmission() {
        let fixture = Fixture::new(Some(policy(vec![
            rule(QuotaMetric::Requests, QuotaPeriod::Daily, 1),
            rule(QuotaMetric::InputTokens, QuotaPeriod::Weekly, 10),
        ])));
        let barrier = Arc::new(Barrier::new(20));
        let workers = (0..20)
            .map(|index| {
                let path = fixture.path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve_at_path(
                        &path,
                        &request(&format!("concurrent-{index}"), 10),
                        at("2026-09-15T08:00:00Z"),
                    )
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let accepted = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|result| matches!(result, Admission::Reserved(_)))
            .count();
        assert_eq!(accepted, 1);
        let summary = fixture.summary("2026-09-15T08:00:01Z");
        assert_eq!(summary.anchor, Some(stamp(at("2026-09-15T08:00:00Z"))));
        assert!(summary.rules.iter().all(|rule| rule.remaining == 0));
    }

    #[test]
    fn all_token_dimensions_are_independent_without_cache_double_counting() {
        let fixture = Fixture::new(Some(policy(vec![
            rule(QuotaMetric::InputTokens, QuotaPeriod::Daily, 200),
            rule(QuotaMetric::UncachedInputTokens, QuotaPeriod::Daily, 200),
            rule(QuotaMetric::OutputTokens, QuotaPeriod::Daily, 200),
            rule(QuotaMetric::CacheReadTokens, QuotaPeriod::Daily, 200),
            rule(QuotaMetric::CacheWriteTokens, QuotaPeriod::Daily, 200),
            rule(QuotaMetric::CacheTokens, QuotaPeriod::Daily, 200),
        ])));
        reserved(fixture.reserve("cache", 100, "2026-09-15T08:00:00Z"));
        settle_at_path(
            &fixture.path,
            "cache",
            Settlement::Reported(Usage {
                input_tokens: Some(100),
                uncached_input_tokens: Some(20),
                output_tokens: Some(8),
                cache_read_tokens: Some(70),
                cache_write_tokens: Some(10),
                cache_tokens: None,
            }),
            at("2026-09-15T08:00:01Z"),
        )
        .unwrap();
        let summary = fixture.summary("2026-09-15T08:00:01Z");
        let actuals = summary
            .rules
            .iter()
            .map(|rule| (rule.metric, rule.confirmed))
            .collect::<Vec<_>>();
        assert_eq!(
            actuals,
            vec![
                (QuotaMetric::InputTokens, 100),
                (QuotaMetric::UncachedInputTokens, 20),
                (QuotaMetric::OutputTokens, 8),
                (QuotaMetric::CacheReadTokens, 70),
                (QuotaMetric::CacheWriteTokens, 10),
                (QuotaMetric::CacheTokens, 80)
            ]
        );
    }

    #[test]
    fn fresh_connection_after_restart_preserves_holds_anchor_and_settlement() {
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::InputTokens,
            QuotaPeriod::Weekly,
            100,
        )])));
        reserved(fixture.reserve("before-restart", 90, "2026-09-15T08:00:00Z"));
        // Every operation opens/closes its connection: no in-memory balances
        // survive between these calls, exactly like a fresh process startup.
        let summary = fixture.summary("2026-09-17T18:00:00Z");
        assert_eq!(summary.anchor, Some(stamp(at("2026-09-15T08:00:00Z"))));
        assert_eq!(summary.rules[0].remaining, 10);
        assert!(matches!(
            fixture.reserve("after-restart", 11, "2026-09-17T18:00:00Z"),
            Admission::Denied(_)
        ));
        settle_at_path(
            &fixture.path,
            "before-restart",
            Settlement::Unknown,
            at("2026-09-17T18:01:00Z"),
        )
        .unwrap();
        assert_eq!(
            fixture.summary("2026-09-17T18:02:00Z").rules[0].remaining,
            10
        );
    }

    #[test]
    fn dropped_quota_table_or_metadata_row_never_recreates_spendable_capacity() {
        for table in TABLES {
            let fixture = Fixture::new(Some(policy(vec![rule(
                QuotaMetric::Requests,
                QuotaPeriod::Daily,
                1,
            )])));
            reserved(fixture.reserve("spent", 1, "2026-09-15T08:00:00Z"));
            let connection = open(&fixture.path).unwrap();
            let pinned: bool = connection
                .query_row(
                    "SELECT renewable_initialized FROM managed_registry_metadata WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(pinned);
            connection
                .execute_batch(&format!("PRAGMA foreign_keys=OFF; DROP TABLE {table};"))
                .unwrap();
            drop(connection);
            assert!(
                reserve_at_path(
                    &fixture.path,
                    &request("after-corruption", 1),
                    at("2026-09-15T08:00:01Z")
                )
                .unwrap_err()
                .contains("table is missing"),
                "{table}"
            );
            let read_only = Connection::open_with_flags(
                &fixture.path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let exists: bool = read_only
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!exists, "corrupt table was recreated: {table}");
        }
        let fixture = Fixture::new(Some(policy(vec![rule(
            QuotaMetric::Requests,
            QuotaPeriod::Daily,
            1,
        )])));
        reserved(fixture.reserve("spent", 1, "2026-09-15T08:00:00Z"));
        open(&fixture.path)
            .unwrap()
            .execute("DELETE FROM renewable_quota_metadata", [])
            .unwrap();
        assert!(summary_at_path(&fixture.path, "key-1", at("2026-09-15T08:00:01Z")).is_err());
    }

    #[test]
    fn missing_schedule_with_remaining_accounting_never_reanchors_or_claims_free_capacity() {
        for corruption in ["attempt-only", "balance-only", "both"] {
            let fixture = Fixture::new(Some(policy(vec![rule(
                QuotaMetric::Requests,
                QuotaPeriod::Daily,
                1,
            )])));
            reserved(fixture.reserve("spent", 1, "2026-09-15T08:00:00Z"));
            let connection = open(&fixture.path).unwrap();
            connection
                .execute("DELETE FROM renewable_quota_schedules", [])
                .unwrap();
            match corruption {
                "attempt-only" => {
                    connection
                        .execute("UPDATE renewable_quota_windows SET reserved=0", [])
                        .unwrap();
                }
                "balance-only" => {
                    connection
                        .execute_batch(
                            "DELETE FROM renewable_quota_holds;
                             DELETE FROM renewable_quota_attempts;",
                        )
                        .unwrap();
                }
                _ => {}
            }
            let snapshot = || {
                connection.query_row(
                    "SELECT (SELECT COUNT(*) FROM renewable_quota_schedules),
                            (SELECT COUNT(*) FROM renewable_quota_attempts),
                            (SELECT COUNT(*) FROM renewable_quota_holds),
                            (SELECT COUNT(*) FROM renewable_quota_requests),
                            (SELECT COUNT(*) FROM renewable_quota_ledger),
                            (SELECT SUM(confirmed+reserved+uncertain) FROM renewable_quota_windows)",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?)),
                ).unwrap()
            };
            let before = snapshot();
            for _ in 0..2 {
                assert!(reserve_at_path(
                    &fixture.path,
                    &request("after-corruption", 1),
                    at("2026-09-15T08:00:01Z"),
                )
                .unwrap_err()
                .contains("schedule is missing"));
                assert!(
                    summary_at_path(&fixture.path, "key-1", at("2026-09-15T08:00:01Z"),)
                        .unwrap_err()
                        .contains("schedule is missing")
                );
                assert_eq!(snapshot(), before, "must not repair {corruption}");
            }
        }
    }

    #[test]
    fn limit_rules_can_combine_periods_without_resetting_each_other() {
        let fixture = Fixture::new(Some(policy(vec![
            rule(QuotaMetric::Requests, QuotaPeriod::Daily, 1),
            rule(QuotaMetric::Requests, QuotaPeriod::Weekly, 2),
        ])));
        reserved(fixture.reserve("first-day", 1, "2026-09-15T08:00:00Z"));
        assert!(matches!(
            fixture.reserve("first-day-extra", 1, "2026-09-15T20:00:00Z"),
            Admission::Denied(_)
        ));
        reserved(fixture.reserve("second-day", 1, "2026-09-16T08:00:00Z"));
        assert!(matches!(
            fixture.reserve("third-day", 1, "2026-09-17T08:00:00Z"),
            Admission::Denied(Denial {
                period: Some(QuotaPeriod::Weekly),
                ..
            })
        ));
        reserved(fixture.reserve("next-week", 1, "2026-09-22T08:00:00Z"));
    }
}
