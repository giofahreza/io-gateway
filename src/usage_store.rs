use rusqlite::{params, params_from_iter, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::PathBuf,
};

const HISTORY_DB_FILE: &str = "gateway-usage-history.sqlite3";
const LEGACY_HISTORY_FILE: &str = "gateway-usage-history.jsonl";

#[derive(Clone, Serialize, Deserialize)]
pub struct UsageHistoryEntry {
    pub recorded_at: String,
    pub provider: String,
    pub account_key: String,
    pub account_label: String,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub request_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_id: Option<String>,
    pub success: bool,
    pub error: bool,
    pub request_total: u64,
    pub prompt_total: u64,
    pub prompt_error_total: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cache_tokens: u64,
    pub reasoning_tokens: u64,
    pub input_chars: u64,
    pub prompt_items: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_usage: Option<serde_json::Value>,
}

#[derive(Default, Deserialize)]
pub struct UsageHistoryQuery {
    pub limit: Option<usize>,
    pub provider: Option<String>,
    pub account_key: Option<String>,
    pub api_key_id: Option<String>,
    pub model: Option<String>,
}

#[derive(Default, Deserialize)]
pub struct UsageFilterSummaryQuery {
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub providers: Option<String>,
    pub provider: Option<String>,
    pub account_keys: Option<String>,
    pub account_key: Option<String>,
    pub api_key_ids: Option<String>,
    pub api_key_id: Option<String>,
}

#[derive(Default, Serialize)]
pub struct UsageFilterSummary {
    pub requests: u64,
    pub successes: u64,
    pub errors: u64,
    pub prompt_total: u64,
    pub prompt_errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cache_tokens: u64,
    pub reasoning_tokens: u64,
    pub input_chars: u64,
    pub prompt_items: u64,
    pub first_recorded_at: Option<String>,
    pub last_recorded_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextAggregateRow {
    pub bucket_start: i64,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cache_tokens: u64,
    pub reasoning_tokens: u64,
    pub request_count: u64,
}

pub fn append(cfg: &crate::Config, entry: &UsageHistoryEntry) -> Result<(), String> {
    append_batch(cfg, std::slice::from_ref(entry))
}

pub fn append_batch(cfg: &crate::Config, entries: &[UsageHistoryEntry]) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut connection = open_connection(cfg)?;
    let transaction = connection
        .transaction()
        .map_err(|err| format!("failed to start history transaction: {}", err))?;
    {
        let mut statement = transaction
            .prepare_cached(
                "INSERT INTO usage_history (
                    recorded_at, provider, account_key, account_label, account_id,
                    credential_file, model, request_path, success, error,
                    request_total, prompt_total, prompt_error_total, input_tokens,
                    output_tokens, total_tokens, cache_tokens, reasoning_tokens,
                    input_chars, prompt_items, error_message, raw_usage, api_key_id
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                    ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23
                )",
            )
            .map_err(|err| err.to_string())?;
        for entry in entries {
            insert_entry(&mut statement, entry)?;
        }
    }
    transaction
        .commit()
        .map_err(|err| format!("failed to commit history transaction: {}", err))
}

pub fn load(
    cfg: &crate::Config,
    query: &UsageHistoryQuery,
) -> Result<Vec<UsageHistoryEntry>, String> {
    let connection = open_connection(cfg)?;
    let mut sql = format!("SELECT {} FROM usage_history", HISTORY_COLUMNS);
    let mut clauses = Vec::new();
    let mut values = Vec::<String>::new();
    if let Some(provider) = normalized_filter(query.provider.as_deref()) {
        clauses.push("provider = ? COLLATE NOCASE");
        values.push(provider.to_string());
    }
    if let Some(account_key) = normalized_filter(query.account_key.as_deref()) {
        clauses.push("account_key = ?");
        values.push(account_key.to_string());
    }
    if let Some(api_key_id) = normalized_filter(query.api_key_id.as_deref()) {
        clauses.push("api_key_id = ?");
        values.push(api_key_id.to_string());
    }
    if let Some(model) = normalized_filter(query.model.as_deref()) {
        clauses.push("model = ?");
        values.push(model.to_string());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }

    let limit = query.limit.unwrap_or(0);
    sql.push_str(if limit > 0 {
        " ORDER BY id DESC LIMIT ?"
    } else {
        " ORDER BY id ASC"
    });
    if limit > 0 {
        values.push(limit.to_string());
    }

    let mut statement = connection.prepare(&sql).map_err(|err| err.to_string())?;
    let rows = statement
        .query_map(params_from_iter(values.iter()), row_to_entry)
        .map_err(|err| err.to_string())?;
    let mut entries = rows
        .filter_map(|row| match row {
            Ok(entry) => Some(entry),
            Err(err) => {
                tracing::error!("failed to decode usage history row: {}", err);
                None
            }
        })
        .collect::<Vec<_>>();
    if limit > 0 {
        entries.reverse();
    }
    Ok(entries)
}

