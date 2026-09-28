# IO Gateway Operator Guide

This is the detailed setup and operations guide for IO Gateway. The root [README](../README.md) is the showcase overview; this guide keeps the full configuration, routing, provider setup, API, deployment, and troubleshooting reference.

Local multi-provider AI gateway that rotates multiple accounts across ten AI providers behind one shared API key. Supports the OpenAI Responses API, OpenAI Chat Completions API, and Anthropic Messages API — so Codex CLI, Claude Code, and any OpenAI-compatible client can all point at the same gateway and simply switch providers by changing the `model` field.

## Supported Providers

| Provider | Prefix | Auth method |
|---|---|---|
| Codex / OpenAI (via ChatGPT) | `cod:` | OAuth (browser redirect) |
| Antigravity (Google Cloud) | `agw:` | OAuth (Google) |
| Gemini (Google AI Studio) | `gem:` | OAuth (Google) |
| Qwen (Alibaba) | `qwn:` | Browser token extraction |
| DeepSeek | `dsk:` | API key |
| MiniMax | `min:` | API key |
| Grok (xAI) | `grk:` | OAuth device code |
| GitHub Copilot | `cop:` | GitHub device code |
| Claude (Anthropic) | `cld:` | PKCE OAuth |
| GLM / Z.AI | `glm:` | API key |

---

## Who Is This For

Anyone who runs multiple AI accounts and wants to:

- Share load evenly across accounts without changing client config.
- Expose a single endpoint that clients can call with any supported model.
- Manage all accounts from a single dashboard without touching credential files manually.
- Receive Telegram or Google Chat alerts when an account hits an error or exhausts its quota.

---

## Quick Start

### Install a published release

Use a published GitHub Release when you want a native binary without installing Rust. The
installer selects the right archive, verifies it against the release `SHA256SUMS`, preserves an
existing config and credential directory, and creates a new localhost-only config only when one
does not exist.

Linux and macOS:

```sh
bash -c 'set -o pipefail; curl -fsSL https://github.com/giofahreza/io-gateway/releases/latest/download/install.sh | sh'
```

The Bash wrapper preserves a failed `curl` exit status instead of treating an empty download as a successful install.
For an interactive terminal install, the script reads its answers from `/dev/tty` rather than its
standard input, so its questions do not consume the script being downloaded through `curl | sh`.

Windows PowerShell:

```powershell
irm https://github.com/giofahreza/io-gateway/releases/latest/download/install.ps1 | iex
```

The installers are user-scoped and do not use `sudo` or require an administrator account. They
publish native downloads for Linux x86_64 and ARM64, macOS Intel and Apple Silicon, and Windows
x86_64 and ARM64. They do not support 32-bit or other CPU families. Linux archives require a
64-bit glibc-based distribution (musl-only systems such as Alpine should build from source).
macOS binaries target Intel macOS 10.13+ and Apple Silicon macOS 11+.

#### First-run choices and unattended setup

On a new install run from an interactive terminal, the installer asks for these choices in order
before it writes the first configuration:

| Choice | Default | Result |
|---|---|---|
| Local TCP port | `8319` | The generated config listens on `127.0.0.1:<port>` and its Qwen callback uses the same port. The installer preflights the port; an occupied interactive choice is re-prompted, while an explicit or unattended choice fails. |
| `iogw` management client/TUI | Yes | Installs the optional terminal client next to `io-gateway`. |
| Start at sign-in | Yes | Creates a persistent user service: systemd on Linux, a LaunchAgent on macOS, or a Scheduled Task on Windows. This governs future sign-ins, separately from starting now. |
| Start now | Yes | Launches the gateway after this installation. It can run now even when sign-in autostart is off, or be skipped while autostart remains on. |

The generated configuration always remains localhost-only (`127.0.0.1`); set up `admin_auth`
before changing it to a LAN or public address. During an upgrade, the installer preserves the
existing config, credential directory, and `listen` port rather than prompting or rewriting them.

For a repeatable, noninteractive installation, provide the choices explicitly:

```sh
# Linux or macOS
IO_GATEWAY_PORT=9444 \
IO_GATEWAY_INSTALL_IOGW=no \
IO_GATEWAY_AUTOSTART=no \
IO_GATEWAY_START_NOW=no \
IO_GATEWAY_INTERACTIVE=no \
bash -c 'set -o pipefail; curl -fsSL https://github.com/giofahreza/io-gateway/releases/latest/download/install.sh | sh'
```

Unix flags are `--port <1-65535>`, `--with-iogw` / `--without-iogw`, `--autostart` /
`--no-autostart`, `--start-now` / `--no-start`, and `--interactive` / `--non-interactive`.
PowerShell accepts `-Port`, `-InstallIogw` / `-NoIogw`, `-AutoStart` / `-NoAutoStart`,
`-StartNow` / `-NoStart`, and `-Interactive` / `-NonInteractive` (with equivalent long
POSIX-style spellings). The environment variables `IO_GATEWAY_PORT`,
`IO_GATEWAY_INSTALL_IOGW=auto|yes|no`, `IO_GATEWAY_AUTOSTART=auto|yes|no`,
`IO_GATEWAY_START_NOW=auto|yes|no`, and `IO_GATEWAY_INTERACTIVE=auto|yes|no` work on all three
desktop platforms; explicit command line choices take precedence. `--start-now` / `-StartNow`
means launch immediately without changing the persistent service or task. `--no-start` /
`-NoStart` skips the immediate launch while preserving an existing service or task. For a
next-sign-in-only setup, combine `--autostart --no-start`; for a one-off local background process,
combine `--no-autostart --start-now`.

To choose a specific release, use `--version vX.Y.Z` on Unix or `-Version vX.Y.Z` in PowerShell.

### 1. Build

```bash
cargo build --release
```

### 2. Create config

```bash
cp config.example.json config.json
```

Edit `config.json` — minimum required fields:

```json
{
  "listen": "0.0.0.0:8319",
  "upstream_base": "https://chatgpt.com/backend-api/codex",
  "proxy_api_key": "your-shared-proxy-key",
  "tokens": [],
  "auth_dir": "./auths"
}
```

### 3. Set your shared key

```bash
export IO_GATEWAY_KEY="your-shared-proxy-key"
```

All clients use this single key in the `Authorization: Bearer` header.

### 4. Run

```bash
cargo run --release
# or after building:
./target/release/io-gateway
```

To use a configuration outside the current working directory:

```bash
./target/release/io-gateway --config /path/to/config.json
# or
IO_GATEWAY_CONFIG=/path/to/config.json ./target/release/io-gateway
```

`--config` takes precedence over `IO_GATEWAY_CONFIG`. Relative paths in the config, including
`auth_dir`, are resolved from the directory that contains the selected config file.

Dashboard: `http://127.0.0.1:8319/`

API docs: `http://127.0.0.1:8319/docs/`

---

## CI/CD Release and Deployment

GitHub Actions builds, tests, packages, publishes, and deploys this app when a `v*` release tag
is pushed. A manual run keeps the existing production deployment behavior but does not create a
GitHub Release.

Every release tag publishes these downloadable assets:

| Asset | Platform |
|---|---|
| `io-gateway-<tag>-linux-x86_64.tar.gz` | Linux x86_64 (glibc) |
| `io-gateway-<tag>-linux-aarch64.tar.gz` | Linux ARM64 (glibc) |
| `io-gateway-<tag>-macos-x86_64.tar.gz` | macOS Intel |
| `io-gateway-<tag>-macos-aarch64.tar.gz` | macOS Apple Silicon |
| `io-gateway-<tag>-windows-x86_64.zip` | Windows x86_64 |
| `io-gateway-<tag>-windows-aarch64.zip` | Windows ARM64 |
| `install.sh`, `install.ps1`, `SHA256SUMS` | User installers and checksums |

Required GitHub repository secret:

| Secret | Description |
|---|---|
| `DEPLOY_SSH_KEY` | Private SSH key that can connect to `ubuntu@141.144.197.96` |

The workflow uses the current production layout:

| Setting | Value |
|---|---|
| Server | `ubuntu@141.144.197.96` |
| Staged binary | `/home/ubuntu/io-gateway.<commit>` |
| Live binary | `/opt/io-gateway/io-gateway` |
| Systemd service | `io-gateway.service` |
| Revision metadata | `/opt/io-gateway/revision.json` |
| Health checks | `http://127.0.0.1:8319/health`, `http://127.0.0.1:8319/ready` |

Deployment flow:

1. Run the existing Linux formatting, dashboard syntax, test, and production-build checks when application code changed.
2. Build and package six native release archives, then create or update the GitHub Release with the archives, installers, and `SHA256SUMS`.
3. Upload the existing Linux x86_64 production binary to `/home/ubuntu/io-gateway.<commit>`.
4. Verify the staged binary checksum against the GitHub-built artifact.
5. Back up the current live binary, install the new binary, restart `io-gateway.service`, and verify health/readiness.
6. Verify the live binary checksum and write `/opt/io-gateway/revision.json`.

---

## Configuration

### config.json

By default the gateway reads `./config.json`. Override that location with
`io-gateway --config /path/to/config.json` or `IO_GATEWAY_CONFIG=/path/to/config.json`.
The command-line flag takes precedence. A relative `auth_dir` is relative to this config file,
not the process working directory.

```json
{
  "listen": "0.0.0.0:8319",
  "upstream_base": "https://chatgpt.com/backend-api/codex",
  "proxy_api_key": "your-shared-proxy-key",
  "tokens": [],
  "auth_dir": "./auths",
  "disabled_files": [],
  "request_body_limit_enabled": false,
  "max_request_body_bytes": 16777216,
  "max_concurrent_requests": 128,
  "upstream_connect_timeout_seconds": 10,
  "upstream_read_timeout_seconds": 120,
  "upstream_first_event_timeout_seconds": 45,
  "history_retention_days": 30,
  "history_max_entries": 200000,
  "trusted_proxy": false,
  "admin_auth": {
    "enabled": true,
    "api_key": "your-admin-api-key",
    "totp_secret": "BASE32_SECRET",
    "session_ttl_seconds": 43200,
    "secure_cookies": false
  },
  "oauth": {
    "providers": {
      "qwen": {
        "client_id": "f0304373b74a44d2b584a3fb70ca9e56",
        "client_secret": "",
        "redirect_uri": "http://127.0.0.1:8319/login/qwen/callback",
        "scopes": ["openid", "profile", "email", "model.completion"],
        "authorize_url": "https://chat.qwen.ai/oauth/authorize",
        "token_url": "https://chat.qwen.ai/api/v1/oauth2/token",
        "validate_url": "https://chat.qwen.ai/api/v1/auths/",
        "refresh_url": "https://chat.qwen.ai/api/v1/auths/",
        "session_url": "https://chat.qwen.ai/api/v1/auths/",
        "base_url": "https://portal.qwen.ai/v1"
      },
      "claude": {
        "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        "redirect_uri": "https://platform.claude.com/oauth/code/callback",
        "scopes": ["org:create_api_key", "user:profile", "user:inference"],
        "authorize_url": "https://claude.com/cai/oauth/authorize",
        "token_url": "https://platform.claude.com/v1/oauth/token",
        "base_url": "https://api.anthropic.com"
      }
    }
  }
}
```

**Notes:**

- `tokens` is optional. Tokens are loaded from `auth_dir` credential files when empty.
- `disabled_files` lists credential filenames that should be loaded but kept disabled at startup.
- `admin_auth.api_key` is a separate dashboard credential. When configured, it is required in addition to TOTP; it never falls back to `proxy_api_key`.
- `admin_auth.session_ttl_seconds` defaults to 43200 (12 hours), min 300, max 604800 (7 days).
- Set `admin_auth.secure_cookies` to `true` when the dashboard is served over HTTPS.
- Set `trusted_proxy` only when a configured reverse proxy sanitizes forwarded IP headers.
- `request_body_limit_enabled` controls the gateway's request payload cap. Set it to
  `false` to disable the cap, or `true` to enforce `max_request_body_bytes` (16 MiB by
  default). Omitting the toggle preserves the existing enabled behavior; the example
  config explicitly disables it. Restart the gateway after changing these settings.
