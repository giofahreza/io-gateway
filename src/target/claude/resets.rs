//! Explicit, manual Claude reset grants. Reads never redeem a reset.
//! A durable account gate is committed before sending the single upstream POST.
//! Ambiguous results block further spending until an operator reviews them.
use super::accounts::ClaudeAccount;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

const CEDAR: &str = "cedar_ember";
const JUNIPER: &str = "juniper_tide";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumeForm {
    pub file_name: String,
    pub credit_id: String,
    pub program: String,
}

fn future(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|date| date > Utc::now())
}

fn started(value: Option<&Value>) -> bool {
    match value.filter(|v| !v.is_null()) {
        None => true,
        Some(value) => value
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|date| date <= Utc::now()),
    }
}

pub fn summarize(raw: &Value) -> Option<Value> {
    let mut credits = Vec::new();
    let cedar = raw.get(CEDAR).filter(|v| v.is_object());
    let juniper = raw.get(JUNIPER).filter(|v| v.is_object());
    if cedar.is_none() && juniper.is_none() {
        return None;
    }
    if let Some(status) = cedar {
        for grant in status
            .get("grants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = grant
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| valid_id(id))
            else {
                continue;
            };
            let left = grant
                .get("resets_left")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let held = left > 0
                && future(grant.get("ends_at"))
                && grant.get("paused").and_then(Value::as_bool) != Some(true);
            let usable = held
                && status.get("eligible").and_then(Value::as_bool) == Some(true)
                && started(grant.get("starts_at"))
                && !future(status.get("cooldown_until"))
                && grant.get("usable_now").and_then(Value::as_bool) == Some(true)
                && (grant.get("use_requires_limit").and_then(Value::as_bool) == Some(false)
                    || status.get("at_limit").and_then(Value::as_bool) == Some(true));
            let clears = grant
                .get("clears")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|name| match name {
                            "five_hour" => "5-hour limit",
                            "seven_day" => "weekly limit",
                            "seven_day_overage_included" => "included Fable usage",
                            "seven_day_opus" => "Opus weekly limit",
                            "seven_day_sonnet" => "Sonnet weekly limit",
                            "seven_day_cowork" => "Cowork weekly limit",
                            "seven_day_omelette" => "Fable weekly limit",
                            "seven_day_oauth_apps" => "OAuth apps weekly limit",
                            _ => "other usage limit",
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            credits.push(json!({
                "id": id, "program": CEDAR,
                "reset_type": grant.get("type").and_then(Value::as_str).unwrap_or("usage"),
                "title": grant.get("label").and_then(Value::as_str).unwrap_or("Claude usage reset"),
                "description": format!("{left} remaining; clears {clears}"),
                "granted_at": grant.get("starts_at"), "expires_at": grant.get("ends_at"),
                "status": if held { "available" } else { "unavailable" },
                "remaining": left, "usable_now": usable,
                "unavailable_reason": grant.get("unavailable_reason").and_then(Value::as_str)
                    .or_else(|| status.get("ineligible_reason").and_then(Value::as_str)).unwrap_or("")
            }));
        }
    }
    if let Some(status) = juniper {
        let usable = status.get("eligible").and_then(Value::as_bool) == Some(true)
            && status.get("arm").and_then(Value::as_str) == Some("reset")
            && status.get("available").and_then(Value::as_bool) == Some(true)
            && raw
                .get("five_hour")
                .and_then(|b| b.get("utilization"))
                .and_then(Value::as_f64)
                .is_some_and(|n| n >= 100.0)
            && future(raw.pointer("/five_hour/resets_at"));
        credits.push(json!({"id": JUNIPER, "program": JUNIPER, "title": "Weekly 5-hour reset",
            "reset_type": "session", "description": "Resets the 5-hour limit; weekly limits still apply.",
            "status": if usable { "available" } else { "unavailable" }, "usable_now": usable,
            "remaining": if usable { 1 } else { 0 }, "next_available_at": status.get("next_available_at"),
            "unavailable_reason": status.get("ineligible_reason") }));
    }
    let count: u64 = credits
        .iter()
        .filter(|c| c["status"] == "available")
        .filter_map(|c| c["remaining"].as_u64())
        .fold(0u64, u64::saturating_add);
    Some(json!({"available_count": count, "credits": credits}))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

pub(super) fn account_key(account: &ClaudeAccount) -> String {
    format!(
        "claude:organization:{}:account:{}",
        account.organization_uuid, account.account_id
    )
}

pub(super) fn database(cfg: &crate::Config) -> Result<Connection, String> {
    let dir = cfg
        .auth_dir
        .as_ref()
        .ok_or("Claude reset management requires a persistent auth directory")?;
    let path = std::path::Path::new(dir).join("claude-reset-actions.sqlite3");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&path)
        .map_err(|_| "Cannot open Claude reset ledger")?;
    let db = Connection::open(path).map_err(|_| "Cannot open Claude reset ledger")?;
    db.busy_timeout(Duration::from_secs(5))
        .map_err(|_| "Claude reset ledger busy")?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS claude_reset_actions (
            account_key TEXT NOT NULL, program TEXT NOT NULL, credit_id TEXT NOT NULL,
            slot TEXT NOT NULL, request_id TEXT NOT NULL, state TEXT NOT NULL,
            updated_at TEXT NOT NULL, source TEXT NOT NULL DEFAULT 'manual',
            credit_expires_at INTEGER, next_attempt_at INTEGER NOT NULL DEFAULT 0,
            lease_until INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(account_key,program,credit_id,slot));
        CREATE UNIQUE INDEX IF NOT EXISTS claude_reset_account_gate ON claude_reset_actions(account_key)
            WHERE state IN ('pending','deferred','submitted','manual_review');
        CREATE TABLE IF NOT EXISTS claude_reset_policies (
            account_key TEXT PRIMARY KEY, enabled INTEGER NOT NULL,
            scan_interval_minutes INTEGER NOT NULL, expiry_window_minutes INTEGER NOT NULL,
            final_attempt_minutes INTEGER NOT NULL, min_natural_reset_remaining_minutes INTEGER NOT NULL,
            next_scan_at INTEGER NOT NULL DEFAULT 0, last_scan_status TEXT NOT NULL DEFAULT 'Not scanned');").map_err(|_| "Cannot initialize Claude reset ledger")?;
    Ok(db)
}

