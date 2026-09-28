//! Request-lifetime integration for the durable renewable quota ledger.
//!
//! The context owns unfinished work even before a streaming body is polled.
//! Dropping the last owner never refunds a possibly dispatched request.

use crate::{api_key_quota as ledger, AppState, Config, SourceApi};
use axum::{
    http::{HeaderValue, StatusCode},
    response::Response,
};
use chrono::Utc;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tracing::error;
use uuid::Uuid;

pub(crate) struct QuotaRequestContext {
    cfg: Arc<Config>,
    pub request_id: String,
    pub api_key_id: String,
    active: Mutex<Option<String>>,
}

impl QuotaRequestContext {
    fn settle(&self, settlement: ledger::Settlement) {
        let Some(id) = self.active.lock().unwrap().take() else {
            return;
        };
        if let Err(err) = ledger::settle(self.cfg.as_ref(), &id, settlement) {
            // The committed reservation is still on disk. Recovery will retain
            // its conservative charge; never release allowance on a write error.
            error!(attempt_id = %id, "renewable quota settlement deferred: {}", err);
        }
    }
}

impl Drop for QuotaRequestContext {
    fn drop(&mut self) {
        self.settle(ledger::Settlement::Unknown);
    }
}

/// Own the admission result until the awaiting request explicitly accepts it.
/// A spawn_blocking job continues after its awaiting HTTP future is dropped.
/// Keeping the context and this lease in the job/result ensures cancellation
/// cannot drop the context before the transaction commits, then strand a newly
/// created hold. No upstream dispatch is possible until `handoff` is called.
struct PendingQuotaAdmission {
    context: Arc<QuotaRequestContext>,
    attempt_id: String,
    release_on_drop: bool,
    handed_off: bool,
}

impl PendingQuotaAdmission {
    fn handoff(&mut self) {
        self.handed_off = true;
    }
}

impl Drop for PendingQuotaAdmission {
    fn drop(&mut self) {
        if self.handed_off {
            return;
        }
        let owns_attempt = {
            let mut active = self.context.active.lock().unwrap();
            if active.as_deref() == Some(self.attempt_id.as_str()) {
                active.take();
                true
            } else {
                false
            }
        };
        if owns_attempt && self.release_on_drop {
            if let Err(err) = ledger::settle(
                self.context.cfg.as_ref(),
                &self.attempt_id,
                ledger::Settlement::Released,
            ) {
                // Retain any committed hold on a storage failure. Recovery is
                // conservative; cancellation must never invent free capacity.
                error!(attempt_id = %self.attempt_id, "unable to release cancelled quota admission: {}", err);
            }
        }
    }
}

fn spawn_admission(
    context: Arc<QuotaRequestContext>,
    request: ledger::ReserveRequest,
    reserve: impl FnOnce(&Config, &ledger::ReserveRequest) -> Result<ledger::Admission, String>
        + Send
        + 'static,
) -> tokio::task::JoinHandle<(Result<ledger::Admission, String>, PendingQuotaAdmission)> {
    tokio::task::spawn_blocking(move || {
        let mut pending = PendingQuotaAdmission {
            context,
            attempt_id: request.attempt_id.clone(),
            release_on_drop: true,
            handed_off: false,
        };
        let result = reserve(pending.context.cfg.as_ref(), &request);
        pending.release_on_drop = match &result {
            Ok(ledger::Admission::Reserved(reservation)) => !reservation.replay,
            Ok(_) => false,
            // A failed commit can have an indeterminate outcome. Since this
            // job has not dispatched, try releasing any visible reservation.
            Err(_) => true,
        };
        (result, pending)
    })
}

pub(crate) fn request_context(state: &AppState) -> Option<Arc<QuotaRequestContext>> {
    Some(Arc::new(QuotaRequestContext {
        cfg: state.cfg.clone(),
        api_key_id: state.request_api_key_id.clone()?,
        request_id: Uuid::new_v4().simple().to_string(),
        active: Mutex::new(None),
    }))
}

/// Called for each actual provider attempt with the final prepared body and
/// canonical account identity. Even an originally unlimited authentication
/// snapshot must recheck the current SQLite authority before dispatch.
pub(crate) async fn reserve_api_key_budgets_for_prepared_dispatch(
    state: &AppState,
    provider: &str,
    account_key: &str,
    payload: &[u8],
) -> Result<(), Response> {
    reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
        state,
        provider,
        account_key,
        payload,
        crate::quota_usage::PreparedProtocol::ProviderDefault,
    )
    .await
}