pub fn summarize_filtered(
    cfg: &crate::Config,
    query: &UsageFilterSummaryQuery,
) -> Result<UsageFilterSummary, String> {
    let connection = open_connection(cfg)?;
    let mut clauses = Vec::new();
    let mut values = Vec::<String>::new();

    if let Some(start_at) = normalized_filter(query.start_at.as_deref()) {
        clauses.push("recorded_at >= ?".to_string());
        values.push(start_at.to_string());
    }
    if let Some(end_at) = normalized_filter(query.end_at.as_deref()) {
        clauses.push("recorded_at <= ?".to_string());
        values.push(end_at.to_string());
    }
    push_list_clause(
        &mut clauses,
        &mut values,
        "lower(provider)",
        query.providers.as_deref().or(query.provider.as_deref()),
        true,
    );
    push_list_clause(
        &mut clauses,
        &mut values,
        "account_key",
        query
            .account_keys
            .as_deref()
            .or(query.account_key.as_deref()),
        false,
    );
    push_list_clause(
        &mut clauses,
        &mut values,
        "api_key_id",
        query.api_key_ids.as_deref().or(query.api_key_id.as_deref()),
        false,
    );

    let mut sql = "SELECT
            COALESCE(SUM(request_total), 0),
            COALESCE(SUM(CASE WHEN success != 0 THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN error != 0 THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(prompt_total), 0),
            COALESCE(SUM(prompt_error_total), 0),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(total_tokens), 0),
            COALESCE(SUM(cache_tokens), 0),
            COALESCE(SUM(reasoning_tokens), 0),
            COALESCE(SUM(input_chars), 0),
            COALESCE(SUM(prompt_items), 0),
            MIN(recorded_at),
            MAX(recorded_at)
        FROM usage_history"
        .to_string();
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }

    connection
        .query_row(&sql, params_from_iter(values.iter()), |row| {
            Ok(UsageFilterSummary {
                requests: row.get::<_, i64>(0)?.max(0) as u64,
                successes: row.get::<_, i64>(1)?.max(0) as u64,
                errors: row.get::<_, i64>(2)?.max(0) as u64,
                prompt_total: row.get::<_, i64>(3)?.max(0) as u64,
                prompt_errors: row.get::<_, i64>(4)?.max(0) as u64,
                input_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                output_tokens: row.get::<_, i64>(6)?.max(0) as u64,
                total_tokens: row.get::<_, i64>(7)?.max(0) as u64,
                cache_tokens: row.get::<_, i64>(8)?.max(0) as u64,
                reasoning_tokens: row.get::<_, i64>(9)?.max(0) as u64,
                input_chars: row.get::<_, i64>(10)?.max(0) as u64,
                prompt_items: row.get::<_, i64>(11)?.max(0) as u64,
                first_recorded_at: row.get(12)?,
                last_recorded_at: row.get(13)?,
            })
        })
        .map_err(|err| err.to_string())
}

