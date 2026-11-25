use axum::http::Uri;
use bytes::Bytes;
use serde_json::Value;

/// Discovery is an explicit projection of the already scoped, augmented Codex
/// catalog. The default response remains byte-for-byte intact for CLI clients.
pub fn response_body(body: Bytes, uri: &Uri) -> Result<Bytes, &'static str> {
    let discovery = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .any(|(key, value)| key == "view" && value == "discovery");
    if !discovery {
        return Ok(body);
    }

    let mut catalog: Value =
        serde_json::from_slice(&body).map_err(|_| "Invalid Codex model catalog")?;
    let models = catalog
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or("Invalid Codex model catalog")?;
    for model in models.iter_mut() {
        let model = model.as_object_mut().ok_or("Invalid Codex model entry")?;
        model
            .retain(|key, _| matches!(key.as_str(), "id" | "slug" | "display_name" | "visibility"));
    }
    serde_json::to_vec(&serde_json::json!({ "models": models }))
        .map(Bytes::from)
        .map_err(|_| "Failed to serialize Codex discovery catalog")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn full_catalog_is_unchanged_without_explicit_discovery() {
        let body = Bytes::from_static(br#"{ "models": [{"slug":"test","base_instructions":"runtime instructions"}], "extra":true }"#);
        for uri in [
            "/codex/models",
            "/codex/models?client_version=1.0.0",
            "/codex/models?view=full",
            "/codex/models?other=discovery",
        ] {
            assert_eq!(
                response_body(body.clone(), &uri.parse().unwrap()).unwrap(),
                body
            );
        }
    }

    #[test]
    fn discovery_preserves_entries_aliases_and_visibility_without_runtime_metadata() {
        let catalog = json!({
            "models": [
                {"slug":"test", "display_name":"Test", "visibility":"list",
                 "base_instructions":"x".repeat(900_000), "model_messages":{"instructions_template":"y".repeat(900_000)}},
                {"id":"cod:test", "slug":"cod:test", "display_name":"Test", "visibility":"list"},
                {"slug":"hidden", "visibility":"hide"},
                {"slug":"ctm:allowed", "display_name":"Custom"}
            ],
            "upstream_metadata":"must not be exposed"
        });
        let full = Bytes::from(serde_json::to_vec(&catalog).unwrap());
        let body = response_body(
            full.clone(),
            &"/codex/models?client_version=1.0.0&view=discovery"
                .parse()
                .unwrap(),
        )
        .unwrap();
        assert!(
            body.len() < 1024,
            "discovery size must not scale with instructions"
        );
        assert!(full.len() > 1_800_000);
        let discovery: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            discovery,
            json!({"models":[
                {"slug":"test", "display_name":"Test", "visibility":"list"},
                {"id":"cod:test", "slug":"cod:test", "display_name":"Test", "visibility":"list"},
                {"slug":"hidden", "visibility":"hide"},
                {"slug":"ctm:allowed", "display_name":"Custom"}
            ]})
        );
        assert_eq!(
            catalog["models"][0]["base_instructions"]
                .as_str()
                .unwrap()
                .len(),
            900_000
        );
    }

    #[test]
    fn discovery_handles_empty_catalog_and_rejects_invalid_source_without_leaking_it() {
        let uri: Uri = "/codex/models?view=discovery".parse().unwrap();
        assert_eq!(
            response_body(Bytes::from_static(br#"{"models":[]}"#), &uri).unwrap(),
            Bytes::from_static(br#"{"models":[]}"#)
        );
        for body in ["secret upstream error", "{}", r#"{"models":[null]}"#] {
            let error = response_body(Bytes::from(body.to_string()), &uri).unwrap_err();
            assert!(!error.contains("secret"));
        }
    }
}