pub(crate) async fn reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
    state: &AppState,
    provider: &str,
    account_key: &str,
    payload: &[u8],
    protocol: crate::quota_usage::PreparedProtocol,
) -> Result<(), Response> {
    crate::reserve_api_key_budget_for_upstream_dispatch(state).await?;
    let Some(context) = state.request_api_key_quota.as_ref() else {
        return Ok(());
    };
    let bounds = serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| {
            crate::quota_usage::prepared_bounds_for_protocol(provider, &value, protocol).ok()
        });
    let input = bounds
        .as_ref()
        .filter(|bound| bound.input_measurable)
        .map(|bound| bound.input_upper_bound);
    let id = Uuid::new_v4().simple().to_string();
    {
        let mut active = context.active.lock().unwrap();
        if active.is_some() {
            return Err(storage_error(state.request_source_api));
        }
        *active = Some(id.clone());
    }
    let request = ledger::ReserveRequest {
        api_key_id: context.api_key_id.clone(),
        request_id: context.request_id.clone(),
        attempt_id: id,
        provider: Some(provider.to_string()),
        account_key: Some(account_key.to_string()),
        expected_legacy_budget: state
            .request_api_key_budget
            .as_ref()
            .map(|reservation| reservation.budget.clone()),
        bounds: ledger::Usage {
            input_tokens: input,
            uncached_input_tokens: input,
            cache_read_tokens: input,
            cache_write_tokens: input,
            cache_tokens: input,
            output_tokens: bounds.and_then(|bound| bound.output_upper_bound),
        },
    };
    let (result, mut pending_admission) =
        match spawn_admission(context.clone(), request, ledger::reserve).await {
            Ok((result, pending)) => (Ok(result), Some(pending)),
            Err(error) => (Err(error), None),
        };
    match result {
        Ok(Ok(ledger::Admission::Reserved(reservation))) if !reservation.replay => {
            pending_admission
                .as_mut()
                .expect("completed admission owns its lease")
                .handoff();
            Ok(())
        }
        Ok(Ok(ledger::Admission::Reserved(_))) => {
            // A replay belongs to an existing attempt that may already have
            // dispatched. Reject duplicate execution without refunding it.
            context.active.lock().unwrap().take();
            crate::release_api_key_budget_before_dispatch(state);
            error!("unexpected renewable quota reservation replay");
            Err(storage_error(state.request_source_api))
        }
        Ok(Ok(ledger::Admission::NotConfigured)) => {
            context.active.lock().unwrap().take();
            Ok(())
        }
        Ok(Ok(ledger::Admission::Denied(denial))) => {
            context.active.lock().unwrap().take();
            crate::release_api_key_budget_before_dispatch(state);
            let response = denial_response(state.request_source_api, &denial);
            let decision = match denial.code.as_str() {
                "scope_denied" | "key_revoked" => {
                    crate::api_key_policy_store::ApiKeyPolicyDecision::ScopeDenied
                }
                "token_limit_exceeded" => {
                    crate::api_key_policy_store::ApiKeyPolicyDecision::RequestLimitExceeded
                }
                "quota_exceeded" => {
                    crate::api_key_policy_store::ApiKeyPolicyDecision::BudgetExceeded
                }
                "api_key_policy_changed" => {
                    crate::api_key_policy_store::ApiKeyPolicyDecision::Failed
                }
                _ => crate::api_key_policy_store::ApiKeyPolicyDecision::MeasurementRequired,
            };
            let event = crate::api_key_policy_store::ApiKeyRequestAuditEvent {
                request_id: context.request_id.clone(),
                api_key_id: context.api_key_id.clone(),
                kind: crate::api_key_policy_store::ApiKeyPolicyEventKind::Authorization,
                decision,
                request_path: "/generation".into(),
                provider: Some(provider.into()),
                account_key: Some(account_key.into()),
                model: None,
                status_code: Some(response.status().as_u16()),
                estimated_input_tokens: input,
                actual_input_tokens: None,
                measurement: Some(if input.is_some() {
                    crate::api_key_policy_store::ApiKeyInputMeasurement::Conservative
                } else {
                    crate::api_key_policy_store::ApiKeyInputMeasurement::Unknown
                }),
                reservation_id: None,
            };
            if let Err(err) = crate::api_key_policy_store::append_event(state.cfg.as_ref(), &event)
            {
                error!("failed to persist quota denial audit: {}", err);
            }
            Err(response)
        }
        other => {
            // A new server-generated attempt UUID cannot legitimately replay;
            // never let local idempotency authorize a second upstream call.
            match other {
                Ok(Err(err)) => error!("renewable quota admission failed: {}", err),
                Err(err) => error!("renewable quota admission worker failed: {}", err),
                _ => unreachable!("all successful admission outcomes are handled above"),
            }
            context.settle(ledger::Settlement::Released);
            crate::release_api_key_budget_before_dispatch(state);
            Err(storage_error(state.request_source_api))
        }
    }
}