pub fn aggregate_context(
    cfg: &crate::Config,
    cutoff: &str,
    bucket_seconds: u64,
    account_key: Option<&str>,
    per_model: bool,
) -> Result<Vec<ContextAggregateRow>, String> {
    let connection = open_connection(cfg)?;
    let model_expression = if per_model {
        "COALESCE(NULLIF(trim(model), ''), 'unknown')"
    } else {
        "NULL"
    };
    let account_clause = if account_key.is_some() {
        " AND account_key = ?3"
    } else {
        ""
    };
    let model_group = if per_model { ", model_key" } else { "" };
    let sql = format!(
        "SELECT (unixepoch(recorded_at) / ?1) * ?1 AS bucket_start,
                {model_expression} AS model_key,
                SUM(input_tokens), SUM(output_tokens), SUM(total_tokens),
                SUM(cache_tokens), SUM(reasoning_tokens), COUNT(*)
         FROM usage_history
         WHERE success = 1 AND recorded_at >= ?2{account_clause}
         GROUP BY bucket_start{model_group}
         ORDER BY bucket_start{model_group}"
    );
    let mut statement = connection.prepare(&sql).map_err(|err| err.to_string())?;
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok(ContextAggregateRow {
            bucket_start: row.get(0)?,
            model: row.get(1)?,
            input_tokens: row.get(2)?,
            output_tokens: row.get(3)?,
            total_tokens: row.get(4)?,
            cache_tokens: row.get(5)?,
            reasoning_tokens: row.get(6)?,
            request_count: row.get(7)?,
        })
    };
    let rows = if let Some(account_key) = account_key {
        statement
            .query_map(params![bucket_seconds.max(1), cutoff, account_key], map_row)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?
    } else {
        statement
            .query_map(params![bucket_seconds.max(1), cutoff], map_row)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?
    };
    Ok(rows)
}

pub fn latest_error_messages(
    cfg: &crate::Config,
) -> Result<HashMap<(String, String), crate::stats_store::LatestErrorMessage>, String> {
    let connection = open_connection(cfg)?;
    let mut statement = connection
        .prepare(
            "SELECT provider, account_key, recorded_at, error_message
             FROM usage_history
             WHERE error = 1 AND error_message IS NOT NULL AND trim(error_message) <> ''
             ORDER BY id ASC",
        )
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|err| err.to_string())?;
    let mut latest = HashMap::new();
    for row in rows {
        let (provider, account_key, recorded_at, error_message) =
            row.map_err(|err| err.to_string())?;
        latest.insert(
            (provider.to_ascii_lowercase(), account_key),
            crate::stats_store::LatestErrorMessage {
                recorded_at,
                error_message,
            },
        );
    }
    Ok(latest)
}

pub fn prune(cfg: &crate::Config, retention_days: u64, max_entries: usize) -> Result<(), String> {
    if retention_days == 0 && max_entries == 0 {
        return Ok(());
    }
    let mut connection = open_connection(cfg)?;
    let transaction = connection
        .transaction()
        .map_err(|err| format!("failed to start history prune: {}", err))?;
    if retention_days > 0 {
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::days(retention_days as i64)).to_rfc3339();
        transaction
            .execute(
                "DELETE FROM usage_history WHERE recorded_at < ?1",
                params![cutoff],
            )
            .map_err(|err| err.to_string())?;
    }
    if max_entries > 0 {
        transaction
            .execute(
                "DELETE FROM usage_history
                 WHERE id IN (
                    SELECT id FROM usage_history ORDER BY id DESC LIMIT -1 OFFSET ?1
                 )",
                params![max_entries as i64],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())?;
    connection
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .map_err(|err| err.to_string())
}

const HISTORY_COLUMNS: &str = "recorded_at, provider, account_key, account_label, account_id,
    credential_file, model, request_path, success, error, request_total, prompt_total,
    prompt_error_total, input_tokens, output_tokens, total_tokens, cache_tokens,
    reasoning_tokens, input_chars, prompt_items, error_message, raw_usage, api_key_id";

