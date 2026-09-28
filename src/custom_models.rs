use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CustomModel {
    pub alias: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_load_balance", skip_serializing)]
    pub load_balance: bool,
    #[serde(default, alias = "route_groups")]
    pub routes: Vec<CustomModelRouteGroup>,
    #[serde(default, skip_serializing)]
    pub primary_models: Vec<CustomModelTarget>,
    #[serde(default, skip_serializing)]
    pub fallback_models: Vec<CustomModelTarget>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct CustomModelRouteGroup {
    #[serde(default)]
    pub targets: Vec<CustomModelTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CustomModelTarget {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_default_account_condition",
        alias = "account_mode"
    )]
    pub account_condition: CustomModelAccountCondition,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CustomModelAccountCondition {
    #[default]
    Only,
    Except,
}

#[derive(Default, Deserialize, Serialize)]
struct CustomModelFile {
    #[serde(default)]
    models: Vec<CustomModel>,
}

fn default_enabled() -> bool {
    true
}

fn default_load_balance() -> bool {
    true
}

fn default_weight() -> u32 {
    1
}

fn is_default_account_condition(condition: &CustomModelAccountCondition) -> bool {
    matches!(condition, CustomModelAccountCondition::Only)
}

pub(crate) fn custom_models_path(cfg: &crate::Config) -> PathBuf {
    cfg.auth_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("custom-models.json")
}

pub(crate) fn load(cfg: &crate::Config) -> Vec<CustomModel> {
    let path = custom_models_path(cfg);
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(file) = serde_json::from_str::<CustomModelFile>(&data) else {
        return Vec::new();
    };
    normalize_models(file.models)
}

pub(crate) fn save(cfg: &crate::Config, models: &[CustomModel]) -> Result<(), String> {
    let path = custom_models_path(cfg);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let file = CustomModelFile {
        models: normalize_models(models.to_vec()),
    };
    let data = serde_json::to_vec_pretty(&file).map_err(|err| err.to_string())?;
    std::fs::write(path, data).map_err(|err| err.to_string())
}

pub(crate) fn normalize_alias(alias: &str) -> String {
    alias
        .trim()
        .strip_prefix("ctm:")
        .unwrap_or_else(|| alias.trim())
        .trim()
        .to_string()
}

pub(crate) fn public_model_id(alias: &str) -> String {
    format!("ctm:{}", normalize_alias(alias))
}

pub(crate) fn validate_model(model: &CustomModel) -> Result<(), String> {
    validate_alias(&model.alias)?;
    if model
        .routes
        .iter()
        .flat_map(|group| group.targets.iter())
        .filter(|target| target.enabled)
        .count()
        == 0
    {
        return Err("at least one enabled route target is required".to_string());
    }
    for group in &model.routes {
        for target in &group.targets {
            validate_target(target)?;
        }
    }
    Ok(())
}

fn validate_alias(alias: &str) -> Result<(), String> {
    let alias = normalize_alias(alias);
    if alias.is_empty() {
        return Err("alias is required".to_string());
    }
    if alias
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, ':' | '/' | '\\'))
    {
        return Err("alias must not contain whitespace, colon, slash, or backslash".to_string());
    }
    Ok(())
}

fn validate_target(target: &CustomModelTarget) -> Result<(), String> {
    let model = target.model.trim();
    if model.is_empty() {
        return Err("target model is required".to_string());
    }
    let normalized_model = normalize_target_model_id(model);
    if normalized_model.starts_with("ctm:") {
        return Err("custom models cannot target another custom model".to_string());
    }
    if normalized_model.contains(':') && !is_supported_target_prefix(&normalized_model) {
        return Err(format!("unsupported target model prefix in '{}'", model));
    }
    Ok(())
}

fn is_supported_target_prefix(model: &str) -> bool {
    let Some((prefix, rest)) = model.split_once(':') else {
        return true;
    };
    !rest.trim().is_empty() && canonical_target_provider_prefix(prefix).is_some()
}