fn storage_error(source: SourceApi) -> Response {
    crate::source_error_response(
        source,
        StatusCode::SERVICE_UNAVAILABLE,
        "Quota accounting is unavailable; no upstream request was sent",
        "server_error",
        Some("quota_storage_unavailable"),
        "api_error",
    )
}

fn denial_response(source: SourceApi, denial: &ledger::Denial) -> Response {
    let (status, error_type, claude_type) = match denial.code.as_str() {
        "quota_exceeded" | "token_limit_exceeded" => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "rate_limit_error",
        ),
        "key_revoked" => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "authentication_error",
        ),
        "scope_denied" => (
            StatusCode::FORBIDDEN,
            "permission_error",
            "permission_error",
        ),
        "api_key_policy_changed" => (
            StatusCode::CONFLICT,
            "invalid_request_error",
            "invalid_request_error",
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_request_error",
        ),
    };
    let mut response = crate::source_error_response(
        source,
        status,
        denial.message.clone(),
        error_type,
        Some(&denial.code),
        claude_type,
    );
    if let Some(seconds) = denial.retry_after_seconds {
        if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
            response.headers_mut().insert("retry-after", value);
        }
    }
    if let Some(reset) = &denial.reset_at {
        if let Ok(value) = HeaderValue::from_str(reset) {
            response.headers_mut().insert("x-quota-reset-at", value);
        }
    }
    if let Some(remaining) = denial.remaining {
        if let Ok(value) = HeaderValue::from_str(&remaining.to_string()) {
            response.headers_mut().insert("x-quota-remaining", value);
        }
    }
    response
}

pub(crate) fn release_before_dispatch(state: &AppState) {
    if let Some(context) = &state.request_api_key_quota {
        context.settle(ledger::Settlement::Released);
    }
}

pub(crate) fn settle_unknown(state: &AppState) {
    if let Some(context) = &state.request_api_key_quota {
        context.settle(ledger::Settlement::Unknown);
    }
}

pub(crate) fn settle_usage(state: &AppState, provider: &str, raw: Option<&Value>) {
    let Some(context) = &state.request_api_key_quota else {
        return;
    };
    let normalized = raw.map(|raw| crate::quota_usage::normalize(provider, raw, true));
    let Some(usage) = normalized.filter(|usage| usage.trustworthy_final) else {
        context.settle(ledger::Settlement::Unknown);
        return;
    };
    context.settle(ledger::Settlement::Reported(ledger::Usage {
        input_tokens: usage.input_tokens,
        uncached_input_tokens: usage.uncached_input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cache_tokens: usage.cache_tokens,
    }));
}

pub(crate) fn summaries(cfg: &Config, keys: &[crate::api_keys::PublicApiKeyRecord]) -> Value {
    let mut values = serde_json::Map::new();
    for key in keys.iter().filter(|key| key.access.quota.is_some()) {
        match ledger::summary(cfg, &key.id) {
            Ok(Some(summary)) => {
                values.insert(
                    key.id.clone(),
                    serde_json::to_value(summary).unwrap_or(Value::Null),
                );
            }
            Ok(None) => {}
            Err(err) => {
                error!("failed to load API-key quota summary: {}", err);
                values.insert(key.id.clone(), serde_json::json!({"error": true}));
            }
        }
    }
    Value::Object(values)
}