- With the cap disabled, request bodies are still buffered in memory. Provider and
  reverse-proxy limits apply independently. Concurrent requests default to 128.
- Upstream connect, per-read/stream-idle, and first SSE event timeouts default to 10, 120, and 45 seconds.
- Usage history is stored in SQLite WAL mode. Existing `gateway-usage-history.jsonl` data is imported once, then bounded by the configured retention and entry limits.
- OAuth provider configs are optional; built-in defaults are used when omitted.

### Environment variable overrides

Admin auth:

```bash
ADMIN_AUTH_ENABLED=true
ADMIN_AUTH_API_KEY=your-admin-key
ADMIN_AUTH_TOTP_SECRET=BASE32_SECRET
ADMIN_AUTH_SESSION_TTL_SECONDS=43200
ADMIN_AUTH_SECURE_COOKIES=true
```

Antigravity / Gemini Google OAuth:

```bash
ANTIGRAVITY_GOOGLE_CLIENT_ID=...
ANTIGRAVITY_GOOGLE_CLIENT_SECRET=...
GEMINI_GOOGLE_CLIENT_ID=...
GEMINI_GOOGLE_CLIENT_SECRET=...
```

Qwen OAuth fields can be overridden individually with `QWEN_OAUTH_*` (e.g. `QWEN_OAUTH_BASE_URL`). Claude OAuth fields with `CLAUDE_OAUTH_*`.

---

## Admin Dashboard

Open `http://127.0.0.1:8319/` to access the dashboard.

When `admin_auth.enabled` is `true`, the dashboard requires a 6-digit TOTP code from Google Authenticator (set up with `totp_secret`). If `admin_auth.api_key` is configured, it also requires that separate admin API key. The client-facing `proxy_api_key` never grants dashboard access.

Sessions are persisted to disk and survive server restarts. Session duration is configurable.

### Dashboard sections

**Overview bar** — live counts: total requests, error rate, number of accounts across all providers, attention items (accounts with recent errors).

**Context Usage chart** — token usage over time (input, output, cache, reasoning tokens). Configurable range: 1 hour, 1 day, 1 week, or custom (up to 720 hours). Configurable bucket size: 1 / 5 / 15 / 30 / 60 minutes. Per-model breakdown toggle. Filterable by account.

**Custom Models** — create and manage `ctm:` alias routes with load balancing across multiple provider targets and fallback chains.

**Provider sections** — per-provider account cards showing:
- Request count, error count, token usage (input / output / cache / reasoning)
- Last seen / last success / last error timestamps
- Live quota from upstream (provider-specific: balance, rolling window, weekly window, model breakdown)
- Priority routing status and the **Use first** / **Remove priority** action
- Enable / disable / delete controls
- Re-auth flow for providers with expiring tokens

**Settings modal** — API-key management, notification channel (Telegram or Google Chat) configuration, and per-account alert subscriptions.

Managed API keys can be unrestricted or limited to one or more providers. Each allowed provider can grant access to every account for that provider or only selected accounts. Keys can also enforce estimated prompt-token limits for the whole key, a provider, or a specific account. Access and prompt-limit rules are enforced before provider dispatch and account load balancing, including requests routed through custom models. Existing and legacy keys remain unrestricted until their access is edited.

Prompt-token limits are request guardrails, not monthly counters. A request is blocked when its estimated input prompt tokens exceed the strictest matching whole-key, provider, or account limit.

**Theme toggle** — dark / light, persisted to localStorage.

**Mobile responsive** — hamburger menu for small screens.

---

## API Surface

All client requests use `Authorization: Bearer <IO_GATEWAY_KEY>`.

### Unified endpoints

| Endpoint | Description |
|---|---|
| `GET /v1/models` | Unified model catalog across all enabled providers |
| `POST /v1/responses` | OpenAI Responses API (primary endpoint) |
| `POST /v1/chat/completions` | OpenAI Chat Completions API |
| `POST /claude/v1/messages` | Anthropic Messages API |
| `POST /claude/messages` | Anthropic Messages API (alternate path) |
| `GET /codex/models` | Codex-path model catalog |
| `POST /codex/responses` | Codex-path Responses API |

### Admin endpoints (session cookie required)

| Endpoint | Description |
|---|---|
| `GET /health` | Health check — returns `ok` |
| `GET /ready` | Readiness check — requires at least one enabled upstream account |
| `GET /dashboard.json` | Full dashboard data JSON |
| `GET /quota.json` | Codex account quota data |
| `POST /codex/rate-limit-reset-credit/consume` | Redeem an available Codex usage-limit reset credit |
| `POST /credentials/delete` | Delete a credential file |
| `POST /credentials/toggle` | Enable or disable a credential |
| `GET /admin/api-keys` | List managed API keys and selectable provider accounts |
| `POST /admin/api-keys/create` | Create an API key with optional provider/account access rules |
| `POST /admin/api-keys/access` | Replace an API key's provider/account access rules |
| `POST /admin/api-keys/revoke` | Revoke an API key |
| `GET /admin/account-routing` | List priority routing settings and selectable accounts |
| `POST /admin/account-routing/priority` | Add or remove an account from provider priority routing |
| `GET/POST /notifications/settings` | Read or update notification settings |
| `POST /notifications/test` | Send a test notification |
| `GET /usage/summary.json` | Aggregate usage summary |
| `GET /usage/history.json` | Time-bucketed usage history |
| `GET /usage/context-history.json` | Per-request context history |
| `GET /custom-models.json` | List all custom model aliases |
| `POST /custom-models/save` | Create or replace a custom model alias |
| `POST /custom-models/delete` | Delete a custom model alias |
| `GET /temp-files/:name` | Download a temp file (images, etc.) |
| `GET /docs/` | Swagger UI |
| `GET /api-docs/openapi.json` | OpenAPI spec |

