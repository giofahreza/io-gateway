//! Additive, credential-free quota details shared by the gateway and terminal.
//! Only known quota fields are projected. Never dump an upstream response or
//! credential object into a detail view. Reading this module makes no requests.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Field {
    pub label: String,
    pub value: String,
}

fn text(value: &Value) -> Option<String> {
    let value = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => if *value { "yes" } else { "no" }.into(),
        _ => return None,
    };
    let value = value
        .chars()
        .filter(|c| !c.is_control())
        .take(600)
        .collect::<String>();
    (!value.trim().is_empty()).then(|| value.trim().to_string())
}

fn push(out: &mut Vec<Field>, label: &str, value: String) {
    let field = Field {
        label: label.into(),
        value,
    };
    if !out.contains(&field) {
        out.push(field);
    }
}

/// Human-readable fields, not an aggregate across unrelated quota windows.
pub fn summarize(account: &Value) -> Vec<Field> {
    let mut out = Vec::new();
    for (key, label) in [
        ("plan_type", "Plan"),
        ("tier_name", "Tier"),
        ("tier_description", "Tier details"),
        ("copilot_plan", "Copilot plan"),
        ("account_type", "Account type"),
        ("access_type_sku", "Subscription"),
        ("is_available", "Available"),
        ("source", "Quota source"),
        ("status_msg", "Provider status"),
        ("description", "Description"),
        ("balance_note", "Balance note"),
        ("note", "Quota note"),
        ("quota_reset_date", "Quota reset"),
        ("chat_enabled", "Chat enabled"),
    ] {
        if let Some(value) = account.get(key).and_then(text) {
            push(&mut out, label, value);
        }
    }
    if let Some(notes) = account.get("notes").and_then(Value::as_array) {
        for note in notes.iter().filter_map(text) {
            push(&mut out, "Note", note);
        }
    }
    if account
        .get("error")
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
    {
        // Error bodies may echo upstream request data. Keep those out of this
        // allowlisted summary; the ordinary routing error view is independent.
        push(
            &mut out,
            "Quota status",
            "Unavailable: provider reported an error".into(),
        );
    }
    visit(account, "Quota", 0, &mut out);
    out
}

fn visit(node: &Value, label: &str, depth: usize, out: &mut Vec<Field>) {
    if depth > 12 {
        return;
    }
    if let Some(items) = node.as_array() {
        for (index, item) in items.iter().enumerate() {
            let mut name = [
                "display_name",
                "label",
                "model_id",
                "model_name",
                "scope",
                "currency",
            ]
            .iter()
            .find_map(|key| item.get(*key).and_then(text))
            .unwrap_or_else(|| (index + 1).to_string());
            if let Some(scope) = item
                .get("scope")
                .and_then(text)
                .filter(|scope| *scope != name)
            {
                name.push_str(&format!(" ({scope})"));
            }
            visit(item, &format!("{label} / {name}"), depth + 1, out);
        }
        return;
    }
    let Some(map) = node.as_object() else {
        return;
    };
    let mut metrics = Vec::new();
    // Preserve actual units/counts as well as percentages. In particular a
    // currency balance must never become a fabricated percentage or reset.
    for (aliases, name) in [
        (&["used_text", "used", "usage_count"][..], "used"),
        (
            &["limit_text", "limit", "total_count", "entitlement"][..],
            "limit",
        ),
        (
            &["remaining_text", "remaining", "quota_remaining"][..],
            "remaining",
        ),
        (
            &["used_percent", "usage_percent", "utilization"][..],
            "used %",
        ),
        (
            &["remaining_percent", "percent_remaining"][..],
            "remaining %",
        ),
        (&["unlimited"][..], "unlimited"),
        (&["has_credits"][..], "has credits"),
        (&["balance", "total_balance"][..], "balance"),
        (&["granted_balance"][..], "granted"),
        (&["topped_up_balance"][..], "paid"),
        (&["monthly_limit"][..], "monthly limit"),
        (&["used_credits"][..], "used credits"),
        (&["is_enabled"][..], "enabled"),
        (&["overage_count"][..], "overage"),
        (&["overage_permitted"][..], "overage permitted"),
        (
            &["reset_label", "reset_at", "resets_at", "reset_time"][..],
            "reset",
        ),
        (&["start_time"][..], "window start"),
        (&["end_time"][..], "window end"),
        (&["remains_time"][..], "reset in seconds"),
    ] {
        if let Some(value) = aliases.iter().find_map(|key| map.get(*key).and_then(text)) {
            metrics.push(if let Some(name) = name.strip_suffix(" %") {
                format!("{value}% {name}")
            } else {
                format!("{name} {value}")
            });
        }
    }
    if !metrics.is_empty() {
        push(out, label, metrics.join("; "));
    }
    for (key, child) in map {
        let title = match key.as_str() {
            "code_generation" => "Code Gen",
            "code_review" => "Code Review",
            "additional_rate_limits" => "Additional limits",
            "groups" => "Group",
            "models" => "Model",
            "limits" | "observed_rate_limits" => "Limit",
            "rate_limit" | "rate_limits" => "Rate limit",
            "current_window" | "current" => "Current",
            "five_hour" => "5h",
            "weekly" | "seven_day" => "Weekly",
            "seven_day_sonnet" => "Sonnet weekly",
            "seven_day_opus" => "Opus weekly",
            "balances" | "balance_infos" => "Balance",
            "credits" => "Credits",
            "extra_usage" => "Extra usage",
            "quota_snapshots" => "Allowance",
            "chat" => "Chat",
            "completions" => "Completions",
            "premium_interactions" => "Premium interactions",
            "raw" | "raw_usage" => "", // Descend only through this same allowlist.
            _ => continue,
        };
        let next = if title.is_empty() {
            label.to_string()
        } else if label == "Quota" {
            title.to_string()
        } else {
            format!("{label} / {title}")
        };
        // Header-derived rate limits can be maps keyed by scope rather than
        // arrays. Dynamic names are labels only; values still use the allowlist.
        if key == "rate_limits"
            && child
                .as_object()
                .is_some_and(|m| !m.contains_key("limit") && !m.contains_key("used_percent"))
        {
            for (scope, value) in child.as_object().unwrap() {
                visit(
                    value,
                    &format!(
                        "{next} / {}",
                        text(&Value::String(scope.clone())).unwrap_or_default()
                    ),
                    depth + 1,
                    out,
                );
            }
        } else {
            visit(child, &next, depth + 1, out);
        }
    }
}

