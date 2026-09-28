use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::OpenOptions, path::PathBuf};
use uuid::Uuid;

const API_KEYS_FILE: &str = "api-keys.json";
const API_KEYS_LOCK_FILE: &str = "api-keys.lock";
const API_KEYS_MIGRATION_BACKUP: &str = "api-keys.json.migration-backup";
const API_KEYS_SQLITE_SENTINEL: &str = "managed_api_keys_sqlite_v1";
const MISSING_LEGACY_ACCOUNTING: &str = "legacy API-key JSON contains budget/quota rules, but its previous accounting database is missing; refusing to reset usage. Restore the original api-key-policy.sqlite3 and its consistent WAL state, or explicitly reconcile historical usage before migration";
const LEGACY_PROXY_API_KEY_LABEL: &str = "Legacy proxy_api_key";
/// SQLite stores signed 64-bit integers. Input-token budgets are persisted in
/// the policy database, so accepting a larger unsigned value would create a
/// key that can never be used safely.
pub(crate) const MAX_PERSISTED_INPUT_TOKENS: u64 = i64::MAX as u64;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ApiKeyStore {
    pub keys: Vec<ApiKeyRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ApiKeyRecord {
    pub id: String,
    pub label: String,
    pub key_prefix: String,
    pub lookup_hash: String,
    pub hash: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub source: ApiKeySource,
    pub access: ApiKeyAccess,
}

impl Default for ApiKeyRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            key_prefix: String::new(),
            lookup_hash: String::new(),
            hash: String::new(),
            created_at: String::new(),
            last_used_at: None,
            revoked_at: None,
            source: ApiKeySource::Managed,
            access: ApiKeyAccess::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ApiKeyAccess {
    pub all: bool,
    /// Maximum estimated input tokens allowed for one request. This is a
    /// request guardrail, not a cumulative usage budget.
    #[serde(
        rename = "max_estimated_input_tokens_per_request",
        alias = "prompt_token_limit"
    )]
    pub prompt_token_limit: Option<u64>,
    /// Optional cumulative input-token budget. The budget period is explicit
    /// so this cannot be mistaken for the per-request guardrail above.
    pub input_token_budget: Option<ApiKeyInputTokenBudget>,
    /// Independent renewable multi-resource quotas. Their rules, anchors and
    /// balances share the same durable SQLite authority as this key record.
    pub quota: Option<crate::api_key_quota::QuotaPolicy>,
    pub providers: Vec<ApiKeyProviderAccess>,
}

impl Default for ApiKeyAccess {
    fn default() -> Self {
        Self {
            all: true,
            prompt_token_limit: None,
            input_token_budget: None,
            quota: None,
            providers: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiKeyAccountScope {
    #[default]
    All,
    Selected,
}

impl ApiKeyAccountScope {
    pub(crate) fn is_all(self) -> bool {
        matches!(self, Self::All)
    }
}

/// Scope for a provider rule. `account_scope` is intentionally separate from
/// `accounts`: an empty account list must never be used as an implicit
/// all-accounts sentinel in newly-created rules.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct ApiKeyProviderAccess {
    pub provider: String,
    pub account_scope: ApiKeyAccountScope,
    /// Account selectors when `account_scope` is `selected`. Normalization
    /// rejects an empty selected list and an all-accounts rule with entries.
    pub accounts: Vec<String>,
    #[serde(
        rename = "max_estimated_input_tokens_per_request",
        alias = "prompt_token_limit"
    )]
    pub prompt_token_limit: Option<u64>,
    pub account_limits: Vec<ApiKeyAccountLimit>,
}

/// Wire representation used only to preserve pre-scope API-key files. Older
/// records omitted `account_scope` and used an empty list for all accounts;
/// deserialize those records into the explicit equivalent, then always write
/// the explicit field back out.
#[derive(Deserialize)]
#[serde(default)]
struct ApiKeyProviderAccessWire {
    provider: String,
    account_scope: Option<ApiKeyAccountScope>,
    accounts: Vec<String>,
    #[serde(
        rename = "max_estimated_input_tokens_per_request",
        alias = "prompt_token_limit"
    )]
    prompt_token_limit: Option<u64>,
    account_limits: Vec<ApiKeyAccountLimit>,
}