### Per-provider account / login routes

Each provider exposes:

- `GET /{provider}/accounts.json` — list accounts
- `GET /{provider}/quota.json` — live quota data
- `POST /login/{provider}/start` — initiate login
- `POST /login/{provider}/submit` — complete login

Providers: `codex`, `antigravity`, `gemini`, `qwen`, `deepseek`, `minimax`, `grok`, `copilot`, `claude`, `glm`.

---

## Provider Routing

The gateway picks the upstream provider from the `model` field — no separate per-provider URL prefixes required. Clients stay on `/v1/*` and switch providers by changing only the model name.

### Default routing rules

| Model pattern | Provider |
|---|---|
| `gpt-*` and unmatched names | Codex (OpenAI via ChatGPT) |
| `gemini-3-pro-image`, `gemini-3-pro-high`, `gemini-3-pro-low`, `gemini-2.5-flash-thinking` | Antigravity |
| `gemini-*` (standard) | Gemini (Google AI Studio) |
| `qwen*` | Qwen |
| `deepseek*` | DeepSeek |
| `grok*` | Grok |
| `claude*` | Claude (Anthropic OAuth) |
| `glm*` | GLM / Z.AI |
| `MiniMax-*` (via Claude endpoint) | MiniMax Anthropic bridge |
| `ctm:alias` | Custom model alias |

### Provider prefix overrides

Prefix a model id with a three-letter provider code followed by `:` to force a specific provider regardless of model name. The gateway strips the prefix before forwarding:

| Prefix | Provider |
|---|---|
| `agw:` | Antigravity |
| `gem:` | Gemini |
| `qwn:` | Qwen |
| `dsk:` | DeepSeek |
| `grk:` | Grok |
| `min:` | MiniMax |
| `cop:` | GitHub Copilot |
| `cld:` | Claude |
| `glm:` | GLM / Z.AI |
| `cod:` | Codex / OpenAI |

Examples:

```
agw:gemini-2.5-pro   → Antigravity with upstream model gemini-2.5-pro
gem:gemini-2.5-pro   → native Gemini with upstream model gemini-2.5-pro
cop:gpt-5.1          → GitHub Copilot with upstream model gpt-5.1
cld:claude-sonnet-4  → Claude OAuth with upstream model claude-sonnet-4
```

Prefixed model ids are returned by `GET /v1/models` and should be used in client config and model pickers.

---

## Custom Models

Custom models expose a stable `ctm:alias` that routes requests to one or more real provider targets with optional load balancing and fallback chains. Stored in `custom-models.json` under `auth_dir`; no database required.

### Create / replace an alias

```bash
curl -sS http://127.0.0.1:8319/custom-models/save \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  --data '{
    "alias": "workhorse",
    "display_name": "Workhorse",
    "enabled": true,
    "load_balance": true,
    "routes": [
      {
        "targets": [
          {"model": "agw:gemini-2.5-pro", "weight": 2},
          {
            "model": "gem:gemini-2.5-pro",
            "account": "gemini:email:slow@example.com",
            "account_condition": "except"
          }
        ]
      },
      {
        "targets": [
          {"model": "min:MiniMax-M3"},
          {"model": "dsk:deepseek-v4-pro"}
        ]
      }
    ]
  }'
```

For a custom model target, omit `account` to use any enabled account for that provider. Set `account` with the default `account_condition` of `only` to pin the target to one account, or set `"account_condition": "except"` to use all enabled accounts except the selected account.

### Use the alias

```bash
curl -sS http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  --data '{"model":"ctm:workhorse","input":"Reply with OK only."}'
```

### Delete an alias

```bash
curl -sS http://127.0.0.1:8319/custom-models/delete \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  --data '{"alias":"workhorse"}'
```

### Rules

- `alias` is normalized: `workhorse` and `ctm:workhorse` refer to the same alias.
- Aliases must not be empty and must not contain whitespace, `:`, `/`, or `\`.
- A custom model must have at least one enabled primary target.
- `load_balance: true` — enabled primary targets are sorted by provider/account usage score and rotated on ties. A higher `weight` biases selection toward that target.
- `load_balance: false` — primary targets are tried in configured order.
- Fallback targets are tried in configured order after all primary targets.
- Fallback triggers on HTTP `400` or higher from the selected target.
- If every target fails, the response is `502` with the failed target list in the error body.
- Disabled custom models are hidden from `/v1/models` and return `503` when called directly.
- To rename an alias, send `original_alias` (or `previous_alias`) alongside the new alias; the old alias is deleted after the new one is saved.
- Custom model aliases appear in `GET /v1/models`, `GET /codex/models`.
- Targets cannot point at another `ctm:` alias (no recursive routing).

---

## Account Management

### Round-robin rotation

Each provider maintains its own round-robin counter. Accounts are rotated per-request. Disabled accounts are skipped. Custom models use usage-weighted selection when `load_balance` is enabled.

### Priority account routing

Use priority routing when one or more accounts should be consumed before the normal account pool. In the dashboard, open an account's three-dot menu and choose **Use first**. Priority accounts for the same provider are selected before non-priority accounts until they are disabled, cooling down, unavailable, or quota-exhausted. If multiple accounts are marked priority, the router balances within that priority set.

Disable or delete an account to automatically remove its priority setting. Re-enabling the account does not silently restore priority; use **Use first** again when you want it back.

Admin API:

```bash
curl -sS http://127.0.0.1:8319/admin/account-routing \
  -b "$ADMIN_COOKIE"