fn open_connection(cfg: &crate::Config) -> Result<Connection, String> {
    let path = history_db_path(cfg);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|err| err.to_string())?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS usage_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                recorded_at TEXT NOT NULL,
                provider TEXT NOT NULL,
                account_key TEXT NOT NULL,
                account_label TEXT NOT NULL,
                account_id TEXT NOT NULL,
                credential_file TEXT,
                model TEXT,
                request_path TEXT NOT NULL,
                success INTEGER NOT NULL,
                error INTEGER NOT NULL,
                request_total INTEGER NOT NULL,
                prompt_total INTEGER NOT NULL,
                prompt_error_total INTEGER NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                cache_tokens INTEGER NOT NULL,
                reasoning_tokens INTEGER NOT NULL,
                input_chars INTEGER NOT NULL,
                prompt_items INTEGER NOT NULL,
                error_message TEXT,
                raw_usage TEXT,
                api_key_id TEXT
             );
             CREATE TABLE IF NOT EXISTS usage_metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_usage_provider_account_id
                ON usage_history(provider, account_key, id);
             CREATE INDEX IF NOT EXISTS idx_usage_model_id
                ON usage_history(model, id);
             CREATE INDEX IF NOT EXISTS idx_usage_recorded_at
                ON usage_history(recorded_at);
             CREATE INDEX IF NOT EXISTS idx_usage_error_account_id
                ON usage_history(error, provider, account_key, id);
             CREATE INDEX IF NOT EXISTS idx_usage_success_recorded_at
                ON usage_history(success, recorded_at);
             CREATE INDEX IF NOT EXISTS idx_usage_account_success_recorded_at
                ON usage_history(account_key, success, recorded_at);",
        )
        .map_err(|err| err.to_string())?;
    ensure_optional_columns(&connection)?;
    import_legacy_jsonl_once(cfg, &mut connection)?;
    Ok(connection)
}

fn ensure_optional_columns(connection: &Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare("PRAGMA table_info(usage_history)")
        .map_err(|err| err.to_string())?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    if !columns.iter().any(|column| column == "api_key_id") {
        connection
            .execute("ALTER TABLE usage_history ADD COLUMN api_key_id TEXT", [])
            .map_err(|err| err.to_string())?;
    }
    connection
        .execute(
            "CREATE INDEX IF NOT EXISTS idx_usage_api_key_recorded_at
                ON usage_history(api_key_id, recorded_at)",
            [],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn import_legacy_jsonl_once(
    cfg: &crate::Config,
    connection: &mut Connection,
) -> Result<(), String> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| err.to_string())?;
    let imported = transaction
        .query_row(
            "SELECT value FROM usage_metadata WHERE key = 'legacy_jsonl_imported'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| err.to_string())?
        .is_some();
    if imported {
        transaction.commit().map_err(|err| err.to_string())?;
        return Ok(());
    }

    let entries = read_legacy_jsonl(cfg)?;
    if !entries.is_empty() {
        let mut statement = transaction
            .prepare_cached(&format!(
                "INSERT INTO usage_history ({}) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                    ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23
                )",
                HISTORY_COLUMNS
            ))
            .map_err(|err| err.to_string())?;
        for entry in &entries {
            insert_entry(&mut statement, entry)?;
        }
    }
    transaction
        .execute(
            "INSERT INTO usage_metadata(key, value) VALUES('legacy_jsonl_imported', '1')",
            [],
        )
        .map_err(|err| err.to_string())?;
    transaction.commit().map_err(|err| err.to_string())
}