fn canonical_target_provider_prefix(prefix: &str) -> Option<&'static str> {
    match prefix.trim().to_ascii_lowercase().as_str() {
        "agw" | "antigravity" | "anti-gravity" => Some("agw"),
        "gem" | "gemini" => Some("gem"),
        "qwn" | "qwen" => Some("qwn"),
        "dsk" | "deepseek" => Some("dsk"),
        "grk" | "grok" | "xai" => Some("grk"),
        "min" | "minimax" => Some("min"),
        "cop" | "copilot" | "github-copilot" | "github_copilot" => Some("cop"),
        "cld" | "claude" | "anthropic" => Some("cld"),
        "glm" | "zai" | "z-ai" => Some("glm"),
        "cod" | "codex" => Some("cod"),
        "ctm" | "custom" => Some("ctm"),
        _ => None,
    }
}

fn normalize_target_model_id(model: &str) -> String {
    let model = model.trim();
    let Some((prefix, rest)) = model.split_once(':') else {
        return model.to_string();
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return model.to_string();
    };
    match canonical_target_provider_prefix(prefix) {
        Some(canonical) => format!("{}:{}", canonical, rest),
        None => model.to_string(),
    }
}

pub(crate) fn normalize_model(mut model: CustomModel) -> CustomModel {
    model.alias = normalize_alias(&model.alias);
    model.display_name = model
        .display_name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    model.primary_models = normalize_targets(model.primary_models);
    model.fallback_models = normalize_targets(model.fallback_models);
    model.routes = normalize_route_groups(model.routes);
    if model.routes.is_empty() {
        model.routes = legacy_route_groups(
            model.load_balance,
            model.primary_models.clone(),
            model.fallback_models.clone(),
        );
    }
    model.primary_models.clear();
    model.fallback_models.clear();
    model.load_balance = true;
    model
}

fn normalize_models(models: Vec<CustomModel>) -> Vec<CustomModel> {
    let mut out = Vec::new();
    for model in models {
        let model = normalize_model(model);
        if validate_model(&model).is_ok() {
            out.retain(|existing: &CustomModel| !existing.alias.eq_ignore_ascii_case(&model.alias));
            out.push(model);
        }
    }
    out.sort_by(|left, right| left.alias.cmp(&right.alias));
    out
}

fn normalize_targets(targets: Vec<CustomModelTarget>) -> Vec<CustomModelTarget> {
    targets
        .into_iter()
        .map(|mut target| {
            let (model, account, condition) = parse_target_spec(&target.model);
            target.model = model;
            target.account = target
                .account
                .or(account)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            if matches!(target.account_condition, CustomModelAccountCondition::Only)
                && matches!(condition, CustomModelAccountCondition::Except)
            {
                target.account_condition = condition;
            }
            if target.account.is_none() {
                target.account_condition = CustomModelAccountCondition::Only;
            }
            if target.weight == 0 {
                target.weight = 1;
            }
            target
        })
        .filter(|target| !target.model.is_empty())
        .collect()
}

fn normalize_route_groups(groups: Vec<CustomModelRouteGroup>) -> Vec<CustomModelRouteGroup> {
    groups
        .into_iter()
        .map(|group| CustomModelRouteGroup {
            targets: normalize_targets(group.targets),
        })
        .filter(|group| !group.targets.is_empty())
        .collect()
}

fn legacy_route_groups(
    load_balance: bool,
    primary: Vec<CustomModelTarget>,
    fallback: Vec<CustomModelTarget>,
) -> Vec<CustomModelRouteGroup> {
    let mut groups = Vec::new();
    let primary = normalize_targets(primary);
    if load_balance {
        if !primary.is_empty() {
            groups.push(CustomModelRouteGroup { targets: primary });
        }
    } else {
        groups.extend(primary.into_iter().map(|target| CustomModelRouteGroup {
            targets: vec![target],
        }));
    }
    groups.extend(
        normalize_targets(fallback)
            .into_iter()
            .map(|target| CustomModelRouteGroup {
                targets: vec![target],
            }),
    );
    groups
}