curl -sS http://127.0.0.1:8319/admin/account-routing/priority \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"provider":"codex","account":"codex:label:manual-2","priority":true}'
```

### Enable / disable accounts

Toggle an account without deleting it from the dashboard or via:

```bash
curl -sS http://127.0.0.1:8319/credentials/toggle \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -d "file=my-account.json&enabled=false"
```

### Delete accounts

```bash
curl -sS http://127.0.0.1:8319/credentials/delete \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -d "file=my-account.json"
```

Deleting removes the credential file from disk and unloads it from memory immediately.

---

## Provider Setup

### Codex (OpenAI via ChatGPT)

OAuth browser redirect flow. From the dashboard, click **+ Add account → Codex (ChatGPT)**:

1. Click **Start Login** — opens a new tab for ChatGPT OAuth.
2. Complete login in the browser.
3. Copy the callback URL (the browser will fail to load it — that's expected).
4. Paste the callback URL back into the dashboard form and click **Submit**.
5. The credential is saved to `auth_dir` and loaded immediately.

Or via API:

```bash
# Start
curl http://127.0.0.1:8319/login/codex/start -b "$ADMIN_COOKIE"

# Submit callback URL
curl http://127.0.0.1:8319/login/codex/submit \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode "redirect_url=https://..."
```

`GET /quota.json` includes Codex reset-credit availability when ChatGPT returns it:

```json
{
  "rate_limit_reset_credits": {
    "available_count": 1,
    "credits": [
      {
        "id": "credit_...",
        "reset_type": "codex_rate_limits",
        "status": "available",
        "granted_at": "2026-06-17T00:00:00Z",
        "expires_at": "2026-07-17T00:00:00Z"
      }
    ]
  }
}
```

Redeem one reset credit for a saved Codex credential:

```bash
curl -sS -X POST 'http://127.0.0.1:8319/codex/rate-limit-reset-credit/consume' \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -d "file_name=my-codex-account.json" \
  -d "credit_id=credit_..." \
  -d "idempotency_key=$(uuidgen)"
```

`credit_id` is optional; when omitted, ChatGPT selects the next available reset credit. A reset is only possible when the account has an available earned reset credit and a current Codex rate-limit window is eligible for reset.

#### Codex CLI config

```toml
[model_providers.io_gateway]
name = "Local IO Gateway"
base_url = "http://127.0.0.1:8319"
env_key = "IO_GATEWAY_KEY"
wire_api = "responses"
requires_openai_auth = false

[profiles.io_gateway]
model = "gpt-5.2-codex"
model_provider = "io_gateway"
```

---

### Antigravity (Google Cloud)

OAuth flow using Google credentials. From the dashboard, click **+ Add account → Antigravity (Google)**:

1. Click **Start Login** — opens Google OAuth.
2. Complete login and grant permissions.
3. The credential is saved automatically via the callback.

Or via API:

```bash
curl http://127.0.0.1:8319/login/antigravity/start -b "$ADMIN_COOKIE"
```

Antigravity models include advanced Gemini variants and image generation:

- `agw:gemini-2.5-pro`, `agw:gemini-2.5-flash`, `agw:gemini-3-pro`
- `agw:gemini-3-pro-image`, `agw:gemini-3-pro-high`, `agw:gemini-3-pro-low`
- `agw:gemini-2.5-flash-thinking`
- `agw:claude-sonnet-4-6`, `agw:claude-opus-4-6-thinking`

---

### Gemini (Google Code Assist)

OAuth flow using the official Gemini Code Assist Google client. From the dashboard, click **+ Add account → Gemini (Google)**. Individual Google accounts should leave Project ID empty so Google can provision or return its managed Code Assist project. Workspace, organization, and Gemini Code Assist subscription accounts must provide a project that already has the required Gemini API and IAM access.

Standard Gemini models: `gem:gemini-2.5-pro`, `gem:gemini-2.5-flash`, `gem:gemini-2.5-flash-lite`, `gem:gemini-3-pro`.

---

### Qwen

Browser token extraction flow (no standard OAuth redirect):

1. From the dashboard, click **+ Add account → Qwen** (or open `http://127.0.0.1:8319/login/qwen/start`).
2. The helper page shows instructions: open `https://chat.qwen.ai`, sign in, run the token extractor snippet against `localStorage.token`.
3. Paste the extracted token back into the dashboard form or send it to:

```bash
curl -sS http://127.0.0.1:8319/login/qwen/start \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"token":"BROWSER_TOKEN","label":"personal"}'
```

The gateway validates and saves the credential. Tokens refresh automatically via the upstream session endpoint.

**Note:** The API base is normalized to `https://portal.qwen.ai/v1`. Older credentials pointing at `https://chat.qwen.ai/api/v1` are remapped at runtime.

```bash
# Check registered accounts
curl http://127.0.0.1:8319/qwen/accounts.json -b "$ADMIN_COOKIE"
```

#### Codex CLI config

```toml
[model_providers.qwen_via_gateway]
name = "Qwen via IO Gateway"
base_url = "http://127.0.0.1:8319/v1"
experimental_bearer_token = "<IO_GATEWAY_KEY>"
wire_api = "responses"

[profiles.qwen]
model = "qwn:qwen3.7-max"
model_provider = "qwen_via_gateway"
```

---

### DeepSeek

API key flow. From the dashboard, click **+ Add account → DeepSeek**, or:

```bash
curl -sS http://127.0.0.1:8319/login/deepseek/start \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"api_key":"sk-...","label":"personal","base_url":"https://api.deepseek.com"}'
```

The gateway validates the key against `GET /models` before saving. Balance is pulled from `GET /user/balance` (cached 60 s) and shown on each account card.

Tool calls and reasoning summaries are translated to/from DeepSeek's chat-completions format transparently.

```bash
curl http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-v4-pro","input":"Reply with one short line."}'
```

#### Codex CLI config

```toml
[model_providers.deepseek_via_gateway]
name = "DeepSeek via IO Gateway"
base_url = "http://127.0.0.1:8319/v1"
experimental_bearer_token = "<IO_GATEWAY_KEY>"
wire_api = "responses"

[profiles.deepseek]
model = "dsk:deepseek-v4-pro"
model_provider = "deepseek_via_gateway"
```

---

### MiniMax