pub(crate) fn recover(cfg: &Config) {
    let now = Utc::now();
    let cutoff = now
        - chrono::Duration::seconds(crate::API_KEY_BUDGET_STALE_RESERVATION_AGE.as_secs() as i64);
    if let Err(err) = ledger::recover_at_path(
        &crate::api_key_policy_store::policy_db_path(cfg),
        cutoff,
        now,
        crate::API_KEY_BUDGET_SWEEP_BATCH_SIZE,
    ) {
        error!("failed to recover renewable quota reservations: {}", err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        directory: std::path::PathBuf,
        cfg: Arc<Config>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("io-gateway-quota-runtime-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let cfg: Config = serde_json::from_value(serde_json::json!({
                "listen":"127.0.0.1:0", "upstream_base":"https://example.test",
                "proxy_api_key":"", "tokens":[], "auth_dir":directory
            }))
            .unwrap();
            let mut store = crate::api_keys::load(&cfg).unwrap();
            store.keys.push(crate::api_keys::ApiKeyRecord {
                id: "runtime-key".into(),
                access: crate::api_keys::ApiKeyAccess {
                    quota: Some(ledger::QuotaPolicy {
                        timezone: "UTC".into(),
                        rules: vec![ledger::QuotaRule {
                            metric: ledger::QuotaMetric::OutputTokens,
                            period: ledger::QuotaPeriod::Weekly,
                            limit: 100,
                        }],
                    }),
                    ..Default::default()
                },
                ..Default::default()
            });
            crate::api_keys::save(&cfg, &store).unwrap();
            Self {
                directory,
                cfg: Arc::new(cfg),
            }
        }

        fn pending_context(&self) -> (Arc<QuotaRequestContext>, ledger::ReserveRequest) {
            let attempt = Uuid::new_v4().to_string();
            let context = Arc::new(QuotaRequestContext {
                cfg: self.cfg.clone(),
                api_key_id: "runtime-key".into(),
                request_id: Uuid::new_v4().to_string(),
                active: Mutex::new(Some(attempt.clone())),
            });
            let request = ledger::ReserveRequest {
                api_key_id: context.api_key_id.clone(),
                request_id: context.request_id.clone(),
                attempt_id: attempt,
                provider: Some("claude".into()),
                account_key: Some("claude:test".into()),
                expected_legacy_budget: None,
                bounds: ledger::Usage {
                    output_tokens: Some(40),
                    ..Default::default()
                },
            };
            (context, request)
        }

        fn reserve_context(&self) -> Arc<QuotaRequestContext> {
            let (context, request) = self.pending_context();
            assert!(matches!(
                ledger::reserve(&self.cfg, &request).unwrap(),
                ledger::Admission::Reserved(_)
            ));
            context
        }

        fn summary(&self) -> ledger::QuotaSummary {
            ledger::summary(&self.cfg, "runtime-key").unwrap().unwrap()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_admission_before_or_after_commit_releases_without_starting_first_use() {
        for pause_after_commit in [false, true] {
            let fixture = Fixture::new();
            let (context, request) = fixture.pending_context();
            let context_lifetime = Arc::downgrade(&context);
            let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            let job = spawn_admission(context.clone(), request, move |cfg, request| {
                let pause = move || {
                    paused_tx.send(()).unwrap();
                    resume_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                };
                if pause_after_commit {
                    let result = ledger::reserve(cfg, request);
                    pause();
                    result
                } else {
                    pause();
                    ledger::reserve(cfg, request)
                }
            });
            paused_rx.await.unwrap();
            drop(job); // The HTTP waiter is gone, but its blocking job continues.
            drop(context);
            assert!(
                context_lifetime.upgrade().is_some(),
                "worker must own unfinished admission"
            );
            resume_tx.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while context_lifetime.upgrade().is_some() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let summary = fixture.summary();
            assert!(summary.anchor.is_none());
            let rule = &summary.rules[0];
            assert_eq!(
                (
                    rule.confirmed,
                    rule.reserved,
                    rule.uncertain,
                    rule.remaining
                ),
                (0, 0, 0, 100)
            );
        }
    }

    #[tokio::test]
    async fn accepted_admission_handoff_never_refunds_a_possibly_dispatched_request() {
        let fixture = Fixture::new();
        let (context, request) = fixture.pending_context();
        let (admission, mut pending) = spawn_admission(context.clone(), request, ledger::reserve)
            .await
            .unwrap();
        assert!(matches!(admission.unwrap(), ledger::Admission::Reserved(_)));
        pending.handoff();
        drop(pending);
        assert_eq!(fixture.summary().rules[0].reserved, 40);
        drop(context);
        let rule = &fixture.summary().rules[0];
        assert_eq!(
            (
                rule.confirmed,
                rule.reserved,
                rule.uncertain,
                rule.remaining
            ),
            (0, 0, 40, 60)
        );
    }

    #[tokio::test]
    async fn rejected_replay_does_not_refund_an_existing_reservation() {
        let fixture = Fixture::new();
        let (context, request) = fixture.pending_context();
        assert!(matches!(
            ledger::reserve(&fixture.cfg, &request).unwrap(),
            ledger::Admission::Reserved(ledger::Reservation { replay: false, .. })
        ));
        let (admission, pending) = spawn_admission(context.clone(), request, ledger::reserve)
            .await
            .unwrap();
        assert!(matches!(
            admission.unwrap(),
            ledger::Admission::Reserved(ledger::Reservation { replay: true, .. })
        ));
        // A replay may already have dispatched. Duplicate execution relinquishes
        // local ownership without applying an undispatched-request refund.
        context.active.lock().unwrap().take();
        drop(pending);
        drop(context);
        let rule = &fixture.summary().rules[0];
        assert_eq!(
            (
                rule.confirmed,
                rule.reserved,
                rule.uncertain,
                rule.remaining
            ),
            (0, 40, 0, 60)
        );
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn quota_context_last_drop_preserves_unpolled_stream_allowance_as_uncertain() {
        let fixture = Fixture::new();
        let context = fixture.reserve_context();
        let stream_owner = context.clone();
        drop(context);
        assert_eq!(fixture.summary().rules[0].reserved, 40);
        drop(stream_owner);
        let summary = fixture.summary();
        assert_eq!(
            (
                summary.rules[0].reserved,
                summary.rules[0].uncertain,
                summary.rules[0].remaining
            ),
            (0, 40, 60)
        );
    }

    #[test]
    fn quota_context_completed_settlement_is_durable_and_drop_does_not_double_charge() {
        let fixture = Fixture::new();
        let context = fixture.reserve_context();
        context.settle(ledger::Settlement::Reported(ledger::Usage {
            output_tokens: Some(7),
            ..Default::default()
        }));
        drop(context);
        let summary = fixture.summary();
        assert_eq!(
            (
                summary.rules[0].confirmed,
                summary.rules[0].reserved,
                summary.rules[0].uncertain
            ),
            (7, 0, 0)
        );
    }

    #[test]
    fn quota_context_proven_local_failure_releases_without_an_uncertain_charge() {
        let fixture = Fixture::new();
        let context = fixture.reserve_context();
        context.settle(ledger::Settlement::Released);
        drop(context);
        let summary = fixture.summary();
        assert_eq!(
            (
                summary.rules[0].confirmed,
                summary.rules[0].reserved,
                summary.rules[0].uncertain
            ),
            (0, 0, 0)
        );
        assert_eq!(summary.rules[0].remaining, 100);
    }

    #[test]
    fn quota_denials_have_protocol_status_and_reset_headers() {
        let denial = ledger::Denial {
            code: "quota_exceeded".into(),
            message: "Quota exhausted".into(),
            metric: Some(ledger::QuotaMetric::Requests),
            period: Some(ledger::QuotaPeriod::Weekly),
            limit: Some(1),
            remaining: Some(0),
            reset_at: Some("2026-09-29T08:00:00Z".into()),
            retry_after_seconds: Some(25),
        };
        let response = denial_response(SourceApi::V1, &denial);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "25");
        assert_eq!(response.headers()["x-quota-remaining"], "0");
        assert!(response.headers().contains_key("x-quota-reset-at"));
    }

    #[test]
    fn missing_observation_is_not_a_zero_usage_measurement() {
        let metrics = crate::UsageMetrics::default();
        assert!(metrics.raw_usage.is_none());
        let usage = crate::quota_usage::normalize("deepseek", &serde_json::json!({}), true);
        assert!(usage.input_tokens.is_none());
        assert!(usage.output_tokens.is_none());
    }
}