fn insert_entry(
    statement: &mut rusqlite::Statement<'_>,
    entry: &UsageHistoryEntry,
) -> Result<(), String> {
    let raw_usage = entry
        .raw_usage
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|err| err.to_string())?;
    statement
        .execute(params![
            &entry.recorded_at,
            &entry.provider,
            &entry.account_key,
            &entry.account_label,
            &entry.account_id,
            &entry.credential_file,
            &entry.model,
            &entry.request_path,
            entry.success as i64,
            entry.error as i64,
            entry.request_total as i64,
            entry.prompt_total as i64,
            entry.prompt_error_total as i64,
            entry.input_tokens as i64,
            entry.output_tokens as i64,
            entry.total_tokens as i64,
            entry.cache_tokens as i64,
            entry.reasoning_tokens as i64,
            entry.input_chars as i64,
            entry.prompt_items as i64,
            &entry.error_message,
            &raw_usage,
            &entry.api_key_id,
        ])
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageHistoryEntry> {
    let raw_usage = row
        .get::<_, Option<String>>(21)?
        .and_then(|value| serde_json::from_str(&value).ok());
    Ok(UsageHistoryEntry {
        recorded_at: row.get(0)?,
        provider: row.get(1)?,
        account_key: row.get(2)?,
        account_label: row.get(3)?,
        account_id: row.get(4)?,
        credential_file: row.get(5)?,
        model: row.get(6)?,
        request_path: row.get(7)?,
        success: row.get::<_, i64>(8)? != 0,
        error: row.get::<_, i64>(9)? != 0,
        request_total: row.get::<_, i64>(10)?.max(0) as u64,
        prompt_total: row.get::<_, i64>(11)?.max(0) as u64,
        prompt_error_total: row.get::<_, i64>(12)?.max(0) as u64,
        input_tokens: row.get::<_, i64>(13)?.max(0) as u64,
        output_tokens: row.get::<_, i64>(14)?.max(0) as u64,
        total_tokens: row.get::<_, i64>(15)?.max(0) as u64,
        cache_tokens: row.get::<_, i64>(16)?.max(0) as u64,
        reasoning_tokens: row.get::<_, i64>(17)?.max(0) as u64,
        input_chars: row.get::<_, i64>(18)?.max(0) as u64,
        prompt_items: row.get::<_, i64>(19)?.max(0) as u64,
        error_message: row.get(20)?,
        raw_usage,
        api_key_id: row.get(22)?,
    })
}

fn push_list_clause(
    clauses: &mut Vec<String>,
    values: &mut Vec<String>,
    column: &str,
    raw: Option<&str>,
    lowercase: bool,
) {
    let items = split_filter_list(raw);
    if items.is_empty() {
        return;
    }
    let placeholders = std::iter::repeat("?")
        .take(items.len())
        .collect::<Vec<_>>()
        .join(", ");
    clauses.push(format!("{} IN ({})", column, placeholders));
    values.extend(items.into_iter().map(|item| {
        if lowercase {
            item.to_ascii_lowercase()
        } else {
            item
        }
    }));
}