API key flow. From the dashboard, click **+ Add account → MiniMax**, or:

```bash
curl -sS http://127.0.0.1:8319/login/minimax/start \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"api_key":"eyJ...","label":"personal"}'
```

Routing:

- `POST /v1/responses` → MiniMax native `/v1/responses` (Codex Responses body forwarded as-is).
- `POST /claude/messages` with a `MiniMax-*` model → MiniMax Anthropic-compatible `/anthropic/v1/messages`.

Dashboard shows 5-hour and weekly rolling quota windows with per-model breakdown, pulled from MiniMax's coding plan API.

```bash
curl http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"MiniMax-M3","input":"say hi in one word"}'
```

#### Image and video generation

The OpenAI-style media routes select MiniMax from the model prefix and retain
the provider's native request semantics:

| Gateway route | MiniMax models | Result |
|---|---|---|
| `POST /v1/images/generations` | `min:image-01`, `min:image-01-live` | Synchronous image response with `data[].url` or `data[].b64_json` |
| `POST /v1/videos/generations` | `min:MiniMax-H3`, `min:MiniMax-H3-Max`, or legacy Hailuo models | Async task response with `id` and `status_url` |
| `GET /v1/videos/{task_id}` | N/A | Normalized task state and `url` after completion |

`image-01-live` supports the provider's `subject_reference` array for
character-reference image generation. H3/H3 Max use MiniMax's V2 video API and
require Pay-as-you-go access: H3 supports 768P/2K and reference media, while
H3 Max is the faster 480P/768P text or first/last-frame variant. The gateway
also supports the legacy V1 Hailuo models (`MiniMax-Hailuo-2.3`,
`MiniMax-Hailuo-2.3-Fast`, `MiniMax-Hailuo-02`, `T2V-01*`, `I2V-01*`, and
`S2V-01`) for accounts that retain that entitlement.

For the legacy models, `MiniMax-Hailuo-2.3` and
`MiniMax-Hailuo-2.3-Fast` support `768P` (default) or `1080P` at 6 seconds;
10-second output is `768P` only. `MiniMax-Hailuo-02` accepts `512P` only for
first-frame image-to-video, while text-to-video and first/last-frame requests
use `768P` or `1080P` (again, `768P` only at 10 seconds).

```bash
# Generate one image. Use response_format=b64_json to receive base64 instead.
curl -sS http://127.0.0.1:8319/v1/images/generations \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "min:image-01",
    "prompt": "A small red ceramic cup on a white table",
    "size": "1024x1024",
    "response_format": "url"
  }' | jq

# Submit a legacy Hailuo video task, then poll until status is completed or failed.
# Use min:MiniMax-H3 with duration 4-15 and resolution 768P/2K on a Pay-as-you-go key.
task_id=$(curl -sS http://127.0.0.1:8319/v1/videos/generations \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "min:MiniMax-Hailuo-2.3",
    "prompt": "A small red ceramic cup slowly rotates on a white table",
    "duration": 6,
    "resolution": "768P"
  }' | jq -r '.id')

curl -sS "http://127.0.0.1:8319/v1/videos/$task_id" \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" | jq
```

#### Codex CLI config

```toml
[model_providers.minimax_via_gateway]
name = "MiniMax via IO Gateway"
base_url = "http://127.0.0.1:8319/v1"
experimental_bearer_token = "<IO_GATEWAY_KEY>"
wire_api = "responses"

[profiles.minimax]
model = "MiniMax-M3"
model_provider = "minimax_via_gateway"
model_context_window = 1000000
model_reasoning_effort = "high"
```

#### Claude Code config

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8319/claude",
    "ANTHROPIC_AUTH_TOKEN": "<IO_GATEWAY_KEY>",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1000000",
    "ANTHROPIC_MODEL": "MiniMax-M3",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "MiniMax-M3",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "MiniMax-M3",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "MiniMax-M3"
  }
}
```

---

### Grok (xAI)

OAuth device code flow. From the dashboard, click **+ Add account → Grok (xAI)**, or:

```bash
# Step 1: start device code flow
curl -sS http://127.0.0.1:8319/login/grok/start \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"label":"personal"}'
```

Open the returned verification URL, enter the user code, then submit:

```bash
# Step 2: poll for completion (the dashboard does this automatically)
curl http://127.0.0.1:8319/login/grok/status?state=STATE \
  -b "$ADMIN_COOKIE"
```

Grok tokens expire periodically. Re-auth from the dashboard when the account shows auth errors.

#### Codex CLI config

```toml
[model_providers.grok_via_gateway]
name = "Grok via IO Gateway"
base_url = "http://127.0.0.1:8319/v1"
experimental_bearer_token = "<IO_GATEWAY_KEY>"
wire_api = "responses"

[profiles.grok]
model = "grk:grok-4.3"
model_provider = "grok_via_gateway"
```

---

### GitHub Copilot

GitHub device code flow. From the dashboard, click **+ Add account → GitHub Copilot**, or:

```bash
# Step 1: start device code flow
curl -X POST http://127.0.0.1:8319/login/copilot/start \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"account_type":"individual"}'
```

Open the returned GitHub verification URL, enter the user code, then submit the `device_code`:

```bash
# Step 2: submit device_code after GitHub confirms
curl -X POST http://127.0.0.1:8319/login/copilot/submit \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -d 'device_code=DEVICE_CODE'
```

The `cop:` prefix is required so Copilot GPT model names do not collide with the Codex target.

Routing:

- `POST /v1/responses` → Copilot `/responses`.
- `POST /claude/messages` → Copilot Responses bridge (translates Claude Messages → Copilot Responses).

```bash
curl http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"cop:gpt-5.1","input":"say hi in one word"}'
```

#### Codex CLI config

```toml
[model_providers.copilot_via_gateway]
name = "GitHub Copilot via IO Gateway"
base_url = "http://127.0.0.1:8319/v1"
experimental_bearer_token = "<IO_GATEWAY_KEY>"
wire_api = "responses"

