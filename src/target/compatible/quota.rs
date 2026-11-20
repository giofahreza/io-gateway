//! Read-only Qwen Cloud console queries. The inference key cannot read billing.
//! Only normalized quota fields leave this module; cookies, security tokens and
//! raw console responses must never appear in snapshots or error messages.
use super::accounts::Service;
use axum::{
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::time::Duration;

const CONSOLE: &str = "https://home.qwencloud.com";

pub async fn route(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = crate::require_admin_session_json(&state, &headers) {
        return response;
    }
    Json(crate::provider_quota_summary::with_summaries(
        json!({"accounts":get_quota_summaries(&state).await}),
    ))
    .into_response()
}

pub async fn get_quota_summaries(state: &crate::AppState) -> Vec<Value> {
    // Read the account version and its cache together. Reloads use this order too.
    let (accounts, cached) = {
        let accounts_guard = state.compatible_accounts.lock().unwrap();
        let cached = state
            .quota_snapshots
            .lock()
            .unwrap()
            .get("compatible")
            .cloned()
            .unwrap_or_default();
        (accounts_guard.clone(), cached)
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .build();
    let mut results = Vec::new();
    for account in accounts.iter().filter(|a| a.service != Service::Custom) {
        if let Some(value) = cached.iter().find(|value| {
            value["account_id"] == account.account_id
                && value["service"] == json!(account.service)
                && value["fetched_at"]
                    .as_str()
                    .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                    .is_some_and(|at| {
                        (0..60).contains(
                            &(chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds(),
                        )
                    })
        }) {
            results.push(value.clone());
            continue;
        }
        let mut value = json!({"account_id":account.account_id,"key":crate::compatible_stats_key(account),
            "file_name":account.file_name,"label":account.label,"service":account.service,"display_provider":account.service.display_provider(),
            "tier_name":account.service.label(),"credits":{"balance":null},"limits":[],"notes":[],
            "fetched_at":chrono::Utc::now().to_rfc3339()});
        let result = match (account.cloud_auth.as_ref(), account.console_cookie.as_deref(), client.as_ref()) {
            (Some(auth), _, Ok(client)) => super::cloud_auth::fetch_quota(client, &super::cloud_auth::ENDPOINTS, auth, account.service).await,
            (None, Some(cookie), Ok(client)) => fetch(client, CONSOLE, cookie, account.service).await,
            (None, None, _) => Err("Optional to show quota: Connect Qwen Cloud account. Your inference API key works without this connection.".into()),
            (_, _, Err(_)) => Err("Could not initialize the quota connection.".into()),
        };
        match result {
            Ok(fields) => {
                value["quota_status"] = json!("available");
                for (key, field) in fields.as_object().unwrap() {
                    value[key] = field.clone();
                }
            }
            Err(message) => {
                value["quota_status"] = json!(if account
                    .cloud_auth
                    .as_ref()
                    .is_some_and(|auth| auth.expired())
                {
                    "login_required"
                } else if account.console_cookie.is_some() || account.cloud_auth.is_some() {
                    "unavailable"
                } else {
                    "login_required"
                });
                if value["quota_status"] != "login_required" {
                    value["error"] = json!(message);
                }
                value["notes"] = json!([message]);
            }
        }
        results.push(value);
    }
    let current = state.compatible_accounts.lock().unwrap();
    // A disconnect, rekey or removal must win over an in-flight provider read.
    if *current == accounts {
        state
            .quota_snapshots
            .lock()
            .unwrap()
            .insert("compatible".into(), results.clone());
    } else {
        results.retain(|value| {
            accounts.iter().any(|original| {
                value["account_id"] == original.account_id && current.contains(original)
            })
        });
    }
    results
}

pub(super) async fn body(response: reqwest::Response) -> Result<Value, String> {
    if !response.status().is_success() {
        return Err("Qwen Cloud quota unavailable; check the console session.".into());
    }
    json_body(response).await
}

pub(super) async fn json_body(mut response: reqwest::Response) -> Result<Value, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not read Qwen Cloud quota.")?
    {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err("Qwen Cloud quota response was too large.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "Qwen Cloud did not return quota JSON; renew the console session.".into())
}

pub(super) fn payload(value: &Value) -> Result<Value, String> {
    let mut value = value;
    for _ in 0..8 {
        if value["success"] == false
            || value["Success"] == false
            || value["successResponse"] == false
        {
            return Err("Qwen Cloud rejected the quota query; renew the console session.".into());
        }
        if let Some(next) = value.get("data").or_else(|| value.get("DataV2")) {
            value = next;
        } else {
            break;
        }
    }
    if !value.is_object() {
        return Err("Qwen Cloud returned an unrecognized quota response.".into());
    }
    Ok(value.clone())
}

// This closed list contains only data reads, even though the console gateway
// transports them with POST. No user-supplied action or URL is executed.
enum Read {
    Usage,
    Subscription,
    QuotaConfig,
    BillingAccount,
    Balance,
}

async fn query(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    token: &str,
    read: Read,
    nbid: Option<&Value>,
) -> Result<Value, String> {
    let (product, action, params) = match read {
        Read::Usage | Read::Subscription | Read::QuotaConfig => {
            let suffix = match read {
                Read::Usage => "usage",
                Read::Subscription => "subscription",
                _ => "quota-config",
            };
            let data = if matches!(read, Read::Subscription) {
                json!({"commodityCode":"sfm_tokenplansolo_public_intl"})
            } else {
                json!({})
            };
            (
                "sfm_bailian",
                "IntlBroadScopeAspnGateway",
                json!({"Api":format!("zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/{suffix}"),"Data":data}),
            )
        }
        Read::BillingAccount => ("ea-service", "LoadHumanInfo", json!({})),
        Read::Balance => (
            "BssOpenAPI-V3",
            "GetBillingAccountAvailableAmount",
            nbid.map(|id| json!({"Nbid":id}))
                .unwrap_or_else(|| json!({})),
        ),
    };
    let response = client
        .post(format!("{base}/data/api.json"))
        .query(&[("product", product), ("action", action)])
        .header("Cookie", cookie)
        .header("Origin", base)
        .header("Referer", format!("{base}/"))
        .form(&[
            ("product", product),
            ("action", action),
            ("region", "ap-southeast-1"),
            ("language", "en-US"),
            ("sec_token", token),
            ("params", &params.to_string()),
        ])
        .send()
        .await
        .map_err(|_| "Could not connect to Qwen Cloud quota.")?;
    payload(&body(response).await?)
}

async fn fetch(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    service: Service,
) -> Result<Value, String> {
    let info = client
        .get(format!("{base}/tool/user/info.json"))
        .header("Cookie", cookie)
        .send()
        .await
        .map_err(|_| "Could not connect to the Qwen Cloud console.")?;
    let info = payload(&body(info).await?)?;
    let token = info["secToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Qwen Cloud login required; renew the console session.")?;
    if service == Service::QwenTokenPlan {
        let (usage, subscription, config) = tokio::join!(
            query(client, base, cookie, token, Read::Usage, None),
            query(client, base, cookie, token, Read::Subscription, None),
            query(client, base, cookie, token, Read::QuotaConfig, None)
        );
        Ok(token_plan(
            &usage?,
            &subscription.unwrap_or(Value::Null),
            &config.unwrap_or(Value::Null),
        ))
    } else {
        let account = query(client, base, cookie, token, Read::BillingAccount, None).await?;
        let nbid = account.pointer("/Data/SellerInfoDto/Nbid");
        let balance = query(client, base, cookie, token, Read::Balance, nbid).await?;
        payg(&balance)
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}

fn reset(value: &Value) -> Option<String> {
    let epoch = number(value)?;
    if epoch <= 0.0 || epoch > i64::MAX as f64 {
        return None;
    }
    chrono::DateTime::from_timestamp_millis(if epoch < 100_000_000_000.0 {
        (epoch * 1000.0) as i64
    } else {
        epoch as i64
    })
    .map(|t| t.to_rfc3339())
}

pub(super) fn token_plan(usage: &Value, subscription: &Value, config: &Value) -> Value {
    let tier = subscription["specCode"].as_str().unwrap_or("");
    let tier_config = &config[tier];
    let mut limits = Vec::new();
    let mut remaining_credits = None;
    // Monthly, weekly and rolling windows overlap. Never add their balances.
    for (field, reset_field, config_field, label) in [
        ("per5HourPercentage", "per5HourResetTime", "five_hour", "5h"),
        (
            "per1WeekPercentage",
            "per1WeekResetTime",
            "weekly",
            "Weekly",
        ),
        (
            "per1MonthPercentage",
            "per1MonthResetTime",
            "monthly",
            "Monthly",
        ),
    ] {
        let ratio = number(&usage[field]).filter(|n| (0.0..=1.0).contains(n));
        let total = number(&tier_config[config_field]).filter(|n| *n > 0.0);
        if ratio.is_none() {
            continue;
        }
        let remaining = ratio.zip(total).map(|(ratio, total)| (1.0 - ratio) * total);
        if remaining.is_some() {
            remaining_credits = remaining;
        }
        limits.push(
            json!({"label":label,"scope":"subscription","used_percent":ratio.map(|r|r*100.0),
            "limit":total,"used":ratio.zip(total).map(|(r,t)|r*t),"remaining":remaining,
            "reset_at":reset(&usage[reset_field]),"reset_label":reset(&usage[reset_field])}),
        );
    }
    let mut notes = vec!["Token Plan credits are subscription units, not money. Remaining credits are estimates from the reported ratio and allowance; Credit Packs and prorated upgrades are not included.".to_string()];
    if limits.is_empty() {
        notes.push("Quota windows not reported by the console.".into());
    }
    json!({"tier_name":if tier.is_empty(){"Qwen Cloud Token Plan"}else{tier},
        "credits":{"balance":remaining_credits,"estimated":true,"note":"Estimated remaining plan credits; excludes Credit Packs. Overlapping windows are not added."},
        "limits":limits,"notes":notes,"expires_at":reset(&subscription["endTime"])})
}

pub(super) fn payg(value: &Value) -> Result<Value, String> {
    let data = value.get("Data").unwrap_or(value);
    if number(&data["AvailableAmount"]).is_none() && number(&data["TotalCashAmount"]).is_none() {
        return Err("Qwen Cloud did not report an account balance.".into());
    }
    let amount = |key: &str| {
        if number(&data[key]).is_some() {
            data[key].clone()
        } else {
            Value::Null
        }
    };
    let currency = data["Currency"]
        .as_str()
        .filter(|value| value.len() == 3 && value.bytes().all(|b| b.is_ascii_uppercase()));
    Ok(
        json!({"credits":{"balance":amount("AvailableAmount"),"cash_balance":amount("TotalCashAmount"),
        "credit_limit":amount("CreditAmount"),"currency":currency,
        "note":"Available account balance may include a credit line. Cash balance and credit limit are shown separately."},
        "notes":["Pay-as-you-go has no Token Plan subscription allowance. Per-model rate limits are not reported by the billing endpoint."],"limits":[]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn console_transport_queries_only_usage_and_billing_and_preserves_form_encoding() {
        use axum::{
            routing::{get, post},
            Form,
        };
        use std::{
            collections::HashMap,
            sync::{Arc, Mutex},
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app=axum::Router::new()
            .route("/tool/user/info.json",get(|headers: HeaderMap| async move {
                assert_eq!(headers["cookie"],"session=synthetic-private-cookie");
                Json(json!({"code":"200","data":{"secToken":"synthetic+token&x=1"}}))
            }))
            .route("/data/api.json",post(move |headers: HeaderMap,Form(form): Form<HashMap<String,String>>| {
                let calls=recorded.clone();
                async move {
                    assert_eq!(headers["cookie"],"session=synthetic-private-cookie");
                    assert!(headers.get("authorization").is_none());
                    assert_eq!(form["sec_token"],"synthetic+token&x=1");
                    let params:Value=serde_json::from_str(&form["params"]).unwrap();
                    calls.lock().unwrap().push(form["action"].clone());
                    let data=match form["action"].as_str() {
                        "LoadHumanInfo"=>json!({"Data":{"SellerInfoDto":{"Nbid":"seller-fixture"}}}),
                        "GetBillingAccountAvailableAmount"=>{
                            assert_eq!(params["Nbid"],"seller-fixture");
                            json!({"AvailableAmount":"12.50","TotalCashAmount":"2.50","CreditAmount":"10","Currency":"USD"})
                        },
                        "IntlBroadScopeAspnGateway"=> {
                            let api=params["Api"].as_str().unwrap();
                            let data=if api.ends_with("/usage") { json!({"per1MonthPercentage":0.25}) }
                            else if api.ends_with("/subscription") { json!({"specCode":"essential"}) }
                            else if api.ends_with("/quota-config") { json!({"essential":{"monthly":20000}}) }
                            else { panic!("unexpected console action") };
                            json!({"DataV2":{"data":{"success":true,"data":data}}})
                        },
                        _=>panic!("unexpected console action"),
                    };
                    Json(json!({"successResponse":true,"data":data}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let plan = fetch(
            &client,
            &base,
            "session=synthetic-private-cookie",
            Service::QwenTokenPlan,
        )
        .await
        .unwrap();
        assert_eq!(plan["credits"]["balance"], 15000.0);
        let balance = fetch(
            &client,
            &base,
            "session=synthetic-private-cookie",
            Service::QwenPayg,
        )
        .await
        .unwrap();
        assert_eq!(balance["credits"]["balance"], "12.50");
        let text = format!("{plan}{balance}");
        assert!(!text.contains("synthetic-private-cookie") && !text.contains("synthetic+token"));
        assert_eq!(calls.lock().unwrap().len(), 5);
        server.abort();
    }
    #[test]
    fn token_plan_keeps_overlapping_windows_separate_and_never_invents_allowances() {
        let value = token_plan(
            &json!({"per1WeekPercentage":0.2,"per1MonthPercentage":0.1,"per1WeekResetTime":1794585600000i64}),
            &json!({"specCode":"essential"}),
            &json!({"essential":{"weekly":10000,"monthly":45000}}),
        );
        assert_eq!(value["limits"][0]["used_percent"], 20.0);
        assert_eq!(value["credits"]["balance"], 40500.0);
        assert_eq!(value["limits"][0]["remaining"], 8000.0);
        assert!(value["limits"][0]["reset_at"]
            .as_str()
            .unwrap()
            .ends_with("+00:00"));
        let unknown = token_plan(
            &json!({"per1MonthPercentage":0.2}),
            &Value::Null,
            &Value::Null,
        );
        assert!(unknown["credits"]["balance"].is_null());
        assert_eq!(unknown["limits"][0]["used_percent"], 20.0);
        for bad in [json!(-0.1), json!(1.1), json!("oops"), Value::Null] {
            assert_eq!(
                token_plan(
                    &json!({"per1MonthPercentage":bad}),
                    &Value::Null,
                    &Value::Null
                )["limits"],
                json!([])
            );
        }
    }
    #[test]
    fn payg_distinguishes_available_balance_cash_and_credit_line() {
        let value=payg(&json!({"Data":{"AvailableAmount":"12.50","TotalCashAmount":"2.50","CreditAmount":"10","Currency":"USD"}})).unwrap();
        assert_eq!(value["credits"]["balance"], "12.50");
        assert_eq!(value["credits"]["cash_balance"], "2.50");
        assert_eq!(value["credits"]["credit_limit"], "10");
        assert!(payg(&json!({"Data":{}})).is_err());
        assert_eq!(
            payg(&json!({"AvailableAmount":0})).unwrap()["credits"]["balance"],
            0
        );
    }
    #[test]
    fn console_wrappers_unwrap_without_leaking_errors_or_tokens() {
        assert_eq!(payload(&json!({"successResponse":true,"data":{"DataV2":{"data":{"success":true,"data":{"per1MonthPercentage":0.2}}}}})).unwrap(),json!({"per1MonthPercentage":0.2}));
        let err=payload(&json!({"successResponse":true,"data":{"DataV2":{"data":{"success":false,"message":"secret-cookie"}}}})).unwrap_err();
        assert!(!err.contains("secret-cookie"));
    }
}