fn split_filter_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn read_legacy_jsonl(cfg: &crate::Config) -> Result<Vec<UsageHistoryEntry>, String> {
    let file = match std::fs::File::open(legacy_history_path(cfg)) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.to_string()),
    };
    Ok(BufReader::new(file)
        .lines()
        .filter_map(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

fn normalized_filter(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn history_db_path(cfg: &crate::Config) -> PathBuf {
    cfg.auth_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(HISTORY_DB_FILE)
}

fn legacy_history_path(cfg: &crate::Config) -> PathBuf {
    cfg.auth_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(LEGACY_HISTORY_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &std::path::Path) -> crate::Config {
        crate::Config {
            listen: "127.0.0.1:0".to_string(),
            upstream_base: "https://example.test".to_string(),
            proxy_api_key: "test".to_string(),
            tokens: Vec::new(),
            auth_dir: Some(dir.to_string_lossy().to_string()),
            disabled_files: None,
            admin_auth: Default::default(),
            oauth: Default::default(),
            codex_reset_credit_app_server: crate::CodexResetCreditAppServerConfig::default(),
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

    fn entry(recorded_at: &str, provider: &str, account: &str, model: &str) -> UsageHistoryEntry {
        UsageHistoryEntry {
            recorded_at: recorded_at.to_string(),
            provider: provider.to_string(),
            account_key: account.to_string(),
            account_label: account.to_string(),
            account_id: account.to_string(),
            credential_file: None,
            model: Some(model.to_string()),
            request_path: "/responses".to_string(),
            api_key_id: None,
            success: true,
            error: false,
            request_total: 1,
            prompt_total: 1,
            prompt_error_total: 0,
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            cache_tokens: 0,
            reasoning_tokens: 0,
            input_chars: 20,
            prompt_items: 1,
            error_message: None,
            raw_usage: Some(serde_json::json!({"total_tokens": 15})),
        }
    }

    fn temp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("io-gateway-usage-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sqlite_history_filters_and_keeps_latest_limit_in_order() {
        let dir = temp_dir();
        let cfg = test_config(&dir);
        append_batch(
            &cfg,
            &[
                entry("2026-01-01T00:00:00Z", "codex", "a", "gpt-a"),
                entry("2026-01-01T00:01:00Z", "codex", "a", "gpt-b"),
                entry("2026-01-01T00:02:00Z", "claude", "b", "claude-b"),
            ],
        )
        .unwrap();

        let loaded = load(
            &cfg,
            &UsageHistoryQuery {
                provider: Some("CODEX".to_string()),
                account_key: Some("a".to_string()),
                api_key_id: None,
                limit: Some(1),
                model: None,
            },
        )
        .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].model.as_deref(), Some("gpt-b"));
        assert_eq!(
            loaded[0].raw_usage,
            Some(serde_json::json!({"total_tokens": 15}))
        );
        assert!(dir.join(HISTORY_DB_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_jsonl_is_imported_only_once() {
        let dir = temp_dir();
        let cfg = test_config(&dir);
        let legacy = entry("2026-01-01T00:00:00Z", "codex", "legacy", "gpt-a");
        std::fs::write(
            dir.join(LEGACY_HISTORY_FILE),
            format!("{}\n", serde_json::to_string(&legacy).unwrap()),
        )
        .unwrap();

        assert_eq!(load(&cfg, &UsageHistoryQuery::default()).unwrap().len(), 1);
        assert_eq!(load(&cfg, &UsageHistoryQuery::default()).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_enforces_max_entries() {
        let dir = temp_dir();
        let cfg = test_config(&dir);
        append_batch(
            &cfg,
            &[
                entry("2026-01-01T00:00:00Z", "codex", "a", "gpt-a"),
                entry("2026-01-01T00:01:00Z", "codex", "a", "gpt-b"),
                entry("2026-01-01T00:02:00Z", "codex", "a", "gpt-c"),
            ],
        )
        .unwrap();
        prune(&cfg, 0, 2).unwrap();
        let loaded = load(&cfg, &UsageHistoryQuery::default()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].model.as_deref(), Some("gpt-b"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn context_aggregation_uses_buckets_and_account_filter() {
        let dir = temp_dir();
        let cfg = test_config(&dir);
        append_batch(
            &cfg,
            &[
                entry("2026-01-01T00:01:00Z", "codex", "a", "gpt-a"),
                entry("2026-01-01T00:04:00Z", "codex", "a", "gpt-a"),
                entry("2026-01-01T00:06:00Z", "codex", "b", "gpt-b"),
            ],
        )
        .unwrap();

        let rows = aggregate_context(&cfg, "2026-01-01T00:00:00Z", 300, Some("a"), true).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bucket_start, 1_767_225_600);
        assert_eq!(rows[0].model.as_deref(), Some("gpt-a"));
        assert_eq!(rows[0].request_count, 2);
        assert_eq!(rows[0].total_tokens, 30);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn filtered_summary_combines_datetime_provider_account_and_api_key_filters() {
        let dir = temp_dir();
        let cfg = test_config(&dir);
        let mut first = entry("2026-01-01T00:00:00Z", "codex", "a", "gpt-a");
        first.api_key_id = Some("key-one".to_string());
        let mut second = entry("2026-01-01T00:10:00Z", "codex", "b", "gpt-b");
        second.api_key_id = Some("key-two".to_string());
        let mut third = entry("2026-01-01T00:20:00Z", "gemini", "a", "gemini-a");
        third.api_key_id = Some("key-one".to_string());
        third.error = true;
        third.success = false;
        third.prompt_error_total = 1;
        append_batch(&cfg, &[first, second, third]).unwrap();

        let summary = summarize_filtered(
            &cfg,
            &UsageFilterSummaryQuery {
                start_at: Some("2026-01-01T00:00:01Z".to_string()),
                end_at: Some("2026-01-01T00:30:00Z".to_string()),
                providers: Some("codex,gemini".to_string()),
                account_keys: Some("a".to_string()),
                api_key_ids: Some("key-one".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.total_tokens, 15);
        assert_eq!(
            summary.first_recorded_at.as_deref(),
            Some("2026-01-01T00:20:00Z")
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