[profiles.copilot]
model = "cop:gpt-5.1"
model_provider = "copilot_via_gateway"
model_reasoning_effort = "high"
```

#### Claude Code config

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8319/claude",
    "ANTHROPIC_AUTH_TOKEN": "<IO_GATEWAY_KEY>",
    "ANTHROPIC_MODEL": "cop:claude-sonnet-4",
    "ANTHROPIC_SMALL_FAST_MODEL": "cop:claude-sonnet-4"
  }
}
```

---

### Claude (Anthropic)

PKCE OAuth flow. From the dashboard, click **+ Add account → Claude**:

1. Click **Start Login** — opens Anthropic OAuth.
2. Complete login in the browser.
3. Copy the displayed `CODE#STATE` value or the callback URL.
4. Paste it into the dashboard form and click **Submit**.

Or via API:

```bash
# Step 1: get authorization URL
curl -sS 'http://127.0.0.1:8319/login/claude/start?label=personal' \
  -b "$ADMIN_COOKIE"

# Step 2: submit code or callback URL after OAuth completes
curl -sS http://127.0.0.1:8319/login/claude/submit \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode 'redirect_url=CODE#STATE'
```

Cookie fallback (when browser OAuth is unavailable):

```bash
curl -sS http://127.0.0.1:8319/login/claude/start \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"cookie":"CLAUDE_AI_COOKIE","label":"personal","organization_uuid":"..."}'
```

Direct token fallback:

```bash
curl -sS http://127.0.0.1:8319/login/claude/start \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"access_token":"...","refresh_token":"...","label":"personal"}'
```

Access tokens are refreshed automatically when they expire.

```bash
curl http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"cld:claude-sonnet-4-20250514","input":"say hi in one word"}'
```

#### Codex CLI config

```toml
[model_providers.claude_via_gateway]
name = "Claude via IO Gateway"
base_url = "http://127.0.0.1:8319/v1"
experimental_bearer_token = "<IO_GATEWAY_KEY>"
wire_api = "responses"

[profiles.claude]
model = "cld:claude-sonnet-4-20250514"
model_provider = "claude_via_gateway"
model_reasoning_effort = "high"
```

#### Claude Code config

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8319/claude",
    "ANTHROPIC_AUTH_TOKEN": "<IO_GATEWAY_KEY>",
    "ANTHROPIC_MODEL": "cld:claude-sonnet-4-20250514",
    "ANTHROPIC_SMALL_FAST_MODEL": "cld:claude-3-5-haiku-20241022"
  }
}
```

---

### GLM / Z.AI

API key flow with two account types:

- `api_usage` — standard Z.AI API key. OpenAI/Codex requests use `https://api.z.ai/api/paas/v4`; Claude Messages requests are translated through Chat Completions.
- `subscription` — GLM Coding Plan subscription key. OpenAI/Codex requests use `https://api.z.ai/api/coding/paas/v4`; Claude Messages pass through to `https://api.z.ai/api/anthropic`.

```bash
curl -sS http://127.0.0.1:8319/login/glm/start \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{"api_key":"ZAI_API_KEY","label":"personal","account_type":"api_usage"}'
```

Routing:

- `POST /v1/responses` → GLM's OpenAI-compatible `/chat/completions` (translated).
- `POST /v1/chat/completions` → GLM `/chat/completions` (passthrough).
- `POST /claude/messages` → depends on account type (translate or passthrough).

```bash
# Via Responses API
curl http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"glm:glm-5.2","input":"say hi in one word"}'

# Via native Chat Completions
curl http://127.0.0.1:8319/v1/chat/completions \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"glm:glm-5.2","messages":[{"role":"user","content":"say hi"}]}'
```

#### Codex CLI config

```toml
[model_providers.glm_via_gateway]
name = "GLM via IO Gateway"
base_url = "http://127.0.0.1:8319/v1"
experimental_bearer_token = "<IO_GATEWAY_KEY>"
wire_api = "responses"

[profiles.glm]
model = "glm:glm-5.2"
model_provider = "glm_via_gateway"
model_reasoning_effort = "high"
```

#### Claude Code config

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8319/claude",
    "ANTHROPIC_AUTH_TOKEN": "<IO_GATEWAY_KEY>",
    "ANTHROPIC_MODEL": "glm:glm-5.2",
    "ANTHROPIC_SMALL_FAST_MODEL": "glm:glm-5.2"
  }
}
```

---

## Image Input

The gateway forwards image parts from Responses API requests to every upstream target. Clients can attach images using either of these content-part shapes:

```json
{"type": "input_image", "image_url": "data:image/png;base64,..."}
```

```json
{"type": "image_url", "image_url": {"url": "https://example.com/x.png"}}
```

Per-target translation:

| Target | Translation |
|---|---|
| Codex | Passthrough — native Responses API |
| Grok | Passthrough — native Responses API |
| MiniMax / Qwen | OpenAI Chat Completions content array |
| DeepSeek | Anthropic Messages content array |
| Gemini / Antigravity | Google Generative `parts` array |
| Claude | Anthropic Messages content array |

Text-only requests are kept as a plain string for backward compatibility.

### Image generation

Generate an image through Antigravity and save the PNG:

```bash
tmp=$(mktemp)
curl -sS -N http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  --data '{
    "model": "gemini-3-pro-image",
    "input": "Create a flat orange cat icon on a white background.",
    "tools": [{"type": "image_generation"}],
    "stream": true
  }' > "$tmp"

sed -n 's/^data: //p' "$tmp" \
  | jq -r 'select(.type=="response.image_generation_call.partial_image") | .partial_image_b64' \
  | tail -n 1 \
  | base64 -d > /tmp/image.png