fn parse_target_spec(value: &str) -> (String, Option<String>, CustomModelAccountCondition) {
    let trimmed = value.trim();
    let Some((model, account)) = trimmed.split_once('@') else {
        return (
            normalize_target_model_id(trimmed),
            None,
            CustomModelAccountCondition::Only,
        );
    };
    let account = account.trim();
    let (account, condition) = match account.strip_prefix('!') {
        Some(account) => (account.trim(), CustomModelAccountCondition::Except),
        None => (account, CustomModelAccountCondition::Only),
    };
    (
        normalize_target_model_id(model),
        Some(account.to_string()).filter(|value| !value.is_empty()),
        condition,
    )
}

pub(crate) fn parse_model_list(value: &str) -> Vec<CustomModelTarget> {
    value
        .lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(|spec| {
            let (model, account, account_condition) = parse_target_spec(spec);
            CustomModelTarget {
                model,
                account,
                account_condition,
                enabled: true,
                weight: 1,
            }
        })
        .collect()
}

pub(crate) fn parse_route_groups(value: &str) -> Vec<CustomModelRouteGroup> {
    value
        .lines()
        .map(|line| CustomModelRouteGroup {
            targets: line
                .split(',')
                .map(str::trim)
                .filter(|spec| !spec.is_empty())
                .map(|spec| {
                    let (model, account, account_condition) = parse_target_spec(spec);
                    CustomModelTarget {
                        model,
                        account,
                        account_condition,
                        enabled: true,
                        weight: 1,
                    }
                })
                .collect(),
        })
        .filter(|group| !group.targets.is_empty())
        .collect()
}

pub(crate) fn target_label(target: &CustomModelTarget) -> String {
    match target
        .account
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(account)
            if matches!(
                target.account_condition,
                CustomModelAccountCondition::Except
            ) =>
        {
            format!("{}@!{}", target.model, account)
        }
        Some(account) => format!("{}@{}", target.model, account),
        None => target.model.clone(),
    }
}

pub(crate) fn target_count(model: &CustomModel) -> usize {
    model
        .routes
        .iter()
        .flat_map(|group| group.targets.iter())
        .filter(|target| target.enabled)
        .count()
}

pub(crate) fn route_group_count(model: &CustomModel) -> usize {
    model
        .routes
        .iter()
        .filter(|group| group.targets.iter().any(|target| target.enabled))
        .count()
}

fn target(model: &str) -> CustomModelTarget {
    let (model, account, account_condition) = parse_target_spec(model);
    CustomModelTarget {
        model,
        account,
        account_condition,
        enabled: true,
        weight: 1,
    }
}

