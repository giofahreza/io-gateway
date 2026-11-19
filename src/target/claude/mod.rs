pub mod accounts;
pub mod admin;
pub mod api;
pub mod auth;
pub mod quota;
pub mod reset_automation;
pub mod resets;

pub const PROVIDER_NAME: &str = "claude";
pub const DEFAULT_API_BASE_URL: &str = "https://api.anthropic.com";
pub const CLAUDE_CODE_CLI_USER_AGENT: &str = "claude-cli/2.1.328 (external, cli)";
