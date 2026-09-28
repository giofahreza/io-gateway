//! Request-policy auditing for keys without a cumulative budget.
//!
//! Budget reservations already carry an audit lifecycle. A key with only a
//! per-request cap (or no cap) still needs dispatch and outcome events, without
//! creating a fictitious budget reservation. All adapter clones share this
//! state so a stream cleanup cannot settle the same attempt twice.

use crate::api_key_policy_store::{
    ApiKeyInputMeasurement, ApiKeyPolicyDecision, ApiKeyPolicyEventKind, ApiKeyRequestAuditEvent,
};
use std::sync::Mutex;

pub(crate) struct RequestAudit {
    base: ApiKeyRequestAuditEvent,
    active: Mutex<Option<ApiKeyRequestAuditEvent>>,
}

impl RequestAudit {
    pub(crate) fn new(
        api_key_id: String,
        request_path: String,
        estimated_input_tokens: u64,
        complete: bool,
    ) -> Self {
        Self {
            base: ApiKeyRequestAuditEvent {
                request_id: uuid::Uuid::new_v4().simple().to_string(),
                api_key_id,
                kind: ApiKeyPolicyEventKind::Dispatch,
                decision: ApiKeyPolicyDecision::Allowed,
                request_path,
                provider: None,
                model: None,
                account_key: None,
                status_code: None,
                estimated_input_tokens: Some(estimated_input_tokens),
                actual_input_tokens: None,
                measurement: Some(if complete {
                    ApiKeyInputMeasurement::Conservative
                } else {
                    ApiKeyInputMeasurement::Unknown
                }),
                reservation_id: None,
            },
            active: Mutex::new(None),
        }
    }

    pub(crate) fn begin(
        &self,
        provider: &str,
        account_key: &str,
    ) -> Option<ApiKeyRequestAuditEvent> {
        let mut active = self.active.lock().unwrap();
        if active.is_some() {
            return None;
        }
        let mut event = self.base.clone();
        event.provider = Some(provider.to_string());
        event.account_key = Some(account_key.to_string());
        *active = Some(event.clone());
        Some(event)
    }

    pub(crate) fn settle(
        &self,
        decision: ApiKeyPolicyDecision,
        actual_input_tokens: Option<u64>,
    ) -> Option<ApiKeyRequestAuditEvent> {
        let mut event = self.active.lock().unwrap().take()?;
        event.kind = ApiKeyPolicyEventKind::Settlement;
        event.decision = decision;
        event.actual_input_tokens = actual_input_tokens;
        if actual_input_tokens.is_some() {
            event.measurement = Some(ApiKeyInputMeasurement::Exact);
        }
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn cap_only_attempts_keep_one_request_id_and_settle_once_across_clones() {
        let audit = Arc::new(RequestAudit::new(
            "key-one".into(),
            "/v1/responses".into(),
            106,
            true,
        ));
        let stream_clone = audit.clone();
        let first = audit.begin("deepseek", "account-one").unwrap();
        assert!(first.reservation_id.is_none());
        assert!(audit.begin("deepseek", "account-one").is_none());
        let failed = stream_clone
            .settle(ApiKeyPolicyDecision::Failed, None)
            .unwrap();
        assert_eq!(failed.request_id, first.request_id);
        assert!(audit.settle(ApiKeyPolicyDecision::Failed, None).is_none());

        let retry = audit.begin("deepseek", "account-two").unwrap();
        assert_eq!(retry.request_id, first.request_id);
        let completed = stream_clone
            .settle(ApiKeyPolicyDecision::Completed, Some(34))
            .unwrap();
        assert_eq!(completed.account_key.as_deref(), Some("account-two"));
        assert_eq!(completed.estimated_input_tokens, Some(106));
        assert_eq!(completed.actual_input_tokens, Some(34));
        assert_eq!(completed.measurement, Some(ApiKeyInputMeasurement::Exact));
        assert!(audit.settle(ApiKeyPolicyDecision::Failed, None).is_none());
        assert!(completed.model.is_none());
    }

    #[test]
    fn unknown_input_and_aborted_usage_are_not_reported_as_exact_zero() {
        let audit = RequestAudit::new("key-one".into(), "/v1/responses".into(), 12, false);
        assert!(audit.settle(ApiKeyPolicyDecision::Failed, None).is_none());
        let dispatch = audit.begin("grok", "account-one").unwrap();
        assert_eq!(dispatch.measurement, Some(ApiKeyInputMeasurement::Unknown));
        let failed = audit.settle(ApiKeyPolicyDecision::Failed, None).unwrap();
        assert_eq!(failed.actual_input_tokens, None);
        assert_eq!(failed.measurement, Some(ApiKeyInputMeasurement::Unknown));
    }

    #[test]
    fn cap_only_success_is_persisted_without_a_budget_reservation() {
        use crate::api_key_policy_store::{append_event, list_events, ApiKeyRequestAuditQuery};

        let directory = std::env::temp_dir().join(format!(
            "io-gateway-request-audit-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&directory).unwrap();
        let cfg: crate::Config = serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:0",
            "upstream_base": "http://127.0.0.1:1",
            "proxy_api_key": "",
            "tokens": [],
            "auth_dir": directory.to_str().unwrap()
        }))
        .unwrap();
        let audit = RequestAudit::new("cap-only".into(), "/v1/responses".into(), 106, true);
        append_event(&cfg, &audit.begin("deepseek", "account-one").unwrap()).unwrap();
        append_event(
            &cfg,
            &audit
                .settle(ApiKeyPolicyDecision::Completed, Some(34))
                .unwrap(),
        )
        .unwrap();
        let events = list_events(
            &cfg,
            &ApiKeyRequestAuditQuery {
                api_key_id: Some("cap-only".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(events.len(), 2);
        let completed = events
            .iter()
            .find(|row| row.event.kind == ApiKeyPolicyEventKind::Settlement)
            .unwrap();
        assert_eq!(completed.event.decision, ApiKeyPolicyDecision::Completed);
        assert_eq!(completed.event.actual_input_tokens, Some(34));
        assert_eq!(events[0].event.request_id, events[1].event.request_id);
        assert!(events.iter().all(|row| row.event.reservation_id.is_none()));
        let connection =
            rusqlite::Connection::open(crate::api_key_policy_store::policy_db_path(&cfg)).unwrap();
        let reservations: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM api_key_budget_reservations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reservations, 0);
        drop(connection);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