```

---

## Tool Calls / Agentic Loop

The gateway translates `tools` arrays and `tool_calls` / `functionCall` responses between OpenAI Responses format and each provider's native format. Codex CLI multi-step agentic loops (shell tool, repeated tool call turns) work end-to-end across all providers.

| Provider | Agentic loop |
|---|---|
| Codex (gpt-5.5, gpt-5.4, gpt-5.4-mini, codex-auto-review) | Native |
| MiniMax-M3 | Via native Responses and Anthropic endpoints |
| DeepSeek (deepseek-v4-pro / flash) | Via Anthropic Messages bridge |
| Gemini (gemini-2.5-pro / flash / flash-lite, gemini-3-pro) | Tools forwarded, functionCall translated |
| Antigravity (all models) | Same as Gemini |
| Qwen (qwen3-coder-plus, qwen3.5-plus, qwen3.6-plus, qwen3.7-plus, qwen3.7-max) | Tools forwarded to Chat Completions, tool_calls translated back |
| GitHub Copilot | Via Copilot Responses endpoint |
| Claude | Native Anthropic Messages |

---

## Notifications

The gateway can send alerts when an account hits an error or a model crosses a quota threshold. Configure from the dashboard **Settings** modal or via API.

Supported channels: **Telegram** and **Google Chat** (webhook).

### Configure via API

```bash
# Read current settings
curl http://127.0.0.1:8319/notifications/settings -b "$ADMIN_COOKIE"

# Update Telegram settings
curl -sS http://127.0.0.1:8319/notifications/settings \
  -b "$ADMIN_COOKIE" \
  -H "Content-Type: application/json" \
  -d '{
    "enabled": true,
    "channel": "telegram",
    "telegram": {
      "bot_token": "123456:ABC-DEF...",
      "chat_id": "-1001234567890"
    },
    "watched_accounts": ["account-key-1", "account-key-2"]
  }'

# Send a test notification
curl -sS -X POST http://127.0.0.1:8319/notifications/test -b "$ADMIN_COOKIE"
```

Alert types:

- **Account error** — fired when a watched account returns an upstream error.
- **Model quota transition** — fired when a model moves between fully-used and available states.

---

## Usage Tracking

The gateway records per-request usage metrics to disk and keeps them in memory across all providers.

Per-account stats: requests, errors, input tokens, output tokens, total tokens, cache tokens, reasoning tokens, first seen, last seen, last success, last error.

Stats survive server restarts (written to `usage-stats.json` under `auth_dir`).

### Usage history chart

The dashboard chart shows time-bucketed history for the configured range. The same data is available via API:

```bash
# Aggregate history, last 24 hours, 15-minute buckets
curl 'http://127.0.0.1:8319/usage/history.json?hours=24&bucket_minutes=15' \
  -b "$ADMIN_COOKIE"

# Per-model breakdown
curl 'http://127.0.0.1:8319/usage/context-history.json?hours=24&bucket_minutes=15&per_model=true' \
  -b "$ADMIN_COOKIE"

# Filter by account
curl 'http://127.0.0.1:8319/usage/context-history.json?hours=24&account=ACCOUNT_KEY' \
  -b "$ADMIN_COOKIE"
```

---

## Quick API Tests

List all available models (across all enabled providers):

```bash
curl http://127.0.0.1:8319/v1/models \
  -H "Authorization: Bearer $IO_GATEWAY_KEY"
```

Basic text request (Codex):

```bash
curl http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-5.2","input":"Reply with one line."}'
```

Streaming:

```bash
curl http://127.0.0.1:8319/v1/responses \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"model":"MiniMax-M3","input":"say hi","stream":true}'
```

Via Claude Code endpoint:

```bash
curl http://127.0.0.1:8319/claude/v1/messages \
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model":"cld:claude-sonnet-4-20250514","max_tokens":256,"messages":[{"role":"user","content":"say hi"}]}'
```

---

## Process Management (PM2)

The repo includes `ecosystem.config.cjs` for PM2:

```bash
pm2 start ecosystem.config.cjs
pm2 save
pm2 startup
```

Set `ANTIGRAVITY_GOOGLE_CLIENT_ID`, `ANTIGRAVITY_GOOGLE_CLIENT_SECRET`, `GEMINI_GOOGLE_CLIENT_ID`, and `GEMINI_GOOGLE_CLIENT_SECRET` in the shell environment before starting PM2 so they are forwarded to the gateway process.

---

## Files

| Path | Description |
|---|---|
| `config.json` | Runtime config (create from `config.example.json`) |
| `auths/` | Credential JSON files, one per account |
| `auths/custom-models.json` | Custom model alias definitions |
| `auths/usage-stats.json` | Persisted per-account usage counters |
| `auths/usage-history.json` | Per-request token history for charts |
| `auths/notification-settings.json` | Notification channel and watched accounts |
| `auths/admin-sessions.json` | Active admin sessions (survives restarts) |
| `auths/api-keys.json` | Hashed API keys, metadata, and provider/account access rules |
| `auths/account-routing.json` | Priority account routing settings |
| `ecosystem.config.cjs` | PM2 process config |
| `src/main.rs` | Main gateway source (~12k lines, includes embedded dashboard HTML/JS/CSS) |

---

## Building and Testing

```bash
# Full build
cargo build

# Release build
cargo build --release

# Run tests (216 tests)
cargo test --bin io-gateway

# Check JS syntax in embedded dashboard
awk '/<script>/{flag=1;next} /<\/script>/{flag=0} flag' src/main.rs > /tmp/d.js
node --check /tmp/d.js
```

Pre-existing warnings in `src/target/qwen/auth.rs` and similar files are unrelated to the UI and can be ignored.

---

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `403 cloudflare` | Missing upstream headers or wrong `upstream_base` |
| `Instructions are required` | Request body is too minimal; Codex CLI sends proper instructions automatically |
| `502 Bad Gateway` | Port collision, gateway not running, or all upstream accounts failed |
| `503 No upstream credentials configured` | No accounts loaded for the requested provider |
| Account shows auth errors after a while | Token expired — re-auth from the dashboard (common for Grok, Copilot) |
| Qwen model returns empty | Ensure `base_url` is normalized to `https://portal.qwen.ai/v1` |
| Model not found in `/v1/models` | Provider has no enabled accounts; add and enable at least one account |
