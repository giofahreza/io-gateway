# IO Gateway

One local gateway for Codex CLI, Claude Code, and OpenAI-compatible clients. IO Gateway lets you connect multiple AI accounts, expose them through one API, load balance traffic across them, and manage access from a single dashboard.

Website: [gateway.giofahreza.com](https://gateway.giofahreza.com)

Use one base URL. Switch provider by changing the model name.

```text
Codex CLI / Claude Code / OpenAI clients
              |
              v
        IO Gateway
              |
   +----------+----------+----------+
   v          v          v          v
 Codex     Claude     Gemini     Qwen ...
```

## What It Does

IO Gateway turns a pile of separate AI accounts into one operational gateway:

- One API surface for OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages.
- Account-aware routing across Codex, Claude, Gemini, Antigravity, Qwen, DeepSeek, MiniMax, Grok, GitHub Copilot, and GLM.
- Load balancing, priority account usage, and failover across multiple accounts.
- Custom model aliases that can route to one or many provider targets.
- Dashboard-based account, quota, usage, notification, and API-key management.
- Provider/account scoped API keys with per-request estimated-input caps and optional whole-key input-token budgets.

## Feature Highlights

### Unified Client Endpoint

Point clients at IO Gateway and keep your client setup stable:

- `POST /v1/responses`
- `POST /v1/chat/completions`
- `POST /claude/v1/messages`
- `GET /v1/models`

Models can be selected naturally by name, or forced to a provider with a prefix:

| Prefix | Provider |
|---|---|
| `cod:` | Codex / OpenAI via ChatGPT |
| `cld:` | Claude |
| `gem:` | Gemini |
| `agw:` | Antigravity |
| `qwn:` | Qwen |
| `dsk:` | DeepSeek |
| `min:` | MiniMax |
| `grk:` | Grok |
| `cop:` | GitHub Copilot |
| `glm:` | GLM / Z.AI |

Examples: `gpt-5.2`, `cld:claude-sonnet-4`, `gem:gemini-3-pro`, `qwn:qwen3.7-max`, `ctm:workhorse`.

### Custom Models

Create stable `ctm:` model aliases for your own routing strategy.

Custom models can:

- Route one alias to multiple providers.
- Pin a target to a specific account.
- Use every account except a selected account.
- Weight targets for load balancing.
- Fall back to another provider when a target fails.
- Appear in `/v1/models` when enabled and visible through the caller's permitted provider/account targets.

Example use cases:

- `ctm:workhorse` for day-to-day coding across several cheap or high-quota accounts.
- `ctm:fast` for low-latency models.
- `ctm:review` for a strict fallback chain from premium to backup providers.

### Load Balancing And Account Routing

Each provider can hold multiple accounts. IO Gateway tracks request load, quota state, recent failures, cooldowns, and active in-flight requests so traffic is distributed across eligible accounts.

The router is built for agent traffic:

- Avoid disabled and cooling-down accounts.
- Respect upstream quota and retry hints.
- Retry another account before meaningful output reaches the client.
- Track active streams so concurrent prompts spread across accounts.
- Refresh quota snapshots in the background.
- Mark one or more accounts as **Use first** so they are consumed before the normal account pool.
- Remove priority automatically when an account is disabled or deleted.

### API Key Management

Create dashboard-managed API keys without giving dashboard access to every API user.

Keys can be:

- Unrestricted.
- Limited to one provider.
- Limited to multiple providers.
- Limited to selected accounts inside a provider.
- Capped by estimated input tokens in a single request for the whole key, a provider, or a specific account.
- Given an optional cumulative, input-token-only whole-key budget for a lifetime or calendar month.
- Updated or revoked from the dashboard.

Provider and account access is enforced before routing and load balancing. A custom-model request is checked against the provider and account selected for each concrete target; API keys do not have separate model or alias allow-lists.

The per-request cap is a guardrail, not a spending counter. The optional input-token budget is the separate feature for cumulative input-token usage; it does not cap output tokens or monetary cost.

### Dashboard

The dashboard is the control plane for the gateway:

- Add accounts through OAuth, device-code login, browser token extraction, or API key entry.
- See account cards with connection state, quota, package details, latest error details, and usage.
- Enable, disable, delete, refresh, prioritize, and re-auth accounts.
- Switch account layout modes for dense provider views.
- Manage custom models.
- Manage API keys and scoped access.
- Configure notifications.
- Inspect usage charts and context history.

### Notifications

IO Gateway can alert you when accounts or model quotas need attention.

Supported channels:

- Telegram
- Google Chat webhook

Typical alerts:

- Account authentication or upstream errors.
- Model quota becomes fully used.
- Model quota becomes available again.

### Usage And Quota Visibility

The gateway records per-request usage and keeps operational snapshots for the dashboard.

Tracked data includes:

- Requests and errors.
- Input, output, cache, reasoning, and total tokens.
- Last seen, last success, and latest error details.
- Provider-specific quota windows and package data.
- Time-bucketed usage history for charts.

### Codex And Claude Friendly

IO Gateway is designed around real coding-agent workflows:

- Codex CLI can use the OpenAI Responses wire API through the gateway.
- Claude Code can use the Anthropic Messages endpoint through the gateway.
- Tool calls, function responses, image inputs, streaming, and agent loops are translated where providers need adapter logic.

## Supported Providers

| Provider | Prefix | Auth method |
|---|---|---|
| Codex / OpenAI via ChatGPT | `cod:` | OAuth browser redirect |
| Claude | `cld:` | PKCE OAuth |
| Gemini | `gem:` | Google OAuth |
| Antigravity | `agw:` | Google OAuth |
| Qwen | `qwn:` | Browser token extraction |
| DeepSeek | `dsk:` | API key |
| MiniMax | `min:` | API key |
| Grok | `grk:` | OAuth device code |
| GitHub Copilot | `cop:` | GitHub device code |
| GLM / Z.AI | `glm:` | API key |

## Install a Release

Published releases install without Rust, Docker, or administrator access. The installer selects the
native archive for the current machine, verifies its SHA-256 checksum, preserves an existing
configuration and credentials, and guides a new local install through its important choices.

Linux and macOS:

```sh
bash -c 'set -o pipefail; curl -fsSL https://github.com/giofahreza/io-gateway/releases/latest/download/install.sh | sh'
```

The Bash wrapper preserves a failed `curl` exit status instead of treating an empty download as a successful install.
When this is run from a terminal, the installer reads its setup answers from the controlling
terminal (`/dev/tty`), not from the downloaded script stream, so the `curl | sh` form remains safe
to use interactively.

Windows PowerShell:

```powershell
irm https://github.com/giofahreza/io-gateway/releases/latest/download/install.ps1 | iex
```

Release assets are published for Linux x86_64 and ARM64, macOS Intel and Apple Silicon, and
Windows x86_64 and ARM64. Other CPU families and 32-bit systems are not currently supported.
The Linux archives target 64-bit glibc-based distributions; musl-only systems such as Alpine need
to build from source for now. macOS release binaries target Intel macOS 10.13+ and Apple Silicon
macOS 11+.
On a fresh interactive install, the prompts appear in this order: choose the local TCP port
(default `8319`), whether to install the optional `iogw` terminal management client/TUI, whether
to start automatically at sign-in, and whether to start the gateway now. The installer checks that
the selected `127.0.0.1` port is available before creating a new config: interactive setup asks
again for an occupied port, while an explicit or unattended occupied port fails with a clear error.
Autostart controls a persistent user-level systemd service on Linux, a LaunchAgent on macOS, or a
per-user Scheduled Task on Windows; it does not by itself mean the gateway must launch immediately.
The separate **start now** choice launches it after this installation even when autostart is off.
A generated config always listens on `127.0.0.1`; configure administrator authentication before
exposing it to a LAN or the internet. Existing configuration, credentials, and its configured port
are never overwritten on upgrade.

For unattended installs, pass explicit choices instead of relying on prompts. On Linux and macOS,
use `--port 9444`, `--with-iogw` or `--without-iogw`, `--autostart` or `--no-autostart`,
`--start-now` or `--no-start`, and `--non-interactive`. PowerShell accepts the corresponding
`-Port`, `-InstallIogw` / `-NoIogw`, `-AutoStart` / `-NoAutoStart`, `-StartNow` / `-NoStart`, and
`-NonInteractive` options. The cross-platform environment variables `IO_GATEWAY_PORT`,
`IO_GATEWAY_INSTALL_IOGW=auto|yes|no`, `IO_GATEWAY_AUTOSTART=auto|yes|no`,
`IO_GATEWAY_START_NOW=auto|yes|no`, and `IO_GATEWAY_INTERACTIVE=auto|yes|no` support the same
automation. `--start-now` / `-StartNow` requests an immediate launch without changing autostart;
`--no-start` / `-NoStart` skips that launch while retaining an existing autostart service or task.
For example, combine `--autostart --no-start` to enable the next-sign-in service without starting
now, or `--no-autostart --start-now` to run a local background gateway now without a persistent
service. Use `--version vX.Y.Z` (Unix) or `-Version vX.Y.Z` (PowerShell) to install a specific
release.

## Build from Source

```bash
cargo build --release
cp config.example.json config.json
export IO_GATEWAY_KEY="your-shared-proxy-key"
./target/release/io-gateway
```

To keep the configuration outside the current working directory, select it explicitly:

```bash
./target/release/io-gateway --config /path/to/config.json
# or
IO_GATEWAY_CONFIG=/path/to/config.json ./target/release/io-gateway
```

The `--config` flag takes precedence over `IO_GATEWAY_CONFIG`. Relative paths such as
`"auth_dir": "./auths"` are resolved from the selected configuration file's directory.

Dashboard:

```text
http://127.0.0.1:8319/
```

Swagger API docs:

```text
http://127.0.0.1:8319/docs/
```

Terminal management client:

```bash
./target/release/iogw status
./target/release/iogw login
./target/release/iogw
```

`iogw` opens the interactive terminal UI by default. `iogw tui` is the explicit equivalent, and direct commands such as `iogw accounts list`, `iogw quota --refresh`, `iogw keys list`, `iogw models list`, and `iogw usage history` are available for scripts or remote administration.

## Example Client Config

Codex CLI:

```toml
[model_providers.io_gateway]
name = "IO Gateway"
base_url = "http://127.0.0.1:8319"
env_key = "IO_GATEWAY_KEY"
wire_api = "responses"
requires_openai_auth = false

[profiles.gateway]
model = "ctm:workhorse"
model_provider = "io_gateway"
```

Claude Code:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8319/claude",
    "ANTHROPIC_AUTH_TOKEN": "<IO_GATEWAY_KEY>",
    "ANTHROPIC_MODEL": "cld:claude-sonnet-4"
  }
}
```

## Documentation

- [Docs Index](docs/README.md) - detailed documentation entry point.
- [Operator Guide](docs/operator-guide.md) - full setup, configuration, provider login, routing, API, deployment, testing, and troubleshooting docs.
- Runtime Swagger UI - available from a running gateway at `/docs/`.
- OpenAPI JSON - available from a running gateway at `/api-docs/openapi.json`.

## Build And Test

```bash
cargo fmt --check
cargo test --locked
cargo build --release --locked
```

Dashboard JavaScript syntax check:

```bash
awk '/<script>/{flag=1;next} /<\/script>/{flag=0} flag' src/main.rs > /tmp/io-gateway-dashboard.js
node --check /tmp/io-gateway-dashboard.js
```