pub fn last_action(cfg: &crate::Config, account: &ClaudeAccount) -> Option<Value> {
    let path = std::path::Path::new(cfg.auth_dir.as_ref()?).join("claude-reset-actions.sqlite3");
    if !path.exists() {
        return None;
    }
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    db.query_row("SELECT state,updated_at FROM claude_reset_actions WHERE account_key=?1 ORDER BY updated_at DESC LIMIT 1",
        [account_key(account)], |row| Ok(json!({"state": row.get::<_,String>(0)?, "updated_at": row.get::<_,String>(1)?}))).optional().ok().flatten()
}

pub async fn consume(state: &crate::AppState, form: ConsumeForm) -> Result<Value, String> {
    if !valid_id(&form.credit_id) || ![CEDAR, JUNIPER].contains(&form.program.as_str()) {
        return Err("Select a concrete Claude reset grant and program".into());
    }
    let account = {
        let accounts = state.claude_accounts.lock().unwrap();
        let matches = accounts
            .iter()
            .filter(|a| a.file_name.as_deref() == Some(form.file_name.as_str()))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err("Select exactly one Claude credential".into());
        }
        let a = matches[0];
        if !a.enabled || a.organization_uuid.is_empty() || a.account_id.is_empty() {
            return Err("Claude account must be enabled and identified".into());
        }
        a.clone()
    };
    let token = super::auth::ensure_access_token(state, &account).await?;
    let raw = super::quota::fetch_oauth_usage(&state.client, &account, &token).await?;
    let summary = summarize(&raw).ok_or("Claude did not report reset eligibility")?;
    let grant = summary["credits"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|g| {
            g["id"] == form.credit_id && g["program"] == form.program && g["usable_now"] == true
        })
        .ok_or("Selected Claude reset is not currently usable; refresh its status")?;
    let slot = if form.program == CEDAR {
        grant["remaining"].to_string()
    } else {
        raw.pointer("/five_hour/resets_at")
            .and_then(Value::as_str)
            .ok_or("Claude reset window is unknown")?
            .to_string()
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let key = account_key(&account);
    if !state.claude_accounts.lock().unwrap().iter().any(|current| {
        current.enabled && current.file_name == account.file_name && account_key(current) == key
    }) {
        return Err(
            "Claude account was disabled, removed, or changed during the eligibility check".into(),
        );
    }
    let db = database(&state.cfg)?;
    db.execute("INSERT INTO claude_reset_actions(account_key,program,credit_id,slot,request_id,state,updated_at)
        VALUES(?1,?2,?3,?4,?5,'submitted',?6)
        ON CONFLICT(account_key,program,credit_id,slot) DO UPDATE SET state='submitted',source='manual',
            request_id=excluded.request_id,updated_at=excluded.updated_at
            WHERE claude_reset_actions.state IN ('pending','deferred','cancelled')",
        params![key,form.program,form.credit_id,slot,request_id,Utc::now().to_rfc3339()])
        .map_err(|_| "A reset for this account or grant has already been submitted. Review its status before spending again.")?
        .eq(&1).then_some(()).ok_or("This reset entitlement has already been processed")?;
    // No automatic retry, including after a timeout or process restart.
    let outcome = send(state, &account, &token, &form, &request_id, grant).await;
    let (action_state, message, ok) = match outcome.as_deref() {
        Ok("verified") => ("verified", "Claude reset confirmed by a fresh usage read.", true),
        Ok("not_limited") => ("not_limited", "Claude reports no applicable limit; no reset was needed.", false),
        Ok("ineligible") => ("ineligible", "Claude no longer offers this reset.", false),
        _ => ("manual_review", "Reset outcome is unconfirmed. Check Claude usage before taking any further action; this account is blocked from another reset.", false),
    };
    db.execute("UPDATE claude_reset_actions SET state=?6,updated_at=?7 WHERE account_key=?1 AND program=?2 AND credit_id=?3 AND slot=?4 AND request_id=?5",
        params![key,form.program,form.credit_id,slot,request_id,action_state,Utc::now().to_rfc3339()])
        .map_err(|_| "Reset submitted but its status could not be saved; review the account")?;
    state.claude_quota_cache.lock().unwrap().clear();
    if ok {
        crate::clear_router_account_runtime(state, "claude", &crate::claude_stats_key(&account));
    }
    Ok(json!({"ok":ok,"outcome":action_state,"message":message}))
}