impl Default for ApiKeyProviderAccessWire {
    fn default() -> Self {
        Self {
            provider: String::new(),
            account_scope: None,
            accounts: Vec::new(),
            prompt_token_limit: None,
            account_limits: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for ApiKeyProviderAccess {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ApiKeyProviderAccessWire::deserialize(deserializer)?;
        // Legacy records had no scope. Preserve their old meaning during the
        // migration only: non-empty selected accounts, empty all accounts.
        let account_scope = wire.account_scope.unwrap_or_else(|| {
            if wire
                .accounts
                .iter()
                .all(|account| account.trim().is_empty())
            {
                ApiKeyAccountScope::All
            } else {
                ApiKeyAccountScope::Selected
            }
        });
        Ok(Self {
            provider: wire.provider,
            account_scope,
            accounts: wire.accounts,
            prompt_token_limit: wire.prompt_token_limit,
            account_limits: wire.account_limits,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ApiKeyAccountLimit {
    pub account: String,
    #[serde(
        rename = "max_estimated_input_tokens_per_request",
        alias = "prompt_token_limit"
    )]
    pub prompt_token_limit: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiKeyBudgetPeriod {
    #[default]
    Lifetime,
    CalendarMonth,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ApiKeyInputTokenBudget {
    pub limit: u64,
    pub period: ApiKeyBudgetPeriod,
}

impl Default for ApiKeyInputTokenBudget {
    fn default() -> Self {
        Self {
            limit: 0,
            period: ApiKeyBudgetPeriod::Lifetime,
        }
    }
}

impl ApiKeyInputTokenBudget {
    fn normalized(&self) -> Result<Self, String> {
        if self.limit == 0 {
            return Err("input token budget limit must be greater than zero".to_string());
        }
        if self.limit > MAX_PERSISTED_INPUT_TOKENS {
            return Err(format!(
                "input token budget limit must not exceed {} (SQLite signed integer range)",
                MAX_PERSISTED_INPUT_TOKENS
            ));
        }
        Ok(self.clone())
    }
}

impl ApiKeyStore {
    /// Validates and canonicalizes every stored access rule. Call this while
    /// loading persistent state so legacy account-list records are upgraded
    /// in memory and invalid zero limits never become live policy.
    pub(crate) fn normalized(&self) -> Result<Self, String> {
        let mut normalized = self.clone();
        for record in &mut normalized.keys {
            record.access = record
                .access
                .normalized()
                .map_err(|err| format!("invalid access for API key '{}': {}", record.id, err))?;
        }
        Ok(normalized)
    }
}

impl ApiKeyAccess {
    pub(crate) fn allows_provider(&self, provider: &str) -> bool {
        self.all || self.provider_rule(provider).is_some()
    }

    pub(crate) fn provider_rule(&self, provider: &str) -> Option<&ApiKeyProviderAccess> {
        let provider = normalize_provider(provider)?;
        self.providers.iter().find(|rule| rule.provider == provider)
    }

    pub(crate) fn normalized(&self) -> Result<Self, String> {
        let prompt_token_limit =
            normalize_limit(self.prompt_token_limit, "API key prompt token limit")?;
        let input_token_budget = self
            .input_token_budget
            .as_ref()
            .map(ApiKeyInputTokenBudget::normalized)
            .transpose()?;
        let quota = self
            .quota
            .as_ref()
            .map(|quota| quota.normalized())
            .transpose()?;

        // Validate limits in ignored provider rules too. Otherwise a caller
        // could submit a zero limit while `all` is true and have it silently
        // converted to an unlimited limit when that rule is later enabled.
        for rule in &self.providers {
            validate_provider_limits(rule)?;
        }

        if self.all {
            return Ok(Self {
                all: true,
                prompt_token_limit,
                input_token_budget,
                quota,
                providers: Vec::new(),
            });
        }

        let mut providers: Vec<ApiKeyProviderAccess> = Vec::new();
        for incoming in &self.providers {
            let normalized = normalize_provider_rule(incoming)?;
            let provider = normalized.provider.clone();

            if let Some(existing) = providers.iter_mut().find(|rule| rule.provider == provider) {
                if existing.account_scope.is_all() || normalized.account_scope.is_all() {
                    existing.account_scope = ApiKeyAccountScope::All;
                    existing.accounts.clear();
                } else {
                    for account in normalized.accounts {
                        if !existing.accounts.iter().any(|value| value == &account) {
                            existing.accounts.push(account);
                        }
                    }
                }
                existing.prompt_token_limit =
                    min_limit(existing.prompt_token_limit, normalized.prompt_token_limit);
                merge_account_limits(&mut existing.account_limits, normalized.account_limits);
            } else {
                providers.push(normalized);
            }
        }

        if providers.is_empty() {
            return Err("Restricted access requires at least one provider or account".to_string());
        }
        providers.sort_by(|left, right| left.provider.cmp(&right.provider));
        Ok(Self {
            all: false,
            prompt_token_limit,
            input_token_budget,
            quota,
            providers,
        })
    }
}

impl ApiKeyProviderAccess {
    pub(crate) fn allows_account<F>(&self, mut matches: F) -> bool
    where
        F: FnMut(&str) -> bool,
    {
        self.account_scope.is_all() || self.accounts.iter().any(|account| matches(account))
    }

    pub(crate) fn account_prompt_token_limit<F>(&self, mut matches: F) -> Option<u64>
    where
        F: FnMut(&str) -> bool,
    {
        self.account_limits
            .iter()
            .filter(|limit| matches(&limit.account))
            .filter_map(|limit| limit.prompt_token_limit)
            .min()
    }
}

fn validate_provider_limits(rule: &ApiKeyProviderAccess) -> Result<(), String> {
    normalize_limit(
        rule.prompt_token_limit,
        &format!("{} provider prompt token limit", rule.provider.trim()),
    )?;
    for account_limit in &rule.account_limits {
        normalize_limit(
            account_limit.prompt_token_limit,
            &format!(
                "{} account prompt token limit for '{}'",
                rule.provider.trim(),
                account_limit.account.trim()
            ),
        )?;
    }
    Ok(())
}

fn normalize_provider_rule(
    incoming: &ApiKeyProviderAccess,
) -> Result<ApiKeyProviderAccess, String> {
    let provider = normalize_provider(&incoming.provider)
        .ok_or_else(|| format!("Unknown provider '{}'", incoming.provider.trim()))?;
    let accounts = normalize_accounts(&incoming.accounts);
    match incoming.account_scope {
        ApiKeyAccountScope::All if !accounts.is_empty() => {
            return Err(format!(
                "{} all-account scope cannot include selected accounts",
                provider
            ));
        }
        ApiKeyAccountScope::Selected if accounts.is_empty() => {
            return Err(format!(
                "{} selected-account scope requires at least one account",
                provider
            ));
        }
        _ => {}
    }

    Ok(ApiKeyProviderAccess {
        provider: provider.to_string(),
        account_scope: incoming.account_scope,
        accounts,
        prompt_token_limit: normalize_limit(
            incoming.prompt_token_limit,
            &format!("{} provider prompt token limit", provider),
        )?,
        account_limits: normalize_account_limits(&incoming.account_limits, provider)?,
    })
}

fn normalize_accounts(accounts: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for account in accounts {
        let account = account.trim();
        if account.is_empty()
            || normalized
                .iter()
                .any(|existing: &String| existing == account)
        {
            continue;
        }
        normalized.push(account.to_string());
    }
    normalized
}

fn normalize_limit(limit: Option<u64>, field: &str) -> Result<Option<u64>, String> {
    match limit {
        Some(0) => Err(format!("{} must be greater than zero", field)),
        value => Ok(value),
    }
}

fn min_limit(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn normalize_account_limits(
    limits: &[ApiKeyAccountLimit],
    provider: &str,
) -> Result<Vec<ApiKeyAccountLimit>, String> {
    let mut out = Vec::new();
    for incoming in limits {
        let account = incoming.account.trim();
        let prompt_token_limit = normalize_limit(
            incoming.prompt_token_limit,
            &format!("{} account prompt token limit for '{}'", provider, account),
        )?;
        let Some(prompt_token_limit) = prompt_token_limit else {
            continue;
        };
        if account.is_empty() {
            continue;
        }
        if let Some(index) = out
            .iter()
            .position(|limit: &ApiKeyAccountLimit| limit.account == account)
        {
            out[index].prompt_token_limit =
                min_limit(out[index].prompt_token_limit, Some(prompt_token_limit));
        } else {
            out.push(ApiKeyAccountLimit {
                account: account.to_string(),
                prompt_token_limit: Some(prompt_token_limit),
            });
        }
    }
    Ok(out)
}

fn merge_account_limits(target: &mut Vec<ApiKeyAccountLimit>, incoming: Vec<ApiKeyAccountLimit>) {
    for incoming in incoming {
        if let Some(existing) = target
            .iter_mut()
            .find(|limit| limit.account.as_str() == incoming.account.as_str())
        {
            existing.prompt_token_limit =
                min_limit(existing.prompt_token_limit, incoming.prompt_token_limit);
        } else {
            target.push(incoming);
        }
    }
}

fn normalize_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "cod" | "codex" => Some("codex"),
        "agw" | "antigravity" | "anti-gravity" => Some("agw"),
        "gem" | "gemini" => Some("gemini"),
        "qwn" | "qwen" => Some("qwen"),
        "dsk" | "deepseek" | "deep-seek" => Some("deepseek"),
        "grk" | "grok" | "xai" | "x-ai" => Some("grok"),
        "min" | "minimax" | "mini-max" => Some("minimax"),
        "cop" | "copilot" | "github-copilot" => Some("copilot"),
        "cld" | "claude" | "anthropic" => Some("claude"),
        "glm" | "zai" | "z-ai" => Some("glm"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiKeySource {
    #[default]
    Managed,
    LegacyConfig,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PublicApiKeyRecord {
    pub id: String,
    pub label: String,
    pub key_prefix: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub source: ApiKeySource,
    pub access: ApiKeyAccess,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CreatedApiKey {
    pub key: PublicApiKeyRecord,
    pub plain_text_key: String,
}

pub(crate) fn api_keys_path(cfg: &crate::Config) -> PathBuf {
    cfg.auth_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(API_KEYS_FILE)
}

/// A stable companion file used for read/modify/write coordination between
/// gateway processes sharing an auth directory. SQLite makes each write
/// atomic; this lock also protects the registry's in-memory mutation callback.
fn api_keys_lock_path(cfg: &crate::Config) -> PathBuf {
    cfg.auth_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(API_KEYS_LOCK_FILE)
}

/// Keeps a shared or exclusive advisory lock alive for the lifetime of the
/// guard. Every gateway API-key read/write uses this stable lock file, which
/// makes a revoke linearizable across cooperating gateway processes.
pub(crate) struct ApiKeyStoreLock {
    _file: std::fs::File,
}

pub(crate) fn lock_store_shared(cfg: &crate::Config) -> Result<ApiKeyStoreLock, String> {
    lock_store(cfg, false)
}

pub(crate) fn lock_store_exclusive(cfg: &crate::Config) -> Result<ApiKeyStoreLock, String> {
    lock_store(cfg, true)
}

fn lock_store(cfg: &crate::Config, exclusive: bool) -> Result<ApiKeyStoreLock, String> {
    let path = api_keys_lock_path(cfg);
    if let Some(parent) = path.parent() {
        let mut directory = std::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(parent).map_err(|err| {
            format!(
                "failed to create API-key lock directory '{}': {}",
                parent.display(),
                err
            )
        })?;
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|err| {
        format!(
            "failed to open API-key coordination lock '{}': {}",
            path.display(),
            err
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("failed to protect API-key coordination lock: {err}"))?;
    }
    let started = std::time::Instant::now();
    loop {
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock)
                if started.elapsed() < std::time::Duration::from_secs(5) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => {
                return Err(format!(
                    "failed to lock API-key coordination file '{}' within bounded wait: {}",
                    path.display(),
                    err
                ))
            }
        }
    }
    Ok(ApiKeyStoreLock { _file: file })
}

/// Load the SQLite key authority. The legacy JSON import is restartable and
/// keeps a private recovery copy, but makes the old live path deliberately
/// unparsable as an ApiKeyStore so an old binary cannot use stale revocations.
pub(crate) fn load(cfg: &crate::Config) -> Result<ApiKeyStore, String> {
    preflight_legacy_accounting(cfg)?;
    let mut connection = registry_connection(cfg)?;
    migrate_legacy_keys(cfg, &mut connection)?;
    read_records(&connection)
}

pub(crate) fn save(cfg: &crate::Config, store: &ApiKeyStore) -> Result<(), String> {
    let store = store.normalized()?;
    validate_record_ids(&store)?;
    preflight_legacy_accounting(cfg)?;
    let mut connection = registry_connection(cfg)?;
    migrate_legacy_keys(cfg, &mut connection)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| format!("failed to begin API-key registry write: {err}"))?;
    let previous = read_records(&transaction)?;
    validate_record_changes(&transaction, &previous, &store)?;
    for record in &store.keys {
        let old = previous.keys.iter().find(|old| old.id == record.id);
        if old == Some(record) {
            continue;
        }
        let record_json = serde_json::to_string(record)
            .map_err(|err| format!("failed to serialize API key record: {err}"))?;
        transaction
            .execute(
                "INSERT INTO managed_api_keys(id, record_json) VALUES(?1, ?2)
             ON CONFLICT(id) DO UPDATE SET record_json = excluded.record_json",
                params![record.id, record_json],
            )
            .map_err(|err| format!("failed to save API key record: {err}"))?;
        let event = match old {
            None => Some("created"),
            Some(old) if old.revoked_at != record.revoked_at => Some("revoked"),
            Some(old) if old.hash != record.hash => Some("credential_rotated"),
            Some(old) if old.access != record.access => Some("policy_changed"),
            _ => None,
        };
        if let Some(event) = event {
            transaction
                .execute(
                    "INSERT INTO managed_api_key_events(api_key_id, event_kind, occurred_at)
                 VALUES(?1, ?2, ?3)",
                    params![record.id, event, chrono::Utc::now().to_rfc3339()],
                )
                .map_err(|err| format!("failed to save API-key administrative event: {err}"))?;
        }
    }
    transaction
        .commit()
        .map_err(|err| format!("failed to commit API-key registry write: {err}"))
}

/// Validation for a friendly admin response; save repeats this under its
/// authoritative write transaction so concurrent first use cannot race it.
pub(crate) fn validate_policy_changes(
    cfg: &crate::Config,
    store: &ApiKeyStore,
) -> Result<(), String> {
    preflight_legacy_accounting(cfg)?;
    let mut connection = registry_connection(cfg)?;
    migrate_legacy_keys(cfg, &mut connection)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| format!("failed to begin API-key policy validation: {err}"))?;
    let previous = read_records(&transaction)?;
    validate_record_changes(&transaction, &previous, store)
}

fn registry_connection(cfg: &crate::Config) -> Result<Connection, String> {
    crate::api_key_policy_store::open_connection_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
    )
}

fn preflight_legacy_accounting(cfg: &crate::Config) -> Result<(), String> {
    if crate::api_key_policy_store::policy_db_path(cfg)
        .try_exists()
        .map_err(|err| format!("failed to inspect legacy accounting database: {err}"))?
    {
        return Ok(());
    }
    let Some(data) = read_optional_file(&api_keys_path(cfg))? else {
        return Ok(());
    };
    // Malformed files and migrated markers receive their more specific errors
    // from the established identity/import checks. Only legacy rule records
    // can prove that an apparently fresh directory needs historical balances.
    if let Ok(store) = serde_json::from_slice::<ApiKeyStore>(&data) {
        if store.keys.iter().any(|record| {
            record.access.input_token_budget.is_some() || record.access.quota.is_some()
        }) {
            return Err(MISSING_LEGACY_ACCOUNTING.to_string());
        }
    }
    Ok(())
}

fn validate_record_ids(store: &ApiKeyStore) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for record in &store.keys {
        if record.id.is_empty() || record.id.len() > 256 || record.id.chars().any(char::is_control)
        {
            return Err("invalid persistent API key ID".to_string());
        }
        if !seen.insert(record.id.as_str()) {
            return Err("duplicate persistent API key ID".to_string());
        }
    }
    Ok(())
}

fn validate_record_changes(
    transaction: &rusqlite::Transaction<'_>,
    previous: &ApiKeyStore,
    candidate: &ApiKeyStore,
) -> Result<(), String> {
    validate_record_ids(candidate)?;
    for old in &previous.keys {
        let Some(new) = candidate.keys.iter().find(|record| record.id == old.id) else {
            return Err("API key records cannot be removed; revoke the key instead".to_string());
        };
        if old.revoked_at.is_some() && old.revoked_at != new.revoked_at {
            return Err("revoked API keys cannot be restored or rewritten".to_string());
        }
        crate::api_key_quota::validate_policy_change(
            transaction,
            &old.id,
            old.access.quota.as_ref(),
            new.access.quota.as_ref(),
        )?;
    }
    Ok(())
}

fn read_records(connection: &Connection) -> Result<ApiKeyStore, String> {
    let mut statement = connection
        .prepare("SELECT id, record_json FROM managed_api_keys ORDER BY rowid")
        .map_err(|err| format!("failed to read API-key registry: {err}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("failed to query API-key registry: {err}"))?;
    let mut keys = Vec::new();
    for row in rows {
        let (id, json) = row.map_err(|err| format!("failed to read API-key record: {err}"))?;
        let record: ApiKeyRecord = serde_json::from_str(&json)
            .map_err(|err| format!("invalid persistent API-key record: {err}"))?;
        if record.id != id {
            return Err(
                "persistent API-key record ID does not match its registry identity".to_string(),
            );
        }
        keys.push(record);
    }
    let store = ApiKeyStore { keys }.normalized()?;
    validate_record_ids(&store)?;
    Ok(store)
}

fn read_optional_file(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!(
            "failed to read API key store '{}': {err}",
            path.display()
        )),
    }
}

fn is_migration_sentinel(data: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(data)
        .ok()
        .is_some_and(|value| {
            value.get("storage").and_then(serde_json::Value::as_str)
                == Some(API_KEYS_SQLITE_SENTINEL)
                && value.get("keys").is_some_and(serde_json::Value::is_string)
        })
}

pub(crate) fn write_migration_sentinel(
    path: &std::path::Path,
    database_id: &str,
) -> Result<(), String> {
    let sentinel = serde_json::json!({
        "storage": API_KEYS_SQLITE_SENTINEL,
        "database_id": database_id,
        "keys": "Migrated to api-key-policy.sqlite3. This file is not an API-key registry. Do not run an older gateway or replace this marker."
    });
    crate::target::atomic_write(path, &serde_json::to_vec_pretty(&sentinel).unwrap(), true)
        .map_err(|err| format!("failed to retire legacy API-key authority: {err}"))
}

fn migrate_legacy_keys(cfg: &crate::Config, connection: &mut Connection) -> Result<(), String> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| format!("failed to begin API-key migration: {err}"))?;
    let (database_id, initialized): (String, bool) = transaction.query_row(
        "SELECT database_id, keys_initialized FROM managed_registry_metadata WHERE singleton = 1",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(|err| format!("failed to read API-key registry initialization: {err}"))?;
    let path = api_keys_path(cfg);
    let legacy_data = read_optional_file(&path)?;
    if initialized {
        match legacy_data {
            Some(data) if is_migration_sentinel(&data) => {
                let marker: serde_json::Value = serde_json::from_slice(&data).unwrap();
                if marker.get("database_id").and_then(serde_json::Value::as_str) != Some(database_id.as_str()) {
                    return Err("legacy API-key marker does not match the established database".to_string());
                }
            }
            None => write_migration_sentinel(&path, &database_id)?,
            Some(_) => return Err("legacy API-key JSON unexpectedly replaced the migrated marker; refuse stale authority".to_string()),
        }
        return Ok(());
    }
    let backup = path.with_file_name(API_KEYS_MIGRATION_BACKUP);
    let data = match legacy_data {
        Some(data) if is_migration_sentinel(&data) => {
            let marker: serde_json::Value = serde_json::from_slice(&data).unwrap();
            if marker
                .get("database_id")
                .and_then(serde_json::Value::as_str)
                != Some(database_id.as_str())
            {
                return Err("API-key migration marker belongs to a different database".to_string());
            }
            read_optional_file(&backup)?.ok_or_else(|| {
                "interrupted API-key migration is missing its recovery backup".to_string()
            })?
        }
        Some(data) => data,
        None => serde_json::to_vec(&ApiKeyStore::default()).unwrap(),
    };
    let store = serde_json::from_slice::<ApiKeyStore>(&data)
        .map_err(|err| format!("failed to parse API key store '{}': {err}", path.display()))?
        .normalized()?;
    validate_record_ids(&store)?;
    if store
        .keys
        .iter()
        .any(|record| record.access.input_token_budget.is_some() || record.access.quota.is_some())
        && !crate::api_key_policy_store::database_existed_before_initialization(
            &crate::api_key_policy_store::policy_db_path(cfg),
        )?
    {
        return Err(MISSING_LEGACY_ACCOUNTING.to_string());
    }
    match read_optional_file(&backup)? {
        Some(existing) if existing != data => {
            return Err("API-key migration backup differs from its source; refusing to overwrite recovery data".to_string());
        }
        Some(_) => {}
        None => crate::target::atomic_write(&backup, &data, true)
            .map_err(|err| format!("failed to preserve API-key migration backup: {err}"))?,
    }
    // Retire the old authority before committing the import. If the process
    // crashes here, the private backup above is the restartable source.
    write_migration_sentinel(&path, &database_id)?;
    for record in &store.keys {
        transaction
            .execute(
                "INSERT INTO managed_api_keys(id, record_json) VALUES(?1, ?2)",
                params![
                    record.id,
                    serde_json::to_string(record).map_err(|err| err.to_string())?
                ],
            )
            .map_err(|err| format!("failed to import legacy API key: {err}"))?;
        transaction.execute(
            "INSERT INTO managed_api_key_events(api_key_id, event_kind, occurred_at) VALUES(?1, 'migrated', ?2)",
            params![record.id, chrono::Utc::now().to_rfc3339()],
        ).map_err(|err| format!("failed to audit API-key migration: {err}"))?;
    }
    transaction
        .execute(
            "UPDATE managed_registry_metadata SET keys_initialized = 1 WHERE singleton = 1",
            [],
        )
        .map_err(|err| format!("failed to initialize API-key registry: {err}"))?;
    transaction
        .commit()
        .map_err(|err| format!("failed to commit API-key migration: {err}"))
}

pub(crate) fn bootstrap_legacy_key(
    store: &mut ApiKeyStore,
    raw_key: &str,
    now: &str,
) -> Result<bool, String> {
    let raw_key = raw_key.trim();
    // A configured proxy key is a singleton compatibility credential, not a
    // growing set. Build a possible replacement before touching the existing
    // records so a hashing error cannot leave an in-memory partial rotation.
    let replacement = if raw_key.is_empty() || find_matching_index(store, raw_key, true).is_some() {
        None
    } else {
        Some(ApiKeyRecord {
            id: Uuid::new_v4().simple().to_string(),
            label: LEGACY_PROXY_API_KEY_LABEL.to_string(),
            key_prefix: key_prefix(raw_key),
            lookup_hash: lookup_hash(raw_key),
            hash: hash_api_key(raw_key)?,
            created_at: now.to_string(),
            last_used_at: None,
            revoked_at: None,
            source: ApiKeySource::LegacyConfig,
            access: ApiKeyAccess::default(),
        })
    };

    let mut changed = false;
    if let Some(replacement) = replacement.as_ref() {
        // Rotation changes a credential, not its logical key identity. Keep
        // the active key's rules, anchors and spent balances. Retain a revoked
        // hash-only tombstone so restoring an older config cannot reactivate
        // a retired secret. Explicitly revoked keys are never chosen here.
        if let Some(index) = store
            .keys
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                record.source == ApiKeySource::LegacyConfig && record.revoked_at.is_none()
            })
            .max_by(|(_, left), (_, right)| left.created_at.cmp(&right.created_at))
            .map(|(index, _)| index)
        {
            let mut retired_credential = store.keys[index].clone();
            retired_credential.id = Uuid::new_v4().simple().to_string();
            retired_credential.revoked_at = Some(now.to_string());
            store.keys[index].hash = replacement.hash.clone();
            store.keys[index].lookup_hash = replacement.lookup_hash.clone();
            store.keys[index].key_prefix = replacement.key_prefix.clone();
            store.keys.push(retired_credential);
            changed = true;
        }
    }
    let mut kept_current_legacy_record = false;
    for record in &mut store.keys {
        if record.source != ApiKeySource::LegacyConfig || record.revoked_at.is_some() {
            continue;
        }
        // An empty configuration explicitly removes the compatibility key.
        // Otherwise preserve exactly one legacy record that authenticates the
        // current config value and retire every prior generation (including
        // duplicate records from an older concurrent startup race).
        if !raw_key.is_empty() && !kept_current_legacy_record && verify_hash(record, raw_key) {
            kept_current_legacy_record = true;
            continue;
        }
        record.revoked_at = Some(now.to_string());
        changed = true;
    }
    if !kept_current_legacy_record {
        if let Some(replacement) = replacement {
            store.keys.push(replacement);
            changed = true;
        }
    }
    Ok(changed)
}

pub(crate) fn verify_token(store: &ApiKeyStore, raw_key: &str) -> Option<String> {
    find_matching_index(store, raw_key, false).map(|index| store.keys[index].id.clone())
}

pub(crate) fn token_lookup_hash(raw_key: &str) -> String {
    lookup_hash(raw_key.trim())
}

pub(crate) fn verification_candidates(store: &ApiKeyStore, raw_key: &str) -> Vec<ApiKeyRecord> {
    let raw_key = raw_key.trim();
    if raw_key.is_empty() {
        return Vec::new();
    }
    let candidate_lookup_hash = lookup_hash(raw_key);
    store
        .keys
        .iter()
        .filter(|record| {
            record.revoked_at.is_none()
                && (record.lookup_hash.trim().is_empty()
                    || record.lookup_hash == candidate_lookup_hash)
        })
        .cloned()
        .collect()
}

pub(crate) fn verify_record(record: &ApiKeyRecord, raw_key: &str) -> bool {
    record.revoked_at.is_none() && verify_hash(record, raw_key.trim())
}

pub(crate) fn touch_last_used(store: &mut ApiKeyStore, id: &str, now: &str) -> bool {
    let Some(record) = store.keys.iter_mut().find(|record| record.id == id) else {
        return false;
    };
    if record.revoked_at.is_some() || same_minute(record.last_used_at.as_deref(), Some(now)) {
        return false;
    }
    record.last_used_at = Some(now.to_string());
    true
}

pub(crate) fn create_key(
    store: &mut ApiKeyStore,
    label: &str,
    access: &ApiKeyAccess,
    now: &str,
) -> Result<CreatedApiKey, String> {
    let normalized_label = normalize_label(label);
    let access = access.normalized()?;
    let plain_text_key = generate_api_key();
    let record = ApiKeyRecord {
        id: Uuid::new_v4().simple().to_string(),
        label: normalized_label,
        key_prefix: key_prefix(&plain_text_key),
        lookup_hash: lookup_hash(&plain_text_key),
        hash: hash_api_key(&plain_text_key)?,
        created_at: now.to_string(),
        last_used_at: None,
        revoked_at: None,
        source: ApiKeySource::Managed,
        access,
    };
    let public = public_record(&record);
    store.keys.push(record);
    Ok(CreatedApiKey {
        key: public,
        plain_text_key,
    })
}

pub(crate) fn update_access(
    store: &mut ApiKeyStore,
    id: &str,
    access: &ApiKeyAccess,
) -> Result<bool, String> {
    let access = access.normalized()?;
    let Some(record) = store.keys.iter_mut().find(|record| record.id == id) else {
        return Err("API key not found".to_string());
    };
    if record.revoked_at.is_some() {
        return Err("Revoked API key access cannot be changed".to_string());
    }
    if record.access == access {
        return Ok(false);
    }
    record.access = access;
    Ok(true)
}

pub(crate) fn revoke_key(store: &mut ApiKeyStore, id: &str, now: &str) -> Result<bool, String> {
    let Some(record) = store.keys.iter_mut().find(|record| record.id == id) else {
        return Err("API key not found".to_string());
    };
    if record.revoked_at.is_some() {
        return Ok(false);
    }
    record.revoked_at = Some(now.to_string());
    Ok(true)
}

pub(crate) fn public_records(store: &ApiKeyStore) -> Vec<PublicApiKeyRecord> {
    let mut out = store
        .keys
        .iter()
        .map(public_record)
        .collect::<Vec<PublicApiKeyRecord>>();
    out.sort_by(
        |left, right| match (left.revoked_at.is_some(), right.revoked_at.is_some()) {
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
            _ => right.created_at.cmp(&left.created_at),
        },
    );
    out
}

fn public_record(record: &ApiKeyRecord) -> PublicApiKeyRecord {
    PublicApiKeyRecord {
        id: record.id.clone(),
        label: record.label.clone(),
        key_prefix: record.key_prefix.clone(),
        created_at: record.created_at.clone(),
        last_used_at: record.last_used_at.clone(),
        revoked_at: record.revoked_at.clone(),
        source: record.source,
        access: record.access.clone(),
    }
}

fn normalize_label(label: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        "API key".to_string()
    } else {
        label.to_string()
    }
}

fn key_prefix(raw_key: &str) -> String {
    let visible = raw_key.chars().take(12).collect::<String>();
    if raw_key.chars().count() > 12 {
        format!("{}...", visible)
    } else {
        visible
    }
}

fn lookup_hash(raw_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_key.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hash_api_key(raw_key: &str) -> Result<String, String> {
    let salt = SaltString::encode_b64(&Uuid::new_v4().into_bytes())
        .map_err(|err| format!("failed to encode API key salt: {}", err))?;
    Argon2::default()
        .hash_password(raw_key.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| format!("failed to hash API key: {}", err))
}

fn verify_hash(record: &ApiKeyRecord, raw_key: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(&record.hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(raw_key.as_bytes(), &parsed_hash)
        .is_ok()
}

fn find_matching_index(store: &ApiKeyStore, raw_key: &str, include_revoked: bool) -> Option<usize> {
    let raw_key = raw_key.trim();
    if raw_key.is_empty() {
        return None;
    }
    let candidate_lookup_hash = lookup_hash(raw_key);
    store.keys.iter().position(|record| {
        if !include_revoked && record.revoked_at.is_some() {
            return false;
        }
        if !record.lookup_hash.trim().is_empty() && record.lookup_hash != candidate_lookup_hash {
            return false;
        }
        verify_hash(record, raw_key)
    })
}

fn same_minute(left: Option<&str>, right: Option<&str>) -> bool {
    minute_bucket(left) == minute_bucket(right)
}

fn minute_bucket(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| value.get(..16))
}

fn generate_api_key() -> String {
    format!(
        "cgw_{}_{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> crate::Config {
        crate::Config {
            listen: "127.0.0.1:0".to_string(),
            upstream_base: "https://example.test".to_string(),
            proxy_api_key: "legacy-test-key".to_string(),
            tokens: Vec::new(),
            auth_dir: None,
            disabled_files: None,
            admin_auth: crate::admin_auth::AdminAuthConfig::default(),
            oauth: crate::target::oauth::OAuthConfig::default(),
            request_body_limit_enabled: crate::default_request_body_limit_enabled(),
            max_request_body_bytes: crate::default_max_request_body_bytes(),
            max_concurrent_requests: crate::default_max_concurrent_requests(),
            trusted_proxy: false,
            history_retention_days: crate::default_history_retention_days(),
            history_max_entries: crate::default_history_max_entries(),
            upstream_connect_timeout_seconds: crate::default_upstream_connect_timeout_seconds(),
            upstream_read_timeout_seconds: crate::default_upstream_read_timeout_seconds(),
            upstream_first_event_timeout_seconds:
                crate::default_upstream_first_event_timeout_seconds(),
        }
    }

    #[test]
    fn create_verify_and_revoke_api_key() {
        let mut store = ApiKeyStore::default();
        let created = create_key(
            &mut store,
            "Primary",
            &ApiKeyAccess::default(),
            "2026-07-11T00:00:00Z",
        )
        .unwrap();
        let key_id = verify_token(&store, &created.plain_text_key);
        assert_eq!(key_id.as_deref(), Some(created.key.id.as_str()));

        assert!(touch_last_used(
            &mut store,
            &created.key.id,
            "2026-07-11T00:01:00Z"
        ));
        assert!(store.keys[0].last_used_at.is_some());

        assert!(revoke_key(&mut store, &created.key.id, "2026-07-11T00:02:00Z").unwrap());
        assert!(verify_token(&store, &created.plain_text_key).is_none());
    }

    #[test]
    fn bootstrap_legacy_key_does_not_restore_revoked_key() {
        let mut store = ApiKeyStore::default();
        assert!(bootstrap_legacy_key(&mut store, "legacy-secret", "2026-07-11T00:00:00Z").unwrap());
        let key_id = store.keys[0].id.clone();
        assert!(revoke_key(&mut store, &key_id, "2026-07-11T00:05:00Z").unwrap());
        assert!(
            !bootstrap_legacy_key(&mut store, "legacy-secret", "2026-07-11T00:06:00Z").unwrap()
        );
        assert_eq!(store.keys.len(), 1);
    }

    #[test]
    fn bootstrap_legacy_key_rotation_retires_prior_legacy_credentials() {
        let mut store = ApiKeyStore::default();
        assert!(bootstrap_legacy_key(&mut store, "legacy-before", "2026-07-11T00:00:00Z").unwrap());
        let old_id = store.keys[0].id.clone();
        store.keys[0].access.prompt_token_limit = Some(100);
        store.keys[0].access.input_token_budget = Some(ApiKeyInputTokenBudget {
            limit: 1000,
            period: ApiKeyBudgetPeriod::Lifetime,
        });
        let original_access = store.keys[0].access.clone();

        assert!(bootstrap_legacy_key(&mut store, "legacy-after", "2026-07-11T00:01:00Z").unwrap());
        assert!(verify_token(&store, "legacy-before").is_none());
        assert_eq!(
            verify_token(&store, "legacy-after").as_deref(),
            Some(old_id.as_str())
        );
        assert_eq!(
            store
                .keys
                .iter()
                .find(|record| record.id == old_id)
                .and_then(|record| record.revoked_at.as_deref()),
            None
        );
        assert_eq!(
            store
                .keys
                .iter()
                .find(|record| record.id == old_id)
                .unwrap()
                .access,
            original_access
        );
        assert!(store
            .keys
            .iter()
            .any(|record| record.revoked_at.is_some() && verify_hash(record, "legacy-before")));

        // Re-reading the same configuration is a no-op, while removing the
        // compatibility key retires the final active legacy record too.
        assert!(!bootstrap_legacy_key(&mut store, "legacy-after", "2026-07-11T00:02:00Z").unwrap());
        assert!(bootstrap_legacy_key(&mut store, "", "2026-07-11T00:03:00Z").unwrap());
        assert!(store
            .keys
            .iter()
            .filter(|record| record.source == ApiKeySource::LegacyConfig)
            .all(|record| record.revoked_at.is_some()));
    }

    #[test]
    fn legacy_secret_rotation_preserves_persisted_budget_and_rejects_rollback() {
        let dir = tempfile_dir();
        let mut cfg = test_config();
        cfg.auth_dir = Some(dir.to_string_lossy().to_string());
        let mut store = ApiKeyStore::default();
        bootstrap_legacy_key(&mut store, "first-credential", "2026-09-01T00:00:00Z").unwrap();
        let id = store.keys[0].id.clone();
        store.keys[0].access.input_token_budget = Some(ApiKeyInputTokenBudget {
            limit: 50,
            period: ApiKeyBudgetPeriod::Lifetime,
        });
        store.keys[0].access.quota = Some(crate::api_key_quota::QuotaPolicy {
            timezone: "UTC".into(),
            rules: vec![crate::api_key_quota::QuotaRule {
                metric: crate::api_key_quota::QuotaMetric::Requests,
                period: crate::api_key_quota::QuotaPeriod::Weekly,
                limit: 1,
            }],
        });
        save(&cfg, &store).unwrap();
        let first_use = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let quota_request = crate::api_key_quota::ReserveRequest {
            api_key_id: id.clone(),
            request_id: "rotation-request".into(),
            attempt_id: "rotation-attempt".into(),
            provider: Some("claude".into()),
            account_key: None,
            expected_legacy_budget: store.keys[0].access.input_token_budget.clone(),
            bounds: Default::default(),
        };
        assert!(matches!(
            crate::api_key_quota::reserve_at_path(
                &crate::api_key_policy_store::policy_db_path(&cfg),
                &quota_request,
                first_use
            )
            .unwrap(),
            crate::api_key_quota::Admission::Reserved(_)
        ));
        let connection = registry_connection(&cfg).unwrap();
        connection.execute(
            "INSERT INTO api_key_budget_windows VALUES(?1, 'lifetime', 'lifetime', 40, 10, '2026-09-01T00:00:00Z')",
            params![id],
        ).unwrap();
        drop(connection);
        bootstrap_legacy_key(&mut store, "second-credential", "2026-09-02T00:00:00Z").unwrap();
        save(&cfg, &store).unwrap();
        let mut reopened = load(&cfg).unwrap();
        assert_eq!(
            verify_token(&reopened, "second-credential").as_deref(),
            Some(id.as_str())
        );
        assert!(verify_token(&reopened, "first-credential").is_none());
        assert_eq!(
            reopened
                .keys
                .iter()
                .find(|record| record.id == id)
                .unwrap()
                .access
                .input_token_budget
                .as_ref()
                .unwrap()
                .limit,
            50
        );
        let connection = registry_connection(&cfg).unwrap();
        let used: i64 = connection.query_row("SELECT committed_input_tokens + reserved_input_tokens FROM api_key_budget_windows WHERE api_key_id=?1", params![id], |row| row.get(0)).unwrap();
        assert_eq!(used, 50);
        drop(connection);
        let quota_summary = crate::api_key_quota::summary_at_path(
            &crate::api_key_policy_store::policy_db_path(&cfg),
            &id,
            first_use + chrono::Duration::days(1),
        )
        .unwrap()
        .unwrap();
        assert_eq!(quota_summary.rules[0].reserved, 1);
        assert_eq!(quota_summary.rules[0].remaining, 0);
        assert_eq!(
            quota_summary.anchor.as_deref(),
            Some("2026-09-01T00:00:00.000000000Z")
        );
        bootstrap_legacy_key(&mut reopened, "first-credential", "2026-09-03T00:00:00Z").unwrap();
        save(&cfg, &reopened).unwrap();
        assert!(verify_token(&load(&cfg).unwrap(), "first-credential").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saves_and_loads_store() {
        let dir = tempfile_dir();
        let mut cfg = test_config();
        cfg.auth_dir = Some(dir.to_string_lossy().to_string());

        let mut store = ApiKeyStore::default();
        let created = create_key(
            &mut store,
            "Persisted",
            &ApiKeyAccess::default(),
            "2026-07-11T00:00:00Z",
        )
        .unwrap();
        save(&cfg, &store).unwrap();

        let loaded = load(&cfg).unwrap();
        assert_eq!(loaded.keys.len(), 1);
        assert_eq!(
            verify_token(&loaded, &created.plain_text_key).as_deref(),
            Some(loaded.keys[0].id.as_str())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sqlite_migration_preserves_key_identity_policy_and_old_budget_balances() {
        let dir = tempfile_dir();
        let mut cfg = test_config();
        cfg.auth_dir = Some(dir.to_string_lossy().to_string());
        let store = ApiKeyStore {
            keys: vec![
                ApiKeyRecord {
                    id: "persistent-key".to_string(),
                    label: "preserved".to_string(),
                    hash: "original-password-hash".to_string(),
                    lookup_hash: "original-lookup-hash".to_string(),
                    source: ApiKeySource::LegacyConfig,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    last_used_at: Some("2026-08-31T15:00:00Z".to_string()),
                    access: ApiKeyAccess {
                        input_token_budget: Some(ApiKeyInputTokenBudget {
                            limit: 1000,
                            period: ApiKeyBudgetPeriod::Lifetime,
                        }),
                        ..ApiKeyAccess::default()
                    },
                    ..ApiKeyRecord::default()
                },
                ApiKeyRecord {
                    id: "revoked-key".to_string(),
                    revoked_at: Some("2026-09-01T00:00:00Z".to_string()),
                    ..ApiKeyRecord::default()
                },
            ],
        };
        let legacy_bytes = serde_json::to_vec_pretty(&store).unwrap();
        std::fs::write(api_keys_path(&cfg), &legacy_bytes).unwrap();
        // This is an existing pre-registry database: no identity marker or
        // managed key tables, but real conservative usage must survive.
        let db_path = crate::api_key_policy_store::policy_db_path(&cfg);
        let connection = Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE api_key_budget_windows (
                api_key_id TEXT NOT NULL, budget_period TEXT NOT NULL,
                window_start TEXT NOT NULL, committed_input_tokens INTEGER NOT NULL,
                reserved_input_tokens INTEGER NOT NULL, updated_at TEXT NOT NULL,
                PRIMARY KEY(api_key_id, budget_period, window_start));
             INSERT INTO api_key_budget_windows VALUES
                ('persistent-key', 'lifetime', 'lifetime', 123, 45, '2026-09-01T00:00:00Z');
             CREATE TABLE api_key_budget_reservations (
                reservation_id TEXT PRIMARY KEY, request_id TEXT NOT NULL,
                api_key_id TEXT NOT NULL, budget_limit INTEGER NOT NULL,
                budget_period TEXT NOT NULL, window_start TEXT NOT NULL,
                reserved_input_tokens INTEGER NOT NULL, committed_input_tokens INTEGER,
                state TEXT NOT NULL, created_at TEXT NOT NULL, settled_at TEXT);
             INSERT INTO api_key_budget_reservations VALUES
                ('pre-upgrade-hold', 'pre-upgrade-request', 'persistent-key', 1000,
                 'lifetime', 'lifetime', 45, NULL, 'active', '2026-09-01T00:00:00Z', NULL);",
            )
            .unwrap();
        drop(connection);
        assert_eq!(load(&cfg).unwrap(), store);
        assert_eq!(load(&cfg).unwrap(), store);
        assert_eq!(
            std::fs::read(dir.join(API_KEYS_MIGRATION_BACKUP)).unwrap(),
            legacy_bytes
        );
        let sentinel = std::fs::read(api_keys_path(&cfg)).unwrap();
        assert!(is_migration_sentinel(&sentinel));
        assert!(
            serde_json::from_slice::<ApiKeyStore>(&sentinel).is_err(),
            "old binaries must not read an empty permissive registry"
        );
        let connection = registry_connection(&cfg).unwrap();
        let balances: (i64, i64) = connection.query_row(
            "SELECT committed_input_tokens, reserved_input_tokens FROM api_key_budget_windows WHERE api_key_id = 'persistent-key'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(balances, (123, 45));
        let old_hold: (String, i64) = connection.query_row(
            "SELECT state,reserved_input_tokens FROM api_key_budget_reservations WHERE reservation_id='pre-upgrade-hold'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(old_hold, ("active".to_string(), 45));
        let imported_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM managed_api_key_events WHERE event_kind = 'migrated'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(imported_count, 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [
                db_path,
                dir.join(API_KEYS_MIGRATION_BACKUP),
                api_keys_path(&cfg),
                dir.join("api-key-policy.sqlite3.identity"),
            ] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        drop(connection);
        crate::api_key_policy_store::settle_input_token_reservation(
            &cfg,
            "pre-upgrade-hold",
            crate::api_key_policy_store::ApiKeyInputTokenSettlement::Commit {
                actual_input_tokens: None,
            },
        )
        .unwrap();
        let old_summary = crate::api_key_policy_store::input_token_budget_summary(
            &cfg,
            "persistent-key",
            &ApiKeyInputTokenBudget {
                limit: 1000,
                period: ApiKeyBudgetPeriod::Lifetime,
            },
        )
        .unwrap();
        assert_eq!(old_summary.committed_input_tokens, 168);
        assert_eq!(old_summary.reserved_input_tokens, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_sqlite_migration_resumes_after_legacy_authority_retired() {
        let dir = tempfile_dir();
        let mut cfg = test_config();
        cfg.auth_dir = Some(dir.to_string_lossy().to_string());
        let connection = registry_connection(&cfg).unwrap();
        let database_id: String = connection
            .query_row(
                "SELECT database_id FROM managed_registry_metadata",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        let store = ApiKeyStore {
            keys: vec![ApiKeyRecord {
                id: "recovered-key".to_string(),
                ..ApiKeyRecord::default()
            }],
        };
        crate::target::atomic_write(
            &dir.join(API_KEYS_MIGRATION_BACKUP),
            &serde_json::to_vec(&store).unwrap(),
            true,
        )
        .unwrap();
        write_migration_sentinel(&api_keys_path(&cfg), &database_id).unwrap();
        // Simulates death after file retirement but before the DB commit.
        assert_eq!(load(&cfg).unwrap(), store);
        assert_eq!(load(&cfg).unwrap(), store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_json_and_stale_store_cannot_restore_revoked_keys() {
        let dir = tempfile_dir();
        let mut cfg = test_config();
        cfg.auth_dir = Some(dir.to_string_lossy().to_string());
        let original = ApiKeyStore {
            keys: vec![ApiKeyRecord {
                id: "key".to_string(),
                ..ApiKeyRecord::default()
            }],
        };
        save(&cfg, &original).unwrap();
        let mut revoked = original.clone();
        revoke_key(&mut revoked, "key", "2026-09-01T00:00:00Z").unwrap();
        save(&cfg, &revoked).unwrap();
        assert!(save(&cfg, &original)
            .unwrap_err()
            .contains("cannot be restored"));
        assert!(save(&cfg, &ApiKeyStore::default())
            .unwrap_err()
            .contains("cannot be removed"));
        assert_eq!(load(&cfg).unwrap(), revoked);
        std::fs::write(api_keys_path(&cfg), serde_json::to_vec(&original).unwrap()).unwrap();
        assert!(load(&cfg).unwrap_err().contains("refuse stale authority"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_established_database_does_not_reimport_backup_or_reset_keys() {
        let dir = tempfile_dir();
        let mut cfg = test_config();
        cfg.auth_dir = Some(dir.to_string_lossy().to_string());
        save(
            &cfg,
            &ApiKeyStore {
                keys: vec![ApiKeyRecord {
                    id: "key".to_string(),
                    ..ApiKeyRecord::default()
                }],
            },
        )
        .unwrap();
        std::fs::rename(
            crate::api_key_policy_store::policy_db_path(&cfg),
            dir.join("original.sqlite3"),
        )
        .unwrap();
        assert!(load(&cfg).unwrap_err().contains("refusing to reset usage"));
        assert!(!crate::api_key_policy_store::policy_db_path(&cfg).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_budget_migration_refuses_missing_accounting_even_after_restart() {
        let dir = tempfile_dir();
        let mut cfg = test_config();
        cfg.auth_dir = Some(dir.to_string_lossy().into_owned());
        let original = serde_json::to_vec(&ApiKeyStore {
            keys: vec![ApiKeyRecord {
                id: "previously-budgeted-key".into(),
                access: ApiKeyAccess {
                    input_token_budget: Some(ApiKeyInputTokenBudget {
                        limit: 100,
                        period: ApiKeyBudgetPeriod::Lifetime,
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }],
        })
        .unwrap();
        std::fs::write(api_keys_path(&cfg), &original).unwrap();
        for index in 0..2 {
            let error = load(&cfg).unwrap_err();
            assert!(error.contains("previous accounting database is missing"));
            assert!(error.contains("Restore the original"));
            assert_eq!(std::fs::read(api_keys_path(&cfg)).unwrap(), original);
            assert!(!dir.join(API_KEYS_MIGRATION_BACKUP).exists());
            if index == 0 {
                assert!(
                    !crate::api_key_policy_store::policy_db_path(&cfg).exists(),
                    "preflight must not create a misleading fresh ledger"
                );
            }
            // Even if an unrelated policy reader initializes an empty DB,
            // persisted first-initialization evidence still blocks import.
            let connection = registry_connection(&cfg).unwrap();
            let count: i64 = connection
                .query_row("SELECT COUNT(*) FROM managed_api_keys", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
            let initialized: bool = connection
                .query_row(
                    "SELECT keys_initialized FROM managed_registry_metadata",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!initialized);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_budget_migration_refuses_empty_partial_or_unrelated_existing_accounting() {
        for fixture in [
            "empty",
            "unrelated",
            "window-only",
            "reservation-only",
            "wrong-columns",
            "corrupt",
            "truncated",
        ] {
            let dir = tempfile_dir();
            let mut cfg = test_config();
            cfg.auth_dir = Some(dir.to_string_lossy().into_owned());
            let original_keys = serde_json::to_vec(&ApiKeyStore {
                keys: vec![ApiKeyRecord {
                    id: "previously-budgeted-key".into(),
                    access: ApiKeyAccess {
                        input_token_budget: Some(ApiKeyInputTokenBudget {
                            limit: 100,
                            period: ApiKeyBudgetPeriod::Lifetime,
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                }],
            })
            .unwrap();
            std::fs::write(api_keys_path(&cfg), &original_keys).unwrap();
            let db_path = crate::api_key_policy_store::policy_db_path(&cfg);
            match fixture {
                "empty" => std::fs::write(&db_path, b"").unwrap(),
                "corrupt" => std::fs::write(&db_path, b"not a SQLite ledger").unwrap(),
                other => {
                    let connection = Connection::open(&db_path).unwrap();
                    let schema = match other {
                        "window-only" => "CREATE TABLE api_key_budget_windows (api_key_id TEXT)",
                        "reservation-only" => {
                            "CREATE TABLE api_key_budget_reservations (reservation_id TEXT)"
                        }
                        "wrong-columns" => {
                            "CREATE TABLE api_key_budget_windows (api_key_id TEXT);
                             CREATE TABLE api_key_budget_reservations (reservation_id TEXT);"
                        }
                        _ => {
                            "CREATE TABLE unrelated (private_data TEXT);
                              INSERT INTO unrelated VALUES ('must stay unchanged');"
                        }
                    };
                    connection.execute_batch(schema).unwrap();
                    drop(connection);
                    if fixture == "truncated" {
                        OpenOptions::new()
                            .write(true)
                            .open(&db_path)
                            .unwrap()
                            .set_len(100)
                            .unwrap();
                    }
                }
            }
            let original_db = std::fs::read(&db_path).unwrap();
            for _ in 0..2 {
                assert!(load(&cfg).is_err(), "must reject {fixture} legacy ledger");
                assert_eq!(std::fs::read(&db_path).unwrap(), original_db, "{fixture}");
                assert_eq!(
                    std::fs::read(api_keys_path(&cfg)).unwrap(),
                    original_keys,
                    "{fixture}"
                );
                assert!(!dir.join(API_KEYS_MIGRATION_BACKUP).exists(), "{fixture}");
                assert!(
                    !db_path.with_extension("sqlite3.identity").exists(),
                    "must not pin an invalid {fixture} ledger"
                );
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn load_fails_closed_for_corrupt_or_invalid_persistent_policy() {
        let dir = tempfile_dir();
        let mut cfg = test_config();
        cfg.auth_dir = Some(dir.to_string_lossy().to_string());
        let path = api_keys_path(&cfg);

        std::fs::write(&path, b"{ not JSON").unwrap();
        assert!(load(&cfg)
            .unwrap_err()
            .contains("failed to parse API key store"));

        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "keys": [{
                    "id": "key-1",
                    "access": {
                        "all": true,
                        "max_estimated_input_tokens_per_request": 0
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(load(&cfg)
            .unwrap_err()
            .contains("must be greater than zero"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn canonical_request_limit_name_serializes_and_legacy_name_deserializes() {
        let legacy: ApiKeyAccess = serde_json::from_value(serde_json::json!({
            "all": true,
            "prompt_token_limit": 321
        }))
        .unwrap();
        assert_eq!(legacy.prompt_token_limit, Some(321));

        let canonical = serde_json::to_value(legacy).unwrap();
        assert_eq!(
            canonical["max_estimated_input_tokens_per_request"],
            serde_json::json!(321)
        );
        assert!(canonical.get("prompt_token_limit").is_none());
    }

    #[test]
    fn normalizes_and_enforces_restricted_provider_accounts() {
        let access = ApiKeyAccess {
            all: false,
            prompt_token_limit: None,
            input_token_budget: None,
            quota: None,
            providers: vec![
                ApiKeyProviderAccess {
                    provider: "cod".to_string(),
                    account_scope: ApiKeyAccountScope::Selected,
                    accounts: vec!["account-a".to_string(), "account-a".to_string()],
                    prompt_token_limit: Some(2000),
                    account_limits: vec![
                        ApiKeyAccountLimit {
                            account: "account-a".to_string(),
                            prompt_token_limit: Some(1000),
                        },
                        ApiKeyAccountLimit {
                            account: "account-a".to_string(),
                            prompt_token_limit: Some(800),
                        },
                    ],
                },
                ApiKeyProviderAccess {
                    provider: "claude".to_string(),
                    account_scope: ApiKeyAccountScope::All,
                    accounts: Vec::new(),
                    prompt_token_limit: None,
                    account_limits: Vec::new(),
                },
            ],
        }
        .normalized()
        .unwrap();

        assert!(access.allows_provider("codex"));
        assert_eq!(
            access.provider_rule("codex").unwrap().accounts,
            vec!["account-a"]
        );
        assert_eq!(
            access.provider_rule("codex").unwrap().prompt_token_limit,
            Some(2000)
        );
        assert_eq!(
            access
                .provider_rule("codex")
                .unwrap()
                .account_prompt_token_limit(|account| account.eq_ignore_ascii_case("account-a")),
            Some(800)
        );
        assert_eq!(
            access.provider_rule("cld").unwrap().account_scope,
            ApiKeyAccountScope::All
        );
        assert!(access
            .provider_rule("cld")
            .unwrap()
            .allows_account(|_| false));
        assert!(!access.allows_provider("gemini"));
    }

    #[test]
    fn restricted_access_requires_a_selection() {
        let err = ApiKeyAccess {
            all: false,
            prompt_token_limit: None,
            input_token_budget: None,
            quota: None,
            providers: Vec::new(),
        }
        .normalized()
        .unwrap_err();
        assert!(err.contains("at least one provider"));
    }

    #[test]
    fn legacy_account_lists_deserialize_into_explicit_scope_and_reserialize() {
        let access: ApiKeyAccess = serde_json::from_value(serde_json::json!({
            "all": false,
            "providers": [
                { "provider": "codex", "accounts": ["first.json"] },
                { "provider": "claude", "accounts": [] }
            ]
        }))
        .unwrap();

        assert_eq!(
            access.provider_rule("codex").unwrap().account_scope,
            ApiKeyAccountScope::Selected
        );
        assert_eq!(
            access.provider_rule("claude").unwrap().account_scope,
            ApiKeyAccountScope::All
        );

        let serialized = serde_json::to_value(access.normalized().unwrap()).unwrap();
        assert_eq!(
            serialized["providers"][0]["account_scope"],
            serde_json::json!("all")
        );
        assert_eq!(
            serialized["providers"][1]["account_scope"],
            serde_json::json!("selected")
        );
    }

    #[test]
    fn explicit_account_scope_requires_consistent_accounts() {
        let selected_without_accounts = ApiKeyAccess {
            all: false,
            prompt_token_limit: None,
            input_token_budget: None,
            quota: None,
            providers: vec![ApiKeyProviderAccess {
                provider: "codex".to_string(),
                account_scope: ApiKeyAccountScope::Selected,
                accounts: Vec::new(),
                prompt_token_limit: None,
                account_limits: Vec::new(),
            }],
        };
        assert!(selected_without_accounts
            .normalized()
            .unwrap_err()
            .contains("selected-account scope"));

        let all_with_accounts = ApiKeyAccess {
            all: false,
            prompt_token_limit: None,
            input_token_budget: None,
            quota: None,
            providers: vec![ApiKeyProviderAccess {
                provider: "codex".to_string(),
                account_scope: ApiKeyAccountScope::All,
                accounts: vec!["first.json".to_string()],
                prompt_token_limit: None,
                account_limits: Vec::new(),
            }],
        };
        assert!(all_with_accounts
            .normalized()
            .unwrap_err()
            .contains("all-account scope"));
    }

    #[test]
    fn zero_limits_are_rejected_in_every_scope() {
        for access in [
            ApiKeyAccess {
                all: true,
                prompt_token_limit: Some(0),
                input_token_budget: None,
                quota: None,
                providers: Vec::new(),
            },
            ApiKeyAccess {
                all: false,
                prompt_token_limit: None,
                input_token_budget: None,
                quota: None,
                providers: vec![ApiKeyProviderAccess {
                    provider: "codex".to_string(),
                    account_scope: ApiKeyAccountScope::All,
                    accounts: Vec::new(),
                    prompt_token_limit: Some(0),
                    account_limits: Vec::new(),
                }],
            },
            ApiKeyAccess {
                all: false,
                prompt_token_limit: None,
                input_token_budget: None,
                quota: None,
                providers: vec![ApiKeyProviderAccess {
                    provider: "codex".to_string(),
                    account_scope: ApiKeyAccountScope::All,
                    accounts: Vec::new(),
                    prompt_token_limit: None,
                    account_limits: vec![ApiKeyAccountLimit {
                        account: "first.json".to_string(),
                        prompt_token_limit: Some(0),
                    }],
                }],
            },
            // Validation also covers an ignored rule on an all-access key.
            ApiKeyAccess {
                all: true,
                prompt_token_limit: None,
                input_token_budget: None,
                quota: None,
                providers: vec![ApiKeyProviderAccess {
                    provider: "codex".to_string(),
                    account_scope: ApiKeyAccountScope::All,
                    accounts: Vec::new(),
                    prompt_token_limit: Some(0),
                    account_limits: Vec::new(),
                }],
            },
        ] {
            assert!(access.normalized().is_err());
        }
    }

    #[test]
    fn input_token_budget_requires_nonzero_limit_and_preserves_period() {
        let access = ApiKeyAccess {
            all: true,
            prompt_token_limit: Some(100),
            input_token_budget: Some(ApiKeyInputTokenBudget {
                limit: 5_000,
                period: ApiKeyBudgetPeriod::CalendarMonth,
            }),
            quota: None,
            providers: Vec::new(),
        }
        .normalized()
        .unwrap();
        assert_eq!(
            access.input_token_budget,
            Some(ApiKeyInputTokenBudget {
                limit: 5_000,
                period: ApiKeyBudgetPeriod::CalendarMonth,
            })
        );

        let zero_budget = ApiKeyAccess {
            all: true,
            prompt_token_limit: None,
            input_token_budget: Some(ApiKeyInputTokenBudget {
                limit: 0,
                period: ApiKeyBudgetPeriod::Lifetime,
            }),
            quota: None,
            providers: Vec::new(),
        };
        assert!(zero_budget
            .normalized()
            .unwrap_err()
            .contains("budget limit must be greater than zero"));

        let too_large_budget = ApiKeyAccess {
            all: true,
            prompt_token_limit: None,
            input_token_budget: Some(ApiKeyInputTokenBudget {
                limit: MAX_PERSISTED_INPUT_TOKENS + 1,
                period: ApiKeyBudgetPeriod::Lifetime,
            }),
            quota: None,
            providers: Vec::new(),
        };
        assert!(too_large_budget
            .normalized()
            .unwrap_err()
            .contains("SQLite signed integer range"));

        let maximum_safe_budget = ApiKeyAccess {
            all: true,
            prompt_token_limit: None,
            input_token_budget: Some(ApiKeyInputTokenBudget {
                limit: MAX_PERSISTED_INPUT_TOKENS,
                period: ApiKeyBudgetPeriod::Lifetime,
            }),
            quota: None,
            providers: Vec::new(),
        };
        assert!(maximum_safe_budget.normalized().is_ok());
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("io-gateway-api-keys-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