/// Attach summaries without changing the existing API payload or cache policy.
#[allow(dead_code)] // Gateway entry point; the shared module also compiles in iogw.
pub fn with_summaries(mut quota: Value) -> Value {
    if let Some(accounts) = quota.get_mut("accounts").and_then(Value::as_array_mut) {
        for account in accounts {
            let fields = summarize(account);
            if let Some(map) = account.as_object_mut() {
                map.insert(
                    "quota_summary".into(),
                    serde_json::to_value(fields).unwrap(),
                );
            }
        }
    } else if quota.get("account").is_some_and(Value::is_object) {
        // Grok keeps passively observed limits next to its account identity.
        let mut source = quota["account"].clone();
        for key in ["observed_rate_limits", "note"] {
            if let Some(value) = quota.get(key) {
                source[key] = value.clone();
            }
        }
        quota["account"]["quota_summary"] = serde_json::to_value(summarize(&source)).unwrap();
    }
    quota
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_provider_shapes_preserve_reported_values_without_credentials() {
        for (provider, quota, expected) in [
            (
                "codex",
                json!({"code_generation":{"five_hour":{"used_percent":25,"reset_label":"1h"}},"credits":{"balance":"8.25","unlimited":false}}),
                "balance 8.25",
            ),
            (
                "antigravity",
                json!({"groups":[{"display_name":"Pro","five_hour":{"remaining_percent":75}}]}),
                "75% remaining",
            ),
            (
                "gemini",
                json!({"models":[{"model_id":"gemini-test","current":{"used_percent":20}}]}),
                "20% used",
            ),
            (
                "qwen",
                json!({"limits":[{"label":"Requests","used":5,"limit":100}]}),
                "limit 100",
            ),
            (
                "deepseek",
                json!({"balances":[{"currency":"USD","total_balance":"4.50"}]}),
                "balance 4.50",
            ),
            (
                "minimax",
                json!({"current_window":{"usage_count":7,"total_count":50,"end_time":123}}),
                "window end 123",
            ),
            (
                "copilot",
                json!({"limits":[{"label":"Chat","unlimited":true}],"raw_usage":{"quota_snapshots":{"premium_interactions":{"overage_count":4}}}}),
                "overage 4",
            ),
            (
                "claude",
                json!({"raw_usage":{"extra_usage":{"is_enabled":true,"monthly_limit":20,"used_credits":3}}}),
                "used credits 3",
            ),
            (
                "glm",
                json!({"balance_note":"not reported","balances":[{"currency":"CNY","total_balance":"0"}]}),
                "balance 0",
            ),
            (
                "grok",
                json!({"limits":[{"scope":"tokens","remaining":40,"limit":100}]}),
                "remaining 40",
            ),
        ] {
            let mut quota = quota;
            quota["access_token"] = json!("SECRET");
            quota["raw_headers"] = json!([{"name":"Authorization","value":"SECRET"}]);
            quota["raw"] = json!({"refresh_token":"SECRET", "debug":{"api_key":"SECRET"}});
            let fields = summarize(&quota);
            let rendered = serde_json::to_string(&fields).unwrap();
            assert!(rendered.contains(expected), "{provider}: {rendered}");
            assert!(!rendered.contains("SECRET"));
        }
        assert!(summarize(&json!({"access_token":"SECRET"})).is_empty());
        assert!(
            !serde_json::to_string(&summarize(&json!({"error":"SECRET"})))
                .unwrap()
                .contains("SECRET")
        );
    }

    #[test]
    fn annotation_is_additive_idempotent_and_supports_passive_grok_limits() {
        let quota = json!({"accounts":[{"file_name":"one.json","credits":{"balance":0}}]});
        let annotated = with_summaries(quota.clone());
        assert_eq!(
            annotated["accounts"][0]["file_name"],
            quota["accounts"][0]["file_name"]
        );
        assert_eq!(with_summaries(annotated.clone()), annotated);
        let grok = with_summaries(
            json!({"account":{"file_name":"grok.json"}, "observed_rate_limits":[{"scope":"requests","limit":5,"remaining":2}]}),
        );
        assert!(grok["account"]["quota_summary"]
            .to_string()
            .contains("remaining 2"));
    }
}
