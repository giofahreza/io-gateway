//! Opt-in expiring-grant coordinator. Never retries a submitted reset.
use super::{accounts::ClaudeAccount, resets};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub account_key: String,
    pub enabled: bool,
    pub scan_interval_minutes: i64,
    pub expiry_window_minutes: i64,
    pub final_attempt_minutes: i64,
    pub min_natural_reset_remaining_minutes: i64,
}
impl Policy {
    fn defaults(key: String) -> Self {
        Self {
            account_key: key,
            enabled: false,
            scan_interval_minutes: 30,
            expiry_window_minutes: 60,
            final_attempt_minutes: 5,
            min_natural_reset_remaining_minutes: 10,
        }
    }
    fn validate(&self) -> Result<(), String> {
        if !self.account_key.starts_with("claude:organization:")
            || self.account_key.len() > 512
            || !(1..=1440).contains(&self.scan_interval_minutes)
            || !(30..=10080).contains(&self.expiry_window_minutes)
            || !(1..self.expiry_window_minutes).contains(&self.final_attempt_minutes)
            || !(0..=10080).contains(&self.min_natural_reset_remaining_minutes)
        {
            return Err("Invalid Claude policy identity or scheduling range".into());
        }
        Ok(())
    }
}
fn read_db(cfg: &crate::Config) -> Option<Connection> {
    let path = std::path::Path::new(cfg.auth_dir.as_ref()?).join("claude-reset-actions.sqlite3");
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}
fn policies(db: &Connection) -> Result<Vec<Policy>, String> {
    let mut q=db.prepare("SELECT account_key,enabled,scan_interval_minutes,expiry_window_minutes,final_attempt_minutes,min_natural_reset_remaining_minutes FROM claude_reset_policies").map_err(|_|"Cannot read Claude policies")?;
    let rows = q
        .query_map([], |r| {
            Ok(Policy {
                account_key: r.get(0)?,
                enabled: r.get(1)?,
                scan_interval_minutes: r.get(2)?,
                expiry_window_minutes: r.get(3)?,
                final_attempt_minutes: r.get(4)?,
                min_natural_reset_remaining_minutes: r.get(5)?,
            })
        })
        .map_err(|_| "Cannot read Claude policies")?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|_| "Cannot read Claude policies".into())
}
fn resolve(state: &crate::AppState, key: &str) -> Result<ClaudeAccount, String> {
    let all = state.claude_accounts.lock().unwrap();
    let matches = all
        .iter()
        .filter(|a| {
            a.enabled
                && !a.account_id.is_empty()
                && !a.organization_uuid.is_empty()
                && resets::account_key(a) == key
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err("Policy requires exactly one enabled, identified Claude credential".into());
    }
    Ok(matches[0].clone())
}
pub fn account_policy(cfg: &crate::Config, a: &ClaudeAccount) -> Value {
    let key = resets::account_key(a);
    let policy = read_db(cfg)
        .and_then(|db| policies(&db).ok())
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.account_key == key)
        .unwrap_or_else(|| Policy::defaults(key));
    serde_json::to_value(policy).unwrap_or(Value::Null)
}
pub fn snapshot(state: &crate::AppState) -> Result<Value, String> {
    let accounts = state.claude_accounts.lock().unwrap().clone();
    let db = read_db(&state.cfg);
    let saved = db.as_ref().map(policies).transpose()?.unwrap_or_default();
    let options = accounts
        .iter()
        .filter(|a| a.enabled && !a.organization_uuid.is_empty() && !a.account_id.is_empty())
        .map(|a| json!({"account_key":resets::account_key(a),"label":a.label}))
        .collect::<Vec<_>>();
    let mut actions = Vec::new();
    let mut statuses = Vec::new();
    if let Some(db) = db {
        let mut q=db.prepare("SELECT account_key,credit_id,state,updated_at,source FROM claude_reset_actions ORDER BY updated_at DESC LIMIT 30").map_err(|_|"Cannot read reset actions")?;
        let rows=q.query_map([],|r|Ok(json!({"account_key":r.get::<_,String>(0)?,"credit_id":r.get::<_,String>(1)?,"state":r.get::<_,String>(2)?,"updated_at":r.get::<_,String>(3)?,"source":r.get::<_,String>(4)?}))).map_err(|_|"Cannot read reset actions")?;
        for row in rows {
            actions.push(row.map_err(|_| "Cannot read reset actions")?);
        }
        let mut q = db
            .prepare("SELECT account_key,last_scan_status FROM claude_reset_policies")
            .map_err(|_| "Cannot read reset status")?;
        for row in q
            .query_map([], |r| {
                Ok(json!({"account_key":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?}))
            })
            .map_err(|_| "Cannot read reset status")?
        {
            statuses.push(row.map_err(|_| "Cannot read reset status")?);
        }
    }
    Ok(
        json!({"ok":true,"accounts":options,"policies":saved,"defaults":Policy::defaults(String::new()),"actions":actions,"statuses":statuses}),
    )
}
pub fn save(state: &crate::AppState, p: &Policy) -> Result<(), String> {
    p.validate()?;
    if p.enabled {
        resolve(state, &p.account_key)?;
    }
    let mut db = resets::database(&state.cfg)?;
    let tx = db.transaction().map_err(|_| "Cannot save policy")?;
    tx.execute("INSERT INTO claude_reset_policies(account_key,enabled,scan_interval_minutes,expiry_window_minutes,final_attempt_minutes,min_natural_reset_remaining_minutes) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(account_key) DO UPDATE SET enabled=excluded.enabled,scan_interval_minutes=excluded.scan_interval_minutes,expiry_window_minutes=excluded.expiry_window_minutes,final_attempt_minutes=excluded.final_attempt_minutes,min_natural_reset_remaining_minutes=excluded.min_natural_reset_remaining_minutes,next_scan_at=0",params![p.account_key,p.enabled,p.scan_interval_minutes,p.expiry_window_minutes,p.final_attempt_minutes,p.min_natural_reset_remaining_minutes]).map_err(|_|"Cannot save policy")?;
    if !p.enabled {
        tx.execute("UPDATE claude_reset_actions SET state='cancelled',updated_at=?2 WHERE account_key=?1 AND state IN ('pending','deferred')",params![p.account_key,Utc::now().to_rfc3339()]).map_err(|_|"Cannot cancel pending actions")?;
    }
    tx.commit().map_err(|_| "Cannot commit policy".into())
}
fn epoch(v: Option<&Value>) -> Option<i64> {
    DateTime::parse_from_rfc3339(v?.as_str()?)
        .ok()
        .map(|v| v.timestamp())
}
/// Require all known reached buckets to be cleared and sufficiently far from natural recovery.
fn limit_ready(raw: &Value, id: &str, p: &Policy, now: i64) -> bool {
    if raw
        .pointer("/cedar_ember/at_limit")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return false;
    }
    let Some(grant) = raw
        .pointer("/cedar_ember/grants")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|g| g["id"] == id))
    else {
        return false;
    };
    let clears = grant["clears"].as_array();
    let mut reached = raw
        .as_object()
        .into_iter()
        .flat_map(|o| o.iter())
        .filter(|(_, v)| v["utilization"].as_f64().is_some_and(|n| n >= 100.0))
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>();
    if let Some(exhausted) = raw
        .pointer("/cedar_ember/exhausted")
        .and_then(Value::as_array)
    {
        for v in exhausted {
            let Some(k) = v.as_str() else {
                return false;
            };
            if !reached.contains(&k) {
                reached.push(k);
            }
        }
    }
    !reached.is_empty()
        && reached.iter().all(|k| {
            clears.is_some_and(|a| a.iter().any(|v| v.as_str() == Some(*k)))
                && epoch(raw.get(*k).and_then(|b| b.get("resets_at")))
                    .is_some_and(|t| t > now + p.min_natural_reset_remaining_minutes * 60)
        })
}
fn stamp(db: &Connection, key: &str, status: &str) {
    let _ = db.execute(
        "UPDATE claude_reset_policies SET last_scan_status=?2 WHERE account_key=?1",
        params![key, status],
    );
}
async fn process(state: &crate::AppState, p: &Policy) -> Result<(), String> {
    let now = Utc::now().timestamp();
    let db = resets::database(&state.cfg)?;
    // A crash after durable submission is uncertain and must never trigger a replay.
    db.execute("UPDATE claude_reset_actions SET state='manual_review',updated_at=?2 WHERE state='submitted' AND lease_until>0 AND lease_until<?1",params![now,Utc::now().to_rfc3339()]).map_err(|_|"Cannot recover reset ledger")?;
    let due=db.execute("UPDATE claude_reset_policies SET next_scan_at=?2 WHERE account_key=?1 AND enabled=1 AND next_scan_at<=?3",params![p.account_key,now+p.scan_interval_minutes*60,now]).map_err(|_|"Cannot claim scan")?==1;
    let pending:Option<(String,String,String,i64)>=db.query_row("SELECT credit_id,slot,request_id,credit_expires_at FROM claude_reset_actions WHERE account_key=?1 AND state IN ('pending','deferred') AND next_attempt_at<=?2 AND lease_until<=?2",params![p.account_key,now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_|"Cannot read pending reset")?;
    if !due && pending.is_none() {
        return Ok(());
    }
    if db.query_row("SELECT COUNT(*) FROM claude_reset_actions WHERE account_key=?1 AND state IN ('submitted','manual_review')",[&p.account_key],|r|r.get::<_,i64>(0)).unwrap_or(1)>0 {stamp(&db,&p.account_key,"Previous reset requires review");return Ok(());}
    let account = resolve(state, &p.account_key)?;
    let token = super::auth::ensure_access_token(state, &account).await?;
    let raw = super::quota::fetch_oauth_usage(&state.client, &account, &token).await?;
    let summary = resets::summarize(&raw).unwrap_or(json!({"credits":[]}));
    let selected = if let Some(action) = pending {
        Some(action)
    } else if due {
        let candidate = summary["credits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|g| g["program"] == "cedar_ember" && g["status"] == "available")
            .filter_map(|g| {
                let exp = epoch(g.get("expires_at"))?;
                (exp >= now + 1800 && exp <= now + p.expiry_window_minutes * 60).then_some((g, exp))
            })
            .min_by_key(|(_, exp)| *exp);
        if let Some((g, exp)) = candidate {
            let id = g["id"].as_str().unwrap_or("").to_owned();
            let slot = g["remaining"].to_string();
            let request = uuid::Uuid::new_v4().to_string();
            let inserted=db.execute("INSERT INTO claude_reset_actions(account_key,program,credit_id,slot,request_id,state,updated_at,source,credit_expires_at) VALUES(?1,'cedar_ember',?2,?3,?4,'pending',?5,'automatic',?6) ON CONFLICT(account_key,program,credit_id,slot) DO UPDATE SET state='pending',source='automatic',request_id=excluded.request_id,updated_at=excluded.updated_at,credit_expires_at=excluded.credit_expires_at,next_attempt_at=0,lease_until=0 WHERE claude_reset_actions.state='cancelled'",params![p.account_key,id,slot,request,Utc::now().to_rfc3339(),exp]).map_err(|_|"Cannot select grant")?;
            if inserted == 1 {
                Some((id, slot, request, exp))
            } else {
                None
            }
        } else {
            stamp(
                &db,
                &p.account_key,
                "No expiring reset grant in scheduling window",
            );
            None
        }
    } else {
        None
    };
    let Some((id, slot, request, expires)) = selected else {
        return Ok(());
    };
    if db.execute("UPDATE claude_reset_actions SET lease_until=?4 WHERE account_key=?1 AND credit_id=?2 AND slot=?3 AND state IN ('pending','deferred') AND lease_until<=?5",params![p.account_key,id,slot,now+180,now]).map_err(|_|"Cannot lease grant")?!=1{return Ok(());}
    // A second read is required immediately before the submission gate.
    let fresh = super::quota::fetch_oauth_usage(&state.client, &account, &token).await;
    let fresh = match fresh {
        Ok(v) => v,
        Err(_) => {
            defer(&db, p, &id, &slot, expires, "Usage temporarily unavailable");
            return Ok(());
        }
    };
    let summary = resets::summarize(&fresh).unwrap_or(json!({"credits":[]}));
    let grant = summary["credits"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|g| {
            g["id"] == id && g["program"] == "cedar_ember" && g["remaining"].to_string() == slot
        });
    let current = Utc::now().timestamp();
    if current >= expires || grant.is_none() {
        db.execute("UPDATE claude_reset_actions SET state='expired',updated_at=?4 WHERE account_key=?1 AND credit_id=?2 AND slot=?3 AND state IN ('pending','deferred')",params![p.account_key,id,slot,Utc::now().to_rfc3339()]).map_err(|_|"Cannot expire action")?;
        return Ok(());
    }
    let grant = grant.unwrap();
    if grant["usable_now"] != true || !limit_ready(&fresh, &id, p, current) {
        defer(
            &db,
            p,
            &id,
            &slot,
            expires,
            "Waiting for a reached limit away from natural reset",
        );
        return Ok(());
    }
    resolve(state, &p.account_key)?;
    let form = resets::ConsumeForm {
        file_name: account.file_name.clone().unwrap_or_default(),
        credit_id: id.clone(),
        program: "cedar_ember".into(),
    };
    // Atomic policy and state gate also prevents a concurrent manual reset.
    if db.execute("UPDATE claude_reset_actions SET state='submitted',updated_at=?4 WHERE account_key=?1 AND credit_id=?2 AND slot=?3 AND state IN ('pending','deferred') AND EXISTS(SELECT 1 FROM claude_reset_policies WHERE account_key=?1 AND enabled=1)",params![p.account_key,id,slot,Utc::now().to_rfc3339()]).map_err(|_|"Cannot commit reset submission")?!=1{return Ok(());}
    let outcome = resets::send(state, &account, &token, &form, &request, grant).await;
    let result = match outcome.as_deref() {
        Ok("verified") => "verified",
        Ok("not_limited") => "not_limited",
        Ok("ineligible") => "ineligible",
        _ => "manual_review",
    };
    db.execute("UPDATE claude_reset_actions SET state=?4,updated_at=?5 WHERE account_key=?1 AND credit_id=?2 AND slot=?3 AND state='submitted'",params![p.account_key,id,slot,result,Utc::now().to_rfc3339()]).map_err(|_|"Cannot save reset result")?;
    stamp(&db, &p.account_key, result);
    state.claude_quota_cache.lock().unwrap().clear();
    if result == "verified" {
        crate::clear_router_account_runtime(state, "claude", &crate::claude_stats_key(&account));
    }
    Ok(())
}
fn defer(db: &Connection, p: &Policy, id: &str, slot: &str, expires: i64, status: &str) {
    let now = Utc::now().timestamp();
    let final_at = expires - p.final_attempt_minutes * 60;
    let next = if final_at > now {
        (now + p.scan_interval_minutes * 60).min(final_at)
    } else {
        now + 60
    };
    let _=db.execute("UPDATE claude_reset_actions SET state='deferred',next_attempt_at=?4,lease_until=0,updated_at=?5 WHERE account_key=?1 AND credit_id=?2 AND slot=?3 AND state IN ('pending','deferred')",params![p.account_key,id,slot,next,Utc::now().to_rfc3339()]);
    stamp(db, &p.account_key, status);
}
pub async fn background_worker(state: crate::AppState) {
    loop {
        let saved = read_db(&state.cfg)
            .and_then(|db| policies(&db).ok())
            .unwrap_or_default();
        for p in saved.into_iter().filter(|p| p.enabled) {
            if process(&state, &p).await.is_err() {
                if let Ok(db) = resets::database(&state.cfg) {
                    stamp(
                        &db,
                        &p.account_key,
                        "Scheduling check failed; inspect last action before changing policy",
                    );
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_reset_requires_known_reached_cleared_limits() {
        let now = Utc::now().timestamp();
        let p = Policy::defaults("claude:organization:o:account:a".into());
        let mut raw = json!({"cedar_ember":{"at_limit":true,"exhausted":["five_hour"],"grants":[{"id":"g","clears":["five_hour"]}]},"five_hour":{"utilization":100,"resets_at":DateTime::from_timestamp(now+3600,0).unwrap().to_rfc3339()}});
        assert!(limit_ready(&raw, "g", &p, now));
        raw["cedar_ember"]["at_limit"] = json!(false);
        assert!(!limit_ready(&raw, "g", &p, now));
        raw["cedar_ember"]["at_limit"] = json!(true);
        raw["five_hour"]["resets_at"] =
            json!(DateTime::from_timestamp(now + 60, 0).unwrap().to_rfc3339());
        assert!(!limit_ready(&raw, "g", &p, now));
        raw["five_hour"]["resets_at"] = Value::Null;
        assert!(!limit_ready(&raw, "g", &p, now));
    }
    #[test]
    fn policy_bounds() {
        let mut p = Policy::defaults("claude:organization:o:account:a".into());
        assert!(p.validate().is_ok());
        p.final_attempt_minutes = 60;
        assert!(p.validate().is_err());
    }
}
