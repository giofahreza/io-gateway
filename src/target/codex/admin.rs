use axum::{
    extract::{Form, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use super::{auth, tokens};

#[derive(Deserialize)]
pub(crate) struct CallbackForm {
    redirect_url: String,
}

#[derive(Deserialize)]
pub(crate) struct DeleteForm {
    file_name: String,
}

#[derive(Deserialize)]
pub(crate) struct ToggleForm {
    file_name: String,
    enabled: String,
}

pub async fn login_start(State(state): State<crate::AppState>) -> impl IntoResponse {
    let (url, state_token, code_verifier) = match auth::build_auth_url() {
        Ok(v) => v,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("failed to create auth url: {}", err),
            )
                .into_response();
        }
    };
    {
        let mut pending = state.oauth_pending.lock().unwrap();
        pending.insert(
            state_token.clone(),
            super::auth::PendingOAuth {
                code_verifier,
                created_at: std::time::Instant::now(),
            },
        );
    }
    axum::Json(serde_json::json!({ "url": url, "state": state_token })).into_response()
}

pub async fn login_submit(
    State(state): State<crate::AppState>,
    Form(form): Form<CallbackForm>,
) -> impl IntoResponse {
    let redirect_url = form.redirect_url.trim();
    if redirect_url.is_empty() {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": "redirect_url is required"
        }))
        .into_response();
    }
    let (code, state_token) = match auth::parse_oauth_callback(redirect_url) {
        Ok(v) => v,
        Err(err) => {
            return axum::Json(serde_json::json!({
                "ok": false,
                "message": err
            }))
            .into_response()
        }
    };
    let code_verifier = {
        let mut pending = state.oauth_pending.lock().unwrap();
        match pending.remove(&state_token) {
            Some(p) => p.code_verifier,
            None => {
                return axum::Json(serde_json::json!({
                    "ok": false,
                    "message": "invalid or expired state"
                }))
                .into_response()
            }
        }
    };

    match auth::exchange_code_for_tokens(&state.client, &code, &code_verifier).await {
        Ok(token_resp) => match auth::save_auth(&state, &token_resp) {
            Ok(saved_path) => axum::Json(serde_json::json!({
                "ok": true,
                "message": format!("saved credentials to {}", saved_path)
            }))
            .into_response(),
            Err(err) => axum::Json(serde_json::json!({
                "ok": false,
                "message": err
            }))
            .into_response(),
        },
        Err(err) => axum::Json(serde_json::json!({
            "ok": false,
            "message": err
        }))
        .into_response(),
    }
}

pub async fn delete_credential(
    State(state): State<crate::AppState>,
    Form(form): Form<DeleteForm>,
) -> impl IntoResponse {
    let file_name = form.file_name.trim();
    if !is_safe_credential_filename(file_name) {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": "file_name must be a credential filename without path separators"
        }))
        .into_response();
    }
    let auth_dir = state
        .cfg
        .auth_dir
        .clone()
        .unwrap_or_else(|| "/root/dev/yow/io-gateway/auths".to_string());
    let path = std::path::Path::new(&auth_dir).join(file_name);
    match std::fs::remove_file(&path) {
        Ok(_) => {
            tokens::reload_state(&state);
            super::super::antigravity::accounts::reload_state(&state);
            super::super::gemini::accounts::reload_state(&state);
            super::super::qwen::accounts::reload_state(&state);
            super::super::deepseek::accounts::reload_state(&state);
            super::super::minimax::accounts::reload_state(&state);
            super::super::grok::accounts::reload_state(&state);
            super::super::copilot::accounts::reload_state(&state);
            super::super::claude::accounts::reload_state(&state);
            super::super::glm::accounts::reload_state(&state);
            super::super::compatible::accounts::reload_state(&state);
            axum::Json(serde_json::json!({
                "ok": true,
                "message": format!("deleted {}", file_name)
            }))
            .into_response()
        }
        Err(err) => axum::Json(serde_json::json!({
            "ok": false,
            "message": format!("delete failed: {}", err)
        }))
        .into_response(),
    }
}

pub async fn toggle_credential(
    State(state): State<crate::AppState>,
    Form(form): Form<ToggleForm>,
) -> impl IntoResponse {
    let file_name = form.file_name.trim();
    if !is_safe_credential_filename(file_name) {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": "file_name must be a credential filename without path separators"
        }))
        .into_response();
    }
    let enable = form.enabled.trim().eq_ignore_ascii_case("true");

    {
        let mut disabled = state.disabled.lock().unwrap();
        if enable {
            disabled.remove(file_name);
        } else {
            disabled.insert(file_name.to_string());
        }
    }

    if let Err(err) = persist_disabled_list(&state.config_path, &state.disabled) {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": format!("failed to persist: {}", err)
        }))
        .into_response();
    }

    tokens::reload_state(&state);
    super::super::antigravity::accounts::reload_state(&state);
    super::super::gemini::accounts::reload_state(&state);
    super::super::qwen::accounts::reload_state(&state);
    super::super::deepseek::accounts::reload_state(&state);
    super::super::minimax::accounts::reload_state(&state);
    super::super::grok::accounts::reload_state(&state);
    super::super::copilot::accounts::reload_state(&state);
    super::super::claude::accounts::reload_state(&state);
    super::super::glm::accounts::reload_state(&state);
    super::super::compatible::accounts::reload_state(&state);
    if enable {
        crate::router_clear_credential_file_cooldown(&state, file_name);
    }

    axum::Json(serde_json::json!({
        "ok": true,
        "message": format!("{} {}", if enable { "enabled" } else { "disabled" }, file_name)
    }))
    .into_response()
}

fn persist_disabled_list(
    config_path: &std::path::Path,
    disabled: &Arc<Mutex<HashSet<String>>>,
) -> Result<(), String> {
    let mut v: serde_json::Value = {
        let data = std::fs::read_to_string(config_path).map_err(|e| e.to_string())?;
        serde_json::from_str(&data).map_err(|e| e.to_string())?
    };
    let list: Vec<String> = disabled.lock().unwrap().iter().cloned().collect();
    if let serde_json::Value::Object(map) = &mut v {
        if list.is_empty() {
            map.remove("disabled_files");
        } else {
            map.insert("disabled_files".to_string(), serde_json::json!(list));
        }
    }
    let data = serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?;
    super::super::atomic_write(config_path, &data, true)
}

fn is_safe_credential_filename(file_name: &str) -> bool {
    let path = std::path::Path::new(file_name);
    !file_name.is_empty()
        && path.components().count() == 1
        && matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
}

#[cfg(test)]
mod tests {
    use super::persist_disabled_list;
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
    };

    #[test]
    fn disabled_files_are_persisted_to_the_selected_config_path() {
        let directory = std::env::temp_dir().join(format!(
            "io-gateway-codex-admin-tests-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let config_path = directory.join("custom-config.json");
        std::fs::write(
            &config_path,
            r#"{"listen":"127.0.0.1:8319","disabled_files":["old.json"]}"#,
        )
        .expect("write config");
        let disabled = Arc::new(Mutex::new(HashSet::from(["new.json".to_string()])));

        persist_disabled_list(&config_path, &disabled).expect("persist disabled files");

        let saved: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&config_path).expect("read saved config"),
        )
        .expect("parse saved config");
        assert_eq!(saved["disabled_files"], serde_json::json!(["new.json"]));
        let _ = std::fs::remove_dir_all(directory);
    }
}