pub(super) async fn send(
    state: &crate::AppState,
    account: &ClaudeAccount,
    token: &str,
    form: &ConsumeForm,
    request_id: &str,
    grant: &Value,
) -> Result<String, String> {
    let base = super::auth::api_base_url(account.api_base_url.as_deref());
    let mut url = reqwest::Url::parse(&base).map_err(|_| "Invalid Claude API URL")?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| "Invalid Claude API URL")?;
        path.pop_if_empty()
            .push("api")
            .push("organizations")
            .push(&account.organization_uuid)
            .push("reset_rate_limits");
    }
    let body = if form.program == CEDAR {
        json!({"program":CEDAR,"grant_id":form.credit_id,"request_id":request_id})
    } else {
        json!({"program":JUNIPER})
    };
    let response = state
        .client
        .post(url)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", super::CLAUDE_CODE_CLI_USER_AGENT)
        .json(&body)
        .timeout(Duration::from_secs(25))
        .send()
        .await
        .map_err(|_| "Claude reset response unavailable")?;
    if !response.status().is_success() {
        return Err("Claude reset response unavailable".into());
    }
    let result: Value = response
        .json()
        .await
        .map_err(|_| "Invalid Claude reset response")?;
    let outcome = result["result"].as_str().unwrap_or("");
    if ["not_limited", "ineligible"].contains(&outcome) {
        return Ok(outcome.into());
    }
    if !["reset", "already_used"].contains(&outcome) {
        return Err("Claude reset requires review".into());
    }
    let fresh = super::quota::fetch_oauth_usage(&state.client, account, token).await?;
    let verified = if form.program == CEDAR {
        fresh
            .pointer("/cedar_ember/grants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|g| g["id"] == form.credit_id)
            .and_then(|g| g["resets_left"].as_u64())
            .zip(grant["remaining"].as_u64())
            .is_some_and(|(now, before)| now < before)
    } else {
        fresh
            .pointer("/juniper_tide/available")
            .and_then(Value::as_bool)
            == Some(false)
            && fresh
                .pointer("/five_hour/utilization")
                .and_then(Value::as_f64)
                .is_some_and(|n| n < 100.0)
    };
    if verified {
        Ok("verified".into())
    } else {
        Err("Claude reset verification unavailable".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reset_grants_require_explicit_fresh_eligibility() {
        let mut raw = json!({"cedar_ember":{"eligible":true,"at_limit":false,"grants":[{
            "id":"full-reset-test","resets_left":1,"ends_at":"2099-01-01T00:00:00Z",
            "usable_now":true,"use_requires_limit":false,"clears":["five_hour","seven_day"]}]}});
        assert_eq!(summarize(&raw).unwrap()["credits"][0]["usable_now"], true);
        raw[CEDAR]["grants"][0]["use_requires_limit"] = json!(true);
        assert_eq!(summarize(&raw).unwrap()["credits"][0]["usable_now"], false);
        raw[CEDAR]["at_limit"] = json!(true);
        raw[CEDAR]["grants"][0]["paused"] = json!(true);
        assert_eq!(summarize(&raw).unwrap()["available_count"], 0);
        raw[CEDAR]["grants"][0]["ends_at"] = json!("invalid");
        assert_eq!(summarize(&raw).unwrap()["credits"][0]["usable_now"], false);
        assert!(summarize(&json!({})).is_none());
    }
}