#[allow(dead_code)]
pub(crate) fn route_group_from_specs(specs: &[&str]) -> CustomModelRouteGroup {
    CustomModelRouteGroup {
        targets: specs.iter().map(|spec| target(spec)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    fn custom_model(alias: &str, routes: Vec<Vec<&str>>) -> CustomModel {
        CustomModel {
            alias: alias.to_string(),
            display_name: None,
            enabled: true,
            load_balance: true,
            routes: routes
                .into_iter()
                .map(|group| CustomModelRouteGroup {
                    targets: group.into_iter().map(target).collect(),
                })
                .collect(),
            primary_models: Vec::new(),
            fallback_models: Vec::new(),
        }
    }

    fn legacy_custom_model(alias: &str, primary: Vec<&str>, fallback: Vec<&str>) -> CustomModel {
        CustomModel {
            alias: alias.to_string(),
            display_name: None,
            enabled: true,
            load_balance: true,
            routes: Vec::new(),
            primary_models: primary.into_iter().map(target).collect(),
            fallback_models: fallback.into_iter().map(target).collect(),
        }
    }

    fn test_config(auth_dir: String) -> Config {
        Config {
            listen: "127.0.0.1:0".to_string(),
            upstream_base: "https://example.test".to_string(),
            proxy_api_key: "test".to_string(),
            tokens: Vec::new(),
            auth_dir: Some(auth_dir),
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

    #[test]
    fn normalizes_public_alias_and_model_lists() {
        assert_eq!(normalize_alias(" ctm:my-model "), "my-model");
        assert_eq!(public_model_id("ctm:my-model"), "ctm:my-model");

        let parsed = parse_model_list(
            "agw:gemini-2.5-pro@agw:email:a@example.com, min:MiniMax-M3\ncop:gpt-5.1",
        );
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].model, "agw:gemini-2.5-pro");
        assert_eq!(
            parsed[0].account.as_deref(),
            Some("agw:email:a@example.com")
        );
        assert_eq!(parsed[1].model, "min:MiniMax-M3");
        assert_eq!(parsed[2].model, "cop:gpt-5.1");

        let groups =
            parse_route_groups("agw:gemini-2.5-pro@one, gem:gemini-2.5-pro@!two\nmin:MiniMax-M3");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].targets.len(), 2);
        assert_eq!(
            groups[0].targets[1].account_condition,
            CustomModelAccountCondition::Except
        );
        assert_eq!(
            target_label(&groups[0].targets[1]),
            "gem:gemini-2.5-pro@!two"
        );
        assert_eq!(groups[1].targets[0].model, "min:MiniMax-M3");
    }

    #[test]
    fn normalizes_full_provider_target_prefixes_without_changing_account_keys() {
        let model = normalize_model(custom_model(
            "demo",
            vec![
                vec![
                    "minimax:MiniMax-M3@minimax:account_id:minimax-1",
                    "claude:claude-sonnet-4-5@claude:organization:org-1",
                ],
                vec![
                    "antigravity:gemini-3-pro@antigravity:email:one@example.com",
                    "github-copilot:gpt-5.1@github-copilot:user:octo",
                    "codex:gpt-5.4@codex:user:gio",
                ],
            ],
        ));

        let first = &model.routes[0].targets[0];
        assert_eq!(first.model, "min:MiniMax-M3");
        assert_eq!(
            first.account.as_deref(),
            Some("minimax:account_id:minimax-1")
        );

        let second = &model.routes[0].targets[1];
        assert_eq!(second.model, "cld:claude-sonnet-4-5");
        assert_eq!(second.account.as_deref(), Some("claude:organization:org-1"));

        assert_eq!(model.routes[1].targets[0].model, "agw:gemini-3-pro");
        assert_eq!(model.routes[1].targets[1].model, "cop:gpt-5.1");
        assert_eq!(model.routes[1].targets[2].model, "cod:gpt-5.4");
        validate_model(&model).expect("full provider prefixes are accepted after normalization");
    }

    #[test]
    fn rejects_invalid_custom_model_targets() {
        let no_routes = custom_model("alias", Vec::new());
        assert!(validate_model(&no_routes).is_err());

        let recursive = custom_model("alias", vec![vec!["ctm:other"]]);
        assert!(validate_model(&recursive).is_err());

        let unsupported = custom_model("alias", vec![vec!["openai:gpt-4"]]);
        assert!(validate_model(&unsupported).is_err());
    }

    #[test]
    fn save_and_load_replace_duplicate_aliases() {
        let unique = format!("io-gateway-custom-model-test-{}", std::process::id());
        let dir = std::env::temp_dir().join(unique);
        let cfg = test_config(dir.to_string_lossy().to_string());

        let first = custom_model("demo", vec![vec!["agw:gemini-2.5-pro"]]);
        let second = custom_model(
            "ctm:demo",
            vec![vec!["min:MiniMax-M3"], vec!["cop:gpt-5.1"]],
        );
        save(&cfg, &[first, second]).expect("save custom models");

        let loaded = load(&cfg);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].alias, "demo");
        assert_eq!(loaded[0].routes[0].targets[0].model, "min:MiniMax-M3");
        assert_eq!(loaded[0].routes[1].targets[0].model, "cop:gpt-5.1");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_primary_and_fallback_models_migrate_to_route_groups() {
        let model = normalize_model(legacy_custom_model(
            "demo",
            vec!["agw:gemini-2.5-pro", "gem:gemini-2.5-pro"],
            vec!["min:MiniMax-M3"],
        ));
        assert_eq!(model.routes.len(), 2);
        assert_eq!(model.routes[0].targets.len(), 2);
        assert_eq!(model.routes[1].targets[0].model, "min:MiniMax-M3");
        assert!(model.primary_models.is_empty());
        assert!(model.fallback_models.is_empty());
    }
}
