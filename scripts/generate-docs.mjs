import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const outputDir = "site/docs";
const docsVersion = "v0.1.11+";
const updated = "September 4, 2026";
const assetVersion = "20260910a";

const groups = [
  { title: "Tutorials", slug: "tutorials", description: "First-run paths that take an operator from zero to a working gateway." },
  { title: "How-to guides", slug: "how-to-guides", description: "Task-oriented procedures for dashboard and routing work." },
  { title: "Reference", slug: "reference", description: "Stable behavior, fields, limits, storage, and endpoint details." },
  { title: "Explanation", slug: "explanation", description: "Concepts and tradeoffs behind IO Gateway routing." },
  { title: "Operations", slug: "operations", description: "Release, deployment, and production operating procedures." },
  { title: "Troubleshooting", slug: "troubleshooting", description: "Symptom-led diagnosis and recovery checks." },
];

const docsFigureDimensions = {
  "docs-api-key-limits.png": [720, 760],
  "docs-custom-models.png": [1420, 343],
  "docs-dashboard-overview.png": [1220, 860],
  "docs-priority-routing.png": [414, 302],
  "docs-provider-accounts.png": [947, 366],
  "docs-test-api.png": [720, 741],
  "docs-usage-quota.png": [1412, 367],
};

const pages = [
  {
    slug: "quick-start",
    title: "Quick start",
    group: "Tutorials",
    type: "Tutorial",
    appliesTo: "New local, desktop, and server installs",
    introduced: docsVersion,
    updated: "September 2, 2026",
    categories: ["Tutorials", "Configuration"],
    keywords: "setup install local run build config dashboard test api health ready first run",
    summary: "Build, configure, run, and smoke-test IO Gateway for the first time.",
    seeAlso: ["configuration", "provider-accounts", "test-api"],
    body: `
      <p class="docs-lead">Install a published IO Gateway release or build it from source, then open the operator dashboard, add upstream credentials, and verify that a model request reaches a provider account.</p>
      ${docsFigure("docs-dashboard-overview.png", "IO Gateway dashboard populated with usage totals, context chart, custom routes, and provider account cards.", "The dashboard is the first place to verify that the gateway sees accounts, quota, and routes.")}
      <h2>Choose an install path</h2>
      <ul class="docs-list">
        <li><strong>Published release</strong> Recommended for a desktop or server. It does not require Rust, Docker, or administrator access.</li>
        <li><strong>Build from source</strong> Use this when developing IO Gateway or when your operating system is not covered by a published release.</li>
        <li><strong>Provider credential</strong> Prepare at least one upstream account before testing real model calls.</li>
      </ul>
      <h2>Install a published release</h2>
      <p>The release installers select the native archive for the current computer, verify the downloaded archive against the release <code>SHA256SUMS</code> file, and keep an existing configuration and credential directory intact during upgrades.</p>
      <p><strong>Linux and macOS</strong></p>
      <pre><code>bash -c 'set -o pipefail; curl -fsSL https://github.com/giofahreza/io-gateway/releases/latest/download/install.sh | sh'</code></pre>
      <p>The Bash wrapper preserves a failed <code>curl</code> exit status instead of treating an empty download as a successful install. In an interactive terminal, the Unix installer reads setup answers from <code>/dev/tty</code>, not from the downloaded script stream, so this <code>curl | sh</code> command can still ask its first-run questions safely.</p>
      <p><strong>Windows PowerShell</strong></p>
      <pre><code>irm https://github.com/giofahreza/io-gateway/releases/latest/download/install.ps1 | iex</code></pre>
      <p>Use <code>--version vX.Y.Z</code> on Linux or macOS, or <code>-Version vX.Y.Z</code> in PowerShell, to install a particular release.</p>
      <h3>Supported release platforms</h3>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Operating system</th><th>Published CPUs</th><th>Notes</th></tr></thead><tbody>
        <tr><td>Linux</td><td>x86_64, ARM64</td><td>64-bit glibc-based distributions. Alpine and other musl-only systems should build from source.</td></tr>
        <tr><td>macOS</td><td>Intel, Apple Silicon</td><td>Intel binaries target macOS 10.13+; Apple Silicon binaries target macOS 11+.</td></tr>
        <tr><td>Windows</td><td>x86_64, ARM64</td><td>Use the PowerShell installer on 64-bit Windows.</td></tr>
      </tbody></table></div>
      <p>Other CPU families and 32-bit systems are not currently published as release assets.</p>
      <h3>Choose first-run setup</h3>
      <ul class="docs-list">
        <li><strong>Installs per user</strong> No <code>sudo</code> or Administrator prompt is needed by default.</li>
        <li><strong>Choose a local port</strong> The first question chooses a TCP port from 1 through 65535 (default <code>8319</code>). The installer preflights <code>127.0.0.1:&lt;port&gt;</code>: an occupied interactive choice is asked again, while an explicit or unattended occupied port fails before a new config is created. The generated config stays localhost-only, creates <code>auths/</code>, and generates a private client API key.</li>
        <li><strong>Choose the terminal client</strong> Decide whether to install the optional <code>iogw</code> management client and TUI beside the gateway binary.</li>
        <li><strong>Choose autostart at sign-in</strong> Enable a systemd user service on Linux, a LaunchAgent on macOS, or a per-user Scheduled Task on Windows. This is a persistent next-sign-in choice, not an immediate launch.</li>
        <li><strong>Choose whether to start now</strong> Start the gateway immediately after installation, independently of autostart. Choose this for a one-off local background process even when sign-in autostart is off, or decline it while enabling the next-sign-in service.</li>
      </ul>
      <p>These questions apply only when a configuration does not already exist. Upgrades preserve the existing configuration, credential directory, and chosen <code>listen</code> port.</p>
      <h3>Automate an install</h3>
      <p>Use explicit flags when there is no terminal or when setup must be repeatable. Unix accepts <code>--port</code>, <code>--with-iogw</code> / <code>--without-iogw</code>, <code>--autostart</code> / <code>--no-autostart</code>, <code>--start-now</code> / <code>--no-start</code>, and <code>--interactive</code> / <code>--non-interactive</code>. PowerShell accepts <code>-Port</code>, <code>-InstallIogw</code> / <code>-NoIogw</code>, <code>-AutoStart</code> / <code>-NoAutoStart</code>, <code>-StartNow</code> / <code>-NoStart</code>, and <code>-Interactive</code> / <code>-NonInteractive</code>.</p>
      <pre><code>IO_GATEWAY_PORT=9444 \\
IO_GATEWAY_INSTALL_IOGW=no \\
IO_GATEWAY_AUTOSTART=no \\
IO_GATEWAY_START_NOW=no \\
IO_GATEWAY_INTERACTIVE=no \\
bash -c 'set -o pipefail; curl -fsSL https://github.com/giofahreza/io-gateway/releases/latest/download/install.sh | sh'</code></pre>
      <p>The same cross-platform environment variables are available with values <code>auto</code>, <code>yes</code>, or <code>no</code> where applicable: <code>IO_GATEWAY_INSTALL_IOGW</code>, <code>IO_GATEWAY_AUTOSTART</code>, <code>IO_GATEWAY_START_NOW</code>, and <code>IO_GATEWAY_INTERACTIVE</code>; <code>IO_GATEWAY_PORT</code> selects the port. Explicit command-line choices take precedence. <code>--start-now</code> / <code>-StartNow</code> starts the gateway immediately without changing autostart; <code>--no-start</code> / <code>-NoStart</code> skips that launch while preserving an existing service or task. Combine <code>--autostart --no-start</code> for a next-sign-in-only service, or <code>--no-autostart --start-now</code> for a one-off local background process.</p>
      <div class="docs-note warning"><strong>Secure before exposing</strong><p>Admin authentication is initially disabled only for local setup. Configure a separate admin API key, a TOTP secret, and <code>admin_auth</code> before changing <code>listen</code> to a LAN or public address. The client-facing <code>proxy_api_key</code> is never a dashboard credential.</p></div>
      <h2>Finish setup</h2>
      <ol class="docs-steps">
        <li><span>1</span><div><strong>Open the local dashboard.</strong><p>If you chose <em>Start now</em>, visit <code>http://127.0.0.1:&lt;selected-port&gt;/</code> after the gateway becomes healthy (the default is <code>http://127.0.0.1:8319/</code>). Otherwise, use the printed command to start it now or sign in again when autostart is enabled.</p></div></li>
        <li><span>2</span><div><strong>Review configuration.</strong><p>Use the <a class="docs-inline-link" href="/docs/configuration/">configuration reference</a> to set <code>listen</code>, <code>proxy_api_key</code>, <code>auth_dir</code>, and dashboard authentication for the intended environment.</p></div></li>
        <li><span>3</span><div><strong>Add a provider account.</strong><p>Use the provider account workflow before sending client traffic.</p></div></li>
      </ol>
      <h2>Verify</h2>
      <pre><code># Replace 8319 if you chose another first-run port.
curl http://127.0.0.1:8319/health
curl http://127.0.0.1:8319/ready</code></pre>
      <p>After the health checks pass, use <a class="docs-inline-link" href="/docs/test-api/">Test API</a> from the dashboard to send a short prompt through one model route.</p>
      <h2>Build from source</h2>
      <p>For development or an unsupported platform, work from a repository checkout with Rust installed.</p>
      <pre><code>cargo build --release
cp config.example.json config.json
# Edit config.json before exposing the gateway.
./target/release/io-gateway --config ./config.json</code></pre>
      <p><code>--config PATH</code> takes precedence over <code>IO_GATEWAY_CONFIG</code>. When a selected configuration uses a relative <code>auth_dir</code>, IO Gateway resolves it from that configuration file's directory rather than the current working directory.</p>
      <h2>Next steps</h2>
      <ul class="docs-list">
        <li><strong>Secure dashboard access</strong> Configure admin auth before exposing the service.</li>
        <li><strong>Create managed API keys</strong> Replace broad shared-key access with provider/account scopes, per-request input caps, and optional input-token budgets.</li>
        <li><strong>Plan routing</strong> Use priority routing or custom models when traffic should prefer specific accounts.</li>
      </ul>
    `,
  },
  {
    slug: "first-client-request",
    title: "First client request",
    group: "Tutorials",
    type: "Tutorial",
    appliesTo: "A running gateway with one enabled provider account",
    introduced: docsVersion,
    updated,
    categories: ["Tutorials", "API access", "Routing"],
    keywords: "client request curl responses api chat completions base url bearer key models first request openai claude",
    summary: "Point a client at IO Gateway, inspect its model catalog, and prove a request reaches an upstream account.",
    seeAlso: ["quick-start", "api-keys", "routing-and-models", "usage-and-quota"],
    body: `
      <p class="docs-lead">Use this path after the gateway is healthy and at least one provider account is enabled. It proves the client-facing boundary: base URL, bearer key, model selection, and the receipt left in usage history.</p>
      <h2>What you need</h2>
      <ul class="docs-list">
        <li><strong>A ready gateway</strong> Confirm <code>/ready</code> returns successfully before configuring a client.</li>
        <li><strong>A client key</strong> Use the shared <code>proxy_api_key</code> or a managed key with access to the provider/account target it may call. Custom aliases are authorized through their concrete targets, not through alias-specific key scopes.</li>
        <li><strong>An enabled model</strong> Ask the gateway for its catalog instead of guessing a provider model name.</li>
      </ul>
      <h2>Read the catalog first</h2>
      <pre><code>export IO_GATEWAY_URL=http://127.0.0.1:8319
export IO_GATEWAY_KEY='replace-with-your-client-key'

curl -sS "$IO_GATEWAY_URL/v1/models" \\
  -H "Authorization: Bearer $IO_GATEWAY_KEY"</code></pre>
      <p>Choose a model returned by <code>/v1/models</code>. For a managed key, the catalog is filtered to its permitted provider/account routes. A model name can select a provider naturally, a three-letter prefix can force one, and a <code>ctm:</code> alias can apply a route policy you created.</p>
      <h2>Send one response</h2>
      <pre><code>curl -sS "$IO_GATEWAY_URL/v1/responses" \\
  -H "Authorization: Bearer $IO_GATEWAY_KEY" \\
  -H "Content-Type: application/json" \\
  --data '{
    "model": "gpt-5.2",
    "input": "Reply with one short line."
  }'</code></pre>
      <p>Replace <code>gpt-5.2</code> with a model from your own catalog. The Responses API is the primary OpenAI-compatible route; existing OpenAI Chat Completions clients can use <code>POST /v1/chat/completions</code>, and Anthropic clients can use <code>POST /claude/v1/messages</code>.</p>
      <h2>Read the receipt</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Confirm the client response.</strong><p>A successful response proves the request cleared client authentication and reached an eligible route.</p></div></li>
        <li><span>2</span><div><strong>Check usage history.</strong><p>Confirm the selected provider and account match the route policy you expected.</p></div></li>
        <li><span>3</span><div><strong>Repeat with a managed key.</strong><p>Use the real key your client will receive to prove its provider/account scope, per-request input cap, and input-token budget behave as intended.</p></div></li>
      </ol>
      <div class="docs-note"><strong>If the route is rejected</strong><p>Check the key's provider/account scope, model spelling or prefix, custom-model state, and provider-account eligibility before changing the client. The <a class="docs-inline-link" href="/docs/test-api/">Test API</a> can isolate an operator route problem in its default bypass mode, or validate a selected managed-key profile without entering that key's secret.</p></div>
    `,
  },
  {
    slug: "dashboard",
    title: "Dashboard",
    group: "How-to guides",
    type: "How-to guide",
    appliesTo: "Authenticated operators",
    introduced: docsVersion,
    updated,
    categories: ["How-to guides", "Accounts", "API access"],
    keywords: "dashboard login providers settings overview custom models api keys usage history admin",
    summary: "Use the operator dashboard for accounts, testing, custom models, keys, and history.",
    seeAlso: ["provider-accounts", "api-keys", "usage-and-quota"],
    body: `
      <p class="docs-lead">The dashboard is the operator console for upstream accounts, model routing, Test API, managed API keys, usage history, and notification settings.</p>
      ${docsFigure("docs-dashboard-overview.png", "IO Gateway dashboard populated with usage totals, context chart, custom routes, and provider account cards.", "The dashboard surfaces account state, quota, custom routes, Test API, and settings in one operator view.")}
      <h2>Before you start</h2>
      <p>Enable dashboard authentication in <a class="docs-inline-link" href="/docs/configuration/">configuration</a> before exposing the dashboard outside a trusted network.</p>
      <h2>Common tasks</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Add provider accounts.</strong><p>Open the provider account section and add OAuth or API-key credentials for each upstream provider.</p></div></li>
        <li><span>2</span><div><strong>Test a model route.</strong><p>Use <a class="docs-inline-link" href="/docs/test-api/">Test API</a> after adding credentials or changing route rules.</p></div></li>
        <li><span>3</span><div><strong>Create custom models.</strong><p>Build <code>ctm:</code> aliases when clients need stable names, weights, account targeting, or fallback chains.</p></div></li>
        <li><span>4</span><div><strong>Manage API keys.</strong><p>Scope keys by provider and account, then set per-request input caps or optional input-token budgets.</p></div></li>
      </ol>
      <h2>Operator checks</h2>
      <div class="docs-grid">
        <article><h3>Account health</h3><p>Look for disabled, cooling-down, failed, and quota-exhausted accounts before blaming client requests.</p></article>
        <article><h3>Usage history</h3><p>Use provider and account history to confirm routing behavior after changes.</p></article>
        <article><h3>Custom aliases</h3><p>Confirm an alias appears in the catalog returned to the intended client key before publishing it to clients.</p></article>
        <article><h3>Notifications</h3><p>Send a notification test after changing Telegram or Google Chat settings.</p></article>
      </div>
      <h2>Verify changes</h2>
      <p>After each dashboard change, send one Test API request and inspect <a class="docs-inline-link" href="/docs/usage-and-quota/">usage and quota</a> to confirm which provider account handled it.</p>
    `,
  },
  {
    slug: "provider-accounts",
    title: "Provider accounts",
    group: "How-to guides",
    type: "How-to guide",
    appliesTo: "Upstream account management",
    storage: "auths/",
    introduced: docsVersion,
    updated,
    categories: ["How-to guides", "Accounts"],
    keywords: "provider accounts codex gemini claude qwen glm grok copilot oauth api key auth_dir disable refresh reauth",
    summary: "Add, enable, refresh, re-auth, and test upstream provider accounts.",
    seeAlso: ["dashboard", "priority-routing", "usage-and-quota", "codex-reset-credit-automation"],
    body: `
      <p class="docs-lead">Provider accounts are the upstream identities IO Gateway uses when routing model requests. Add at least one healthy account before exposing an API key to clients.</p>
      ${docsFigure("docs-provider-accounts.png", "Provider account cards with enabled, disabled, priority, quota, reset limit, and attention states.", "Provider cards make routing eligibility visible before traffic reaches the account pool.")}
      <h2>Before you start</h2>
      <ul class="docs-list">
        <li><strong>Credential storage</strong> Confirm <code>auth_dir</code> points to the directory where provider credentials should live.</li>
        <li><strong>Provider support</strong> Use provider-specific account flows for Claude, Gemini, Codex, Qwen, GLM, Grok, Copilot, or other enabled providers.</li>
      </ul>
      <h2>Add or maintain an account</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Open the dashboard provider section.</strong><p>Select the provider that should receive traffic.</p></div></li>
        <li><span>2</span><div><strong>Add the credential.</strong><p>Use the provider-specific OAuth or API-key flow.</p></div></li>
        <li><span>3</span><div><strong>Test the account.</strong><p>Run a short Test API prompt to prove the account can serve traffic.</p></div></li>
        <li><span>4</span><div><strong>Watch state changes.</strong><p>Disabled, failed, cooling-down, and quota-exhausted accounts are skipped by routing.</p></div></li>
      </ol>
      <h2>Account operations</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Operation</th><th>Use when</th></tr></thead><tbody>
        <tr><td>Disable</td><td>Remove an account from routing without deleting its credential.</td></tr>
        <tr><td>Refresh</td><td>Reload health, quota, or provider-side state.</td></tr>
        <tr><td>Re-authenticate</td><td>Repair expired OAuth or invalid provider credentials.</td></tr>
        <tr><td>Delete</td><td>Remove the account and prune routing state such as priority membership.</td></tr>
      </tbody></table></div>
      <h2>Verify routing</h2>
      <p>Use <a class="docs-inline-link" href="/docs/usage-and-quota/">usage history</a> to confirm that traffic reaches the expected account. For deliberate account draining, enable <a class="docs-inline-link" href="/docs/priority-routing/">priority routing</a>.</p>
    `,
  },
  {
    slug: "codex-reset-credit-automation",
    title: "Codex reset-credit automation",
    group: "How-to guides",
    type: "How-to guide",
    appliesTo: "Preserving expiring Codex reset credits",
    storage: "<auth_dir>/api-key-policy.sqlite3",
    introduced: docsVersion,
    updated: "September 21, 2026",
    categories: ["How-to guides", "Accounts", "Limits", "Operations"],
    keywords: "codex chatgpt app server json rpc rate limit reset credit expiry automatic redeem idempotency account profile policy sqlite action ledger",
    summary: "Safely preserve an expiring Codex reset credit through a managed local App Server profile when an account limit is reached.",
    seeAlso: ["provider-accounts", "usage-and-quota", "configuration"],
    body: `
      <p class="docs-lead">Codex reset-credit automation is an opt-in, per-account safety net for an earned ChatGPT usage-limit reset credit that is nearing expiry. It is deliberately conservative: it never treats a dashboard cache, a renamed credential, or an ambiguous network result as authority to spend a credit.</p>
      <h2>What the policy does</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Read fresh App Server state.</strong><p>The worker reads Codex rate limits and reset-credit details through the mapped local App Server profile; the dashboard quota cache is not used to authorize redemption.</p></div></li>
        <li><span>2</span><div><strong>Select one concrete credit.</strong><p>It chooses the earliest-expiring available Codex-rate-limits credit (the App Server form is <code>codexRateLimits</code>) with an explicit ID and a known expiry at least 30 minutes and no later than the configured upper window from the fresh read.</p></div></li>
        <li><span>3</span><div><strong>Prove it is needed.</strong><p>Immediately before the POST, another fresh read must show a reached Codex limit, the same credit still available, and a natural reset that is not already near.</p></div></li>
        <li><span>4</span><div><strong>Persist, redeem, verify.</strong><p>The exact credit ID and a gateway-generated idempotency key are durable before the POST. A successful or idempotent answer needs a final fresh read proving the limit cleared.</p></div></li>
      </ol>
      <h2>Use managed local App Server profiles</h2>
      <p>Reset-credit work uses the documented Codex App Server JSON-RPC account API, not <code>upstream_base</code> or the gateway's direct HTTP/Wham quota adapter. It launches a short-lived local <code>codex app-server --stdio</code> child for <code>account/rateLimits/read</code> and <code>account/rateLimitResetCredit/consume</code>. Codex App Server is experimental and unsupported for production workloads, so this feature remains off unless its configuration explicitly opts in; there is no direct-backend fallback.</p>
      <pre><code>{
  "codex_reset_credit_app_server": {
    "experimental_opt_in": true,
    "command": "/usr/local/bin/codex",
    "profile_root": "/var/lib/io-gateway/codex-app-server-profiles",
    "profiles": [{
      "account_key": "codex:account_id:org_123",
      "profile": "org_123",
      "expected_email": "ops@example.com"
    }]
  }
}</code></pre>
      <p>The executable and profile root must be absolute existing paths. Each simple profile directory is private and uniquely bound to one stable account, and must already contain its own private <code>auth.json</code> from a Codex-managed ChatGPT login. IO Gateway verifies the managed account and its required configured email on launch, clears ambient environment values, supplies no gateway bearer credential to the child, and fixes a file-backed credential-store override. A profile <code>config.toml</code> may not set <code>sqlite_home</code>; an explicit <code>cli_auth_credentials_store</code> is allowed only when it is <code>"file"</code>.</p>
      <h2>Before enabling it</h2>
      <ul class="docs-list">
        <li><strong>Keep the dashboard private.</strong> This is an administrator-only operation on an upstream ChatGPT account.</li>
        <li><strong>Use a stable identity and profile.</strong> The account must be an enabled, uniquely resolved <code>codex:account_id:&lt;id&gt;</code> with a matching configured managed App Server profile. Labels, filenames, and anonymous <code>manual-N</code> identities are rejected for unattended use.</li>
        <li><strong>Expect fail-closed behavior.</strong> A count/details state that cannot prove the exact selected credit, unknown expiry, unavailable fresh state, or unknown natural reset time means no automatic spend; an existing automatic action is deferred for a later fresh check.</li>
      </ul>
      <h2>Default timing</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Setting</th><th>Default</th><th>Effect</th></tr></thead><tbody>
        <tr><td><code>scan_interval_minutes</code></td><td>30</td><td>Per-account cadence for newly eligible credits; enabling or editing an enabled policy also schedules one immediate scan.</td></tr>
        <tr><td><code>expiry_window_minutes</code></td><td>60</td><td>Upper selection bound: a new automatic action requires <code>30 minutes &lt;= expires_at - now &lt;= window</code>.</td></tr>
        <tr><td><code>final_attempt_minutes</code></td><td>5</td><td>Schedule a deferred action's final pre-expiry check this many minutes before expiry when possible.</td></tr>
        <tr><td><code>min_natural_reset_remaining_minutes</code></td><td>10</td><td>Retain the credit when the ordinary reached window will reset at or within this time.</td></tr>
      </tbody></table></div>
      <p>The worker wakes every minute only to honor durable action deadlines; it does not scan every account every minute. Policy scans remain on their configured cadence. With the default 30-minute cadence and 60-minute upper window, new credits are selected only in the intended 30–60 minute interval before expiry. An already selected durable action may be rechecked closer to expiry after a deferral, retry, or temporary upstream failure; that never makes a newly discovered credit under 30 minutes old eligible. <code>expiry_window_minutes</code> must be 30 through 10,080, and <code>final_attempt_minutes</code> must be 1 through one minute less than that window. A restart preserves scan and action deadlines, but an outage that lasts beyond credit expiry cannot be recovered automatically.</p>
      <h2>Enable and inspect</h2>
      <p>Read the eligible account keys, defaults, policies, and browser-safe action status:</p>
      <pre><code>curl -sS http://127.0.0.1:8319/admin/codex/reset-credit-automation \\
  -b "$ADMIN_COOKIE"</code></pre>
      <p>Enable the default policy for an account marked eligible:</p>
      <pre><code>curl -sS -X POST http://127.0.0.1:8319/admin/codex/reset-credit-automation \\
  -b "$ADMIN_COOKIE" \\
  -H 'Content-Type: application/json' \\
  --data '{
    "account_key":"codex:account_id:org_123",
    "enabled":true,
    "scan_interval_minutes":30,
    "expiry_window_minutes":60,
    "final_attempt_minutes":5,
    "min_natural_reset_remaining_minutes":10
  }'</code></pre>
      <p>Disabling the same stable account key stops future automatic work and remains allowed even after its credential is disabled or removed. Existing actions remain durable for audit and safe recovery; disabling does not make the worker choose another credit. The write endpoint rejects unknown fields and never returns its internal idempotency keys.</p>
      <h2>Read action state safely</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>State</th><th>Meaning</th></tr></thead><tbody>
        <tr><td><code>pending</code></td><td>A specific credit is waiting for its first durable attempt.</td></tr>
        <tr><td><code>deferred</code></td><td>The credit is being retained for a later check: no reached limit, nearby/unknown natural reset, a disabled policy, or a safe retry time.</td></tr>
        <tr><td><code>submitted</code></td><td>The exact credit and idempotency key were stored before a POST; any retry uses those same values.</td></tr>
        <tr><td><code>verified</code></td><td>A fresh post-consume read confirmed the reached rate limit cleared.</td></tr>
        <tr><td><code>no_credit</code>, <code>expired</code></td><td>The selected credit is no longer usable.</td></tr>
        <tr><td><code>manual_review</code></td><td>The outcome is ambiguous, or a legacy direct-HTTP submission needs review. Automation stops rather than spending another credit.</td></tr>
      </tbody></table></div>
      <p>Opaque credit IDs, timing, and safe outcome metadata are stored, but provider credentials, prompts, raw request bodies, raw upstream responses, and idempotency keys are not exposed through the browser endpoint.</p>
      <h2>Manual redemption shares the ledger</h2>
      <p>Manual reset requests require a concrete <code>credit_id</code> and the same matching managed App Server profile; the gateway generates and persists the idempotency key. The dashboard quota view is informational only: the endpoint re-reads the selected credit and rate-limit state through App Server before spending. Do not ask upstream to select the next credit or send a client idempotency key.</p>
      <pre><code>curl -sS -X POST http://127.0.0.1:8319/codex/rate-limit-reset-credit/consume \\
  -b "$ADMIN_COOKIE" \\
  -H 'Content-Type: application/x-www-form-urlencoded' \\
  -d 'file_name=my-codex-account.json' \\
  -d 'credit_id=credit_...'</code></pre>
      <div class="docs-note"><strong>Operate from durable evidence</strong><p>Only <code>verified</code> proves that a reset completed. A timeout after submission retains the exact credit and idempotency key for recovery. Treat <code>manual_review</code> as an operator task, not a reason to send another redemption request with a different credit.</p></div>
      <h2>Legacy transport migration</h2>
      <p>Older direct-HTTP/Wham actions are migrated safely. A legacy action already marked <code>submitted</code> becomes <code>manual_review</code>; its old idempotency key is never replayed through App Server. A legacy <code>pending</code> or <code>deferred</code> action is retained and must pass fresh App Server checks before it can proceed.</p>
      <h2>Persistence and backup</h2>
      <p>Policies and actions live in <code>&lt;auth_dir&gt;/api-key-policy.sqlite3</code> with managed API-key policy and quota accounting. Keep that directory on persistent local storage, retain its identity file, and use <code>io-gateway --config /absolute/config.json --backup-policy /backups/new-policy-snapshot</code> for a WAL-consistent policy snapshot. Do not share the live SQLite database across hosts or over NFS.</p>
    `,
  },
  {
    slug: "priority-routing",
    title: "Priority routing",
    group: "How-to guides",
    type: "How-to guide",
    appliesTo: "Provider account routing",
    storage: "auths/account-routing.json",
    introduced: docsVersion,
    updated,
    categories: ["How-to guides", "Routing", "Accounts"],
    keywords: "priority account use first drain quota disabled auto remove routing provider account-routing",
    summary: "Prioritize one or more provider accounts so they are used first until unavailable or fully drained.",
    seeAlso: ["provider-accounts", "routing-and-models", "usage-and-quota"],
    body: `
      <p class="docs-lead">Priority routing lets operators choose one or more <a class="docs-inline-link" href="/docs/provider-accounts/">provider accounts</a> that receive traffic before the normal account pool. Use it to spend selected subscriptions, balances, or trials before spreading traffic to the rest of the provider.</p>
      ${docsFigure("docs-priority-routing.png", "Codex account action menu showing a priority account with quota bars and a remove-priority control.", "Priority membership is managed from the account actions menu and remains visible on the account card.")}
      <h2>Before you start</h2>
      <ul class="docs-list">
        <li><strong>Provider account exists</strong> Add and test the upstream account before marking it as priority.</li>
        <li><strong>Account remains eligible</strong> Priority does not bypass disabled, failed, cooling-down, or quota-exhausted states.</li>
        <li><strong>Scope is provider-local</strong> A prioritized Claude account affects Claude routing only, not Gemini, GLM, Codex, or other providers.</li>
      </ul>
      <h2>Enable priority</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Open a provider account card.</strong><p>Use the <a class="docs-inline-link" href="/docs/dashboard/">dashboard</a> provider section for the account you want to spend first.</p></div></li>
        <li><span>2</span><div><strong>Open the account actions menu.</strong><p>The priority control appears as the <em>Use first</em> action on eligible accounts.</p></div></li>
        <li><span>3</span><div><strong>Choose <em>Use first</em>.</strong><p>The account joins the provider priority set and receives traffic before normal accounts.</p></div></li>
        <li><span>4</span><div><strong>Repeat for additional accounts.</strong><p>Use this when several accounts should drain together before the rest of the pool.</p></div></li>
      </ol>
      <h2>Verify behavior</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Check</th><th>Expected result</th></tr></thead><tbody>
        <tr><td><code>GET /admin/account-routing</code></td><td>The account appears in the provider priority list.</td></tr>
        <tr><td>Dashboard account card</td><td>The card reflects the current priority state.</td></tr>
        <tr><td>Usage history</td><td>Requests for that provider hit priority accounts before normal accounts while they remain eligible.</td></tr>
        <tr><td>Disable account</td><td>The account is removed from priority and no longer receives traffic.</td></tr>
      </tbody></table></div>
      <h2>Automatic removal</h2>
      <p>Priority is removed automatically when an account is disabled or deleted. IO Gateway prunes <code>auths/account-routing.json</code> so stale priority entries cannot keep routing to disabled accounts.</p>
      <div class="docs-note"><strong>Best use</strong><p>Use priority for deliberate account draining. For permanent tenant isolation, create scoped <a class="docs-inline-link" href="/docs/api-keys/">API keys</a> or <a class="docs-inline-link" href="/docs/custom-models/">custom models</a> with account rules.</p></div>
    `,
  },
  {
    slug: "custom-models",
    title: "Custom models",
    group: "How-to guides",
    type: "How-to guide",
    appliesTo: "Model alias routing",
    introduced: docsVersion,
    updated,
    categories: ["How-to guides", "Routing"],
    keywords: "custom models ctm alias targets weights fallback account targeting exclusion provider chain",
    summary: "Create ctm: aliases with provider targets, specific accounts, weights, and fallback chains.",
    seeAlso: ["routing-and-models", "api-keys", "test-api"],
    body: `
      <p class="docs-lead">Custom models create stable <code>ctm:</code> aliases that can route to one or more provider models, specific accounts, weighted target sets, or fallback chains.</p>
      ${docsFigure("docs-custom-models.png", "Custom model cards showing ctm aliases with weighted provider targets and fallback steps.", "A custom model is shown as a route card: stable alias, route steps, target count, and provider/account targets.")}
      <h2>Before you start</h2>
      <p>Confirm the target provider accounts are healthy and visible in the <a class="docs-inline-link" href="/docs/dashboard/">dashboard</a>. If the alias will be exposed to clients, make sure each target provider/account is allowed by the relevant <a class="docs-inline-link" href="/docs/api-keys/">API keys</a>; keys do not have alias-specific allow-lists.</p>
      <h2>Create an alias</h2>
      <ol class="docs-steps">
        <li><span>1</span><div><strong>Open Custom Models in the dashboard.</strong><p>This section manages aliases that clients call as <code>ctm:name</code>.</p></div></li>
        <li><span>2</span><div><strong>Choose a stable name.</strong><p>Use an operational name such as <code>ctm:workhorse</code>, <code>ctm:research</code>, or <code>ctm:fast</code>.</p></div></li>
        <li><span>3</span><div><strong>Add provider targets.</strong><p>Each target points to a provider model and can optionally select a specific account.</p></div></li>
        <li><span>4</span><div><strong>Set weights and fallback order.</strong><p>Use weighting for distribution and fallback order for controlled failover.</p></div></li>
        <li><span>5</span><div><strong>Save and test.</strong><p>Run a <a class="docs-inline-link" href="/docs/test-api/">Test API</a> prompt before publishing the alias.</p></div></li>
      </ol>
      <h2>Supported behavior</h2>
      <ul class="docs-list">
        <li><strong>Multiple provider targets</strong> Route one alias to several providers or models.</li>
        <li><strong>Specific account targeting</strong> Pin a target to one upstream account when needed.</li>
        <li><strong>Account exclusion</strong> Use every account except a selected account for maintenance or isolation.</li>
        <li><strong>Weighted load balancing</strong> Give preferred targets more traffic while keeping backups available.</li>
        <li><strong>Fallback chains</strong> Try another target when a provider fails, cools down, or runs out of quota.</li>
      </ul>
      <h2>Example</h2>
      <pre><code>{
  "model": "ctm:research",
  "targets": [
    { "model": "cld:claude-sonnet-4", "weight": 3 },
    { "model": "gem:gemini-3-pro", "weight": 1 },
    { "model": "glm:glm-4.5", "weight": 1 }
  ]
}</code></pre>
    `,
  },
  {
    slug: "test-api",
    title: "Test API",
    group: "How-to guides",
    type: "How-to guide",
    appliesTo: "Dashboard route validation",
    introduced: docsVersion,
    updated,
    categories: ["How-to guides", "API access"],
    keywords: "test api dashboard settings model validate route managed key policy prompt admin session smoke test",
    summary: "Validate upstream routes in operator-bypass mode or against a selected managed-key policy.",
    seeAlso: ["dashboard", "custom-models", "usage-and-quota"],
    body: `
      <p class="docs-lead">Test API sends a dashboard-authenticated prompt through the provider-routing layer. Use its default operator-bypass mode to validate an upstream account, model, or custom route; select a managed-key profile to validate that key's policy too.</p>
      ${docsFigure("docs-test-api.png", "Test API panel showing a custom model request and a successful response with HTTP status, latency, selected model, and raw response details.", "Use Test API to validate route behavior before giving the route to client keys.")}
      <h2>Before you start</h2>
      <p>Sign in to the <a class="docs-inline-link" href="/docs/dashboard/">dashboard</a> with an operator session. The default selection is operator bypass, which does not apply a managed key's scope, per-request cap, or budget. Select an active managed API-key profile to apply those controls before dispatch; only its ID is sent, never its plaintext secret.</p>
      <h2>Run a test</h2>
      <ol class="docs-steps">
        <li><span>1</span><div><strong>Open Test API.</strong><p>Use the dashboard action near account and model management.</p></div></li>
        <li><span>2</span><div><strong>Select a model and policy mode.</strong><p>Choose a provider model or a <code>ctm:</code> alias; keep operator bypass for a route-only check or choose an active managed-key profile to test its policy.</p></div></li>
        <li><span>3</span><div><strong>Send a short prompt.</strong><p>Use a small request when validating credentials, quota, or routing behavior.</p></div></li>
        <li><span>4</span><div><strong>Inspect the result.</strong><p>Check response status, account selection, and provider error details.</p></div></li>
      </ol>
      <h2>What a passing result proves</h2>
      <ul class="docs-list">
        <li><strong>Credentials work</strong> The selected provider account can authenticate upstream.</li>
        <li><strong>Route exists</strong> The selected model or custom alias is known to IO Gateway.</li>
        <li><strong>Operator route works</strong> In bypass mode, the gateway can choose an eligible upstream account and send the operator test.</li>
        <li><strong>Managed policy works</strong> With a selected profile, provider/account scope, per-request caps, and the cumulative input-token budget are checked before dispatch.</li>
      </ul>
      <div class="docs-note"><strong>Choose the right mode</strong><p>Operator bypass proves the route without client-key restrictions. A selected managed profile validates policy without exposing its secret; send a request with the actual client key as the final integration check.</p></div>
    `,
  },
  {
    slug: "notifications",
    title: "Notifications",
    group: "How-to guides",
    type: "How-to guide",
    appliesTo: "Operational alerts",
    introduced: docsVersion,
    updated,
    categories: ["How-to guides", "Notifications"],
    keywords: "notifications telegram google chat webhook alerts auth upstream quota test failures",
    summary: "Configure Telegram or Google Chat alerts for auth errors, upstream failures, and quota events.",
    seeAlso: ["dashboard", "troubleshooting", "usage-and-quota"],
    body: `
      <p class="docs-lead">Notifications help operators catch upstream auth errors, provider failures, quota events, and service issues without watching dashboard state continuously.</p>
      <h2>Before you start</h2>
      <ul class="docs-list">
        <li><strong>Destination exists</strong> Prepare a Telegram bot/chat or Google Chat webhook.</li>
        <li><strong>Secrets are protected</strong> Store notification tokens with the same care as provider credentials.</li>
      </ul>
      <h2>Configure alerts</h2>
      <ol class="docs-steps">
        <li><span>1</span><div><strong>Open notification settings.</strong><p>Use the dashboard settings section.</p></div></li>
        <li><span>2</span><div><strong>Select a provider.</strong><p>Choose Telegram or Google Chat and paste the required token or webhook URL.</p></div></li>
        <li><span>3</span><div><strong>Send a test message.</strong><p>Confirm the destination receives a test before relying on alerts.</p></div></li>
        <li><span>4</span><div><strong>Review noise.</strong><p>Keep alerts actionable so real failures are not ignored.</p></div></li>
      </ol>
      <h2>Event types</h2>
      <ul class="docs-list">
        <li><strong>Authentication failures</strong> Provider credentials expired, revoked, or rejected upstream.</li>
        <li><strong>Quota and usage events</strong> Accounts approach or hit provider limits.</li>
        <li><strong>Provider failures</strong> Upstream provider errors, cooldowns, and repeated request failures.</li>
      </ul>
      <h2>Troubleshoot delivery</h2>
      <p>If a test notification does not arrive, verify the destination secret, outbound network access, and dashboard logs before testing provider traffic again.</p>
    `,
  },
  {
    slug: "configuration",
    title: "Configuration",
    group: "Reference",
    type: "Reference",
    appliesTo: "Server configuration",
    storage: "config.json and environment variables",
    introduced: docsVersion,
    updated,
    categories: ["Reference", "Configuration"],
    keywords: "config json env admin auth proxy api key totp secure cookies trusted proxy codex app server reset credit profiles",
    summary: "Core config.json fields, environment overrides, dashboard auth, proxy safety, and optional Codex App Server reset-credit profiles.",
    seeAlso: ["quick-start", "deployment", "troubleshooting"],
    body: `
      <p class="docs-lead">Configuration controls the HTTP listener, upstream defaults, credential directory, dashboard authentication, proxy trust, and retention settings.</p>
      <h2>Start local</h2>
      <pre><code>{
  "listen": "127.0.0.1:8319",
  "upstream_base": "https://chatgpt.com/backend-api/codex",
  "proxy_api_key": "your-shared-proxy-key",
  "tokens": [],
  "auth_dir": "./auths"
}</code></pre>
      <p>A local listener is the safe first-run default. It keeps the dashboard and client API on the same machine until you deliberately place the gateway behind an authenticated HTTPS proxy.</p>
      <h2>Important fields</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Field</th><th>Purpose</th></tr></thead><tbody>
        <tr><td><code>listen</code></td><td>Socket address for the dashboard and API server.</td></tr>
        <tr><td><code>proxy_api_key</code></td><td>Shared key for client API requests unless managed API keys are used.</td></tr>
        <tr><td><code>auth_dir</code></td><td>Directory where provider credential files are stored.</td></tr>
        <tr><td><code>disabled_files</code></td><td>Credential files that should load but start disabled.</td></tr>
        <tr><td><code>codex_reset_credit_app_server</code></td><td>Optional explicit local Codex App Server configuration for reset-credit automation and manual redemption.</td></tr>
        <tr><td><code>admin_auth</code></td><td>Dashboard authentication: a TOTP secret, optional separate admin API key, cookie security, and session lifetime.</td></tr>
        <tr><td><code>trusted_proxy</code></td><td>Enable only behind a reverse proxy that sanitizes forwarded IP headers.</td></tr>
        <tr><td><code>history_retention_days</code></td><td>How long usage history remains available for charts and summaries.</td></tr>
      </tbody></table></div>
      <h2>Codex reset-credit App Server</h2>
      <p>Reset-credit automation uses the documented local Codex App Server JSON-RPC account API, not <code>upstream_base</code> or the direct HTTP quota adapter. It is experimental and off until an administrator explicitly opts in with a trusted executable and a private managed profile for each stable account.</p>
      <pre><code>{
  "codex_reset_credit_app_server": {
    "experimental_opt_in": true,
    "command": "/usr/local/bin/codex",
    "profile_root": "/var/lib/io-gateway/codex-app-server-profiles",
    "profiles": [{
      "account_key": "codex:account_id:org_123",
      "profile": "org_123",
      "expected_email": "ops@example.com"
    }]
  }
}</code></pre>
      <p>The command and profile root are absolute existing paths. Profile names are simple directory names below the root, are unique per stable account, and must already hold a private file-backed Codex-managed ChatGPT login in <code>auth.json</code>. Every enabled profile binding must provide <code>expected_email</code>, which must match App Server's <code>account/read</code> email. This confirms the managed profile's ChatGPT email; it does not prove the upstream stable account ID, which remains the <code>account_key</code> mapping. On Unix, the root and profile directories must be private (<code>0700</code> or stricter) and not symlinks. IO Gateway runs only <code>codex app-server --stdio -c cli_auth_credentials_store="file"</code> with a scrubbed profile-specific environment; it never passes the gateway credential to the child. A profile <code>config.toml</code> may not set <code>sqlite_home</code> and may select only the <code>file</code> credential store. See <a class="docs-inline-link" href="/docs/codex-reset-credit-automation/">Codex reset-credit automation</a> for the safety model and migration behavior.</p>
      <h2>Admin auth environment overrides</h2>
      <pre><code>ADMIN_AUTH_ENABLED=true
ADMIN_AUTH_API_KEY=your-admin-key
ADMIN_AUTH_TOTP_SECRET=BASE32_SECRET
ADMIN_AUTH_SESSION_TTL_SECONDS=43200
ADMIN_AUTH_SECURE_COOKIES=true</code></pre>
      <h2>Security notes</h2>
      <div class="docs-note"><strong>Dashboard and client credentials are separate</strong><p>When <code>admin_auth.api_key</code> or <code>ADMIN_AUTH_API_KEY</code> is configured, dashboard login requires it in addition to TOTP. It never falls back to <code>proxy_api_key</code>.</p></div>
      <div class="docs-note warning"><strong>Public binding is a separate deployment decision</strong><p>Do not change <code>listen</code> to <code>0.0.0.0:8319</code> until dashboard authentication is enabled, client keys are protected, and the proxy in front of the gateway sanitizes forwarded headers.</p></div>
      <div class="docs-note warning"><strong>Use secure cookies behind HTTPS</strong><p>Set <code>ADMIN_AUTH_SECURE_COOKIES=true</code> when the dashboard is served through HTTPS.</p></div>
    `,
  },
  {
    slug: "api-keys",
    title: "API keys",
    group: "Reference",
    type: "Reference",
    appliesTo: "Client API access",
    introduced: docsVersion,
    updated,
    categories: ["Reference", "API access", "Limits"],
    keywords: "api keys managed scopes provider account estimated input tokens per request input token budget custom aliases access rules",
    summary: "Create managed keys with scopes, input caps, and durable first-use daily, weekly, or monthly token/request quotas.",
    seeAlso: ["dashboard", "routing-and-models", "usage-and-quota"],
    body: `
      <p class="docs-lead">Managed API keys restrict clients by provider and account. Add per-request input caps, legacy input budgets, or independent renewable request and token quotas. Rules and usage are persisted in SQLite across restarts.</p>
      ${docsFigure("docs-api-key-limits.png", "API key settings showing whole-key, provider-level, and account-level input-token controls.", "Managed keys combine provider/account access with per-request input caps before upstream dispatch.")}
      <h2>Access model</h2>
      <ul class="docs-list">
        <li><strong>Provider</strong> Allow one or more upstream providers such as Claude, Gemini, or Codex.</li>
        <li><strong>Account</strong> Within an allowed provider, allow every account or selected accounts only.</li>
        <li><strong>Custom models</strong> A <code>ctm:</code> request is authorized against each resolved target's provider and account. There is no model-name or alias-specific allow-list.</li>
      </ul>
      <h2>Create a managed key</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Open API Keys in the dashboard.</strong><p>Use an authenticated operator session.</p></div></li>
        <li><span>2</span><div><strong>Choose route scope.</strong><p>Select providers and, for each provider, every account or selected accounts.</p></div></li>
        <li><span>3</span><div><strong>Set input controls.</strong><p>Set a per-request input cap at whole-key, provider, or account level; optionally set a whole-key cumulative input-token budget.</p></div></li>
        <li><span>4</span><div><strong>Test with the client key.</strong><p>Send a request with the generated key and verify usage history.</p></div></li>
      </ol>
      <h2>Input controls</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Limit</th><th>Meaning</th></tr></thead><tbody>
        <tr><td><code>max_estimated_input_tokens_per_request</code></td><td>A positive maximum for estimated input tokens in one request. Set it at whole-key, provider, or account level; the smallest matching cap applies. Omit it for no per-request cap.</td></tr>
        <tr><td><code>prompt_token_limit</code></td><td>Deprecated input alias for <code>max_estimated_input_tokens_per_request</code>, retained for migration.</td></tr>
        <tr><td><code>input_token_budget</code></td><td>Optional whole-key cumulative input-token control: <code>{"limit": positive_integer, "period": "lifetime" | "calendar_month"}</code>. It does not cap output tokens or monetary cost.</td></tr>
        <tr><td>Scope allow-list</td><td>Providers and accounts this key can call; it is not a model or alias allow-list.</td></tr>
      </tbody></table></div>
      <p>A per-request cap is a guardrail, not a spending counter. Use <code>input_token_budget</code> when cumulative input-token usage needs a lifetime or calendar-month boundary.</p>
      <p>Neither legacy input control is a request-count quota. Cap-only keys can make repeated requests that each fit their cap. Legacy budget estimates are conservative, not exact provider billing counts; each upstream retry or fallback needs its own reservation, and settlement retains at least that amount.</p>
      <h2>Renewable request and token quotas</h2>
      <p>Set <code>access.quota</code> to a <code>timezone</code> (default <code>UTC</code>) and a <code>rules</code> array. Each rule contains <code>metric</code>, <code>period</code>, and a positive integer <code>limit</code>. Periods are <code>daily</code>, <code>weekly</code>, or <code>monthly</code>. Multiple rules apply atomically.</p>
      <pre><code>{"all":true,"quota":{"timezone":"Asia/Jakarta","rules":[{"metric":"requests","period":"daily","limit":100},{"metric":"input_tokens","period":"weekly","limit":100000},{"metric":"output_tokens","period":"monthly","limit":50000}]}}</code></pre>
      <p>Metrics: <code>requests</code>, <code>input_tokens</code>, <code>uncached_input_tokens</code>, <code>output_tokens</code>, <code>cache_read_tokens</code>, <code>cache_write_tokens</code>, and <code>cache_tokens</code>. Input includes cached input; output includes reasoning. Do not add those subsets again to token totals. Missing provider usage is unknown, not zero.</p>
      <p>The first accepted generation starts the common anchor. Daily is 24 hours; weekly is 168 hours; monthly follows the original calendar date/time in the chosen timezone, clamping short months. January 31 renews February 28/29 then March 31. Creation, rejected requests, catalog queries, inactivity, and server restarts do not reset usage or move an established anchor.</p>
      <p>One admitted client generation consumes one request; retries/fallbacks consume additional tokens, not another logical request. All dimensions are reserved before upstream dispatch and settled to trustworthy actual usage. Interrupted or missing reports retain uncertain conservative charges. Requests-per-day is not a burst/concurrency rate limiter.</p>
      <p>Use <strong>Add quota rule</strong> in the dashboard. Inspect confirmed, reserved, uncertain, and remaining amounts through <code>GET /admin/api-keys/quotas?id=KEY_ID</code> or <code>iogw keys quotas --id KEY_ID</code>. Exhaustion returns protocol-native <code>429 quota_exceeded</code> with renewal/retry headers. Ordinary allowance edits preserve balances; activated schedule changes are rejected instead of silently resetting them.</p>
      <p>Dashboard balances refresh after managed-key Test API calls and every 10 seconds while Settings is open. These read-only refreshes preserve unsaved policy edits and account selections. If a per-key accounting summary fails, <code>/admin/api-keys/quotas</code> returns <code>503</code> with <code>ok: false</code>, retaining available summaries and an <code>{"error":true}</code> entry for each failed key. Unavailable accounting never means zero usage or a renewed allowance.</p>
      <p>Renewable quota limits support positive signed-64-bit integers through <code>9223372036854775807</code>. The dashboard safely edits integers only through <code>9007199254740991</code>; it refuses to edit a policy with larger limits instead of rounding or removing them. Use <code>iogw keys update KEY_ID --access-json @access.json</code> or the admin API for those policies, and <code>iogw keys quotas --id KEY_ID</code> or the API for exact large balances.</p>
      <p>Admin create/update requests reject unknown fields in the outer payload and access, provider, account-limit, input-budget, and quota objects. Typos such as <code>acess</code> or <code>quotas</code> cannot silently create an unlimited key. The canonical cap name and deprecated <code>prompt_token_limit</code> alias remain accepted. An empty create payload deliberately creates an unrestricted key; updates require an <code>access</code> object. Persisted legacy metadata remains compatible with migration.</p>
      <div class="docs-note"><strong>Strict provider capability checks</strong><p>Set a small explicit output maximum for output quotas. A provider/model without a defensible bound is rejected before dispatch with <code>quota_measurement_required</code>; Codex, Grok, and native MiniMax Responses strict output quotas are currently unsupported. Unknown usage can reduce usable allowance conservatively. Request-only quotas do not require token measurement.</p></div>
      <h2>Restart-safe accounting</h2>
      <p>Managed key authority and quota ledgers live in <code>&lt;auth_dir&gt;/api-key-policy.sqlite3</code>. Legacy JSON is imported with a private recovery backup and an old-binary migration sentinel. Preserve the database, its identity file, and the persistent local volume. Missing or damaged established accounting fails closed; a legacy budget whose accounting database is missing cannot be migrated to a fresh allowance. Pin an explicit persistent <code>auth_dir</code> before upgrading.</p>
      <pre><code>io-gateway --config /absolute/config.json --backup-policy /backups/new-policy-snapshot</code></pre>
      <p>This command creates a consistent private SQLite snapshot with matching identity metadata and exits. The destination must be new; provider credentials, configuration and reporting history need separate backups. Copying just a live main database can lose committed WAL data. Restoring an old backup requires reconciling the missing usage interval.</p>
      <p>Keys with input controls reject unmeasurable media, encrypted or retained context, and provider-hosted retrieval or execution tools with <code>400 prompt_measurement_required</code>. Ordinary function definitions and JSON schemas remain measurable. Policy history includes dispatches and outcomes for cap-only keys as well as budgeted keys.</p>
      <h2>Operational advice</h2>
      <div class="docs-note"><strong>Prefer managed keys for clients</strong><p>Use managed keys instead of the broad shared proxy key when exposing IO Gateway to applications or users.</p></div>
    `,
  },
  {
    slug: "usage-and-quota",
    title: "Usage and quota",
    group: "Reference",
    type: "Reference",
    appliesTo: "Usage history and quota checks",
    introduced: docsVersion,
    updated,
    categories: ["Reference", "Limits", "Accounts"],
    keywords: "usage quota history context tokens account health endpoints dashboard json account-routing",
    summary: "Read usage history, context-token charts, account health, provider quota, and useful admin endpoints.",
    seeAlso: ["priority-routing", "api-keys", "troubleshooting"],
    body: `
      <p class="docs-lead">Usage and quota views show which clients, providers, models, and upstream accounts are consuming prompt tokens and how routing choices affect account health.</p>
      ${docsFigure("docs-usage-quota.png", "Context usage chart showing input, output, cache, and reasoning token trends across a day.", "Usage views show prompt-token pressure over time before you inspect provider and account details.")}
      <h2>What to watch</h2>
      <div class="docs-grid">
        <article><h3>Prompt tokens</h3><p>Track client-side limits and upstream account spending.</p></article>
        <article><h3>Context size</h3><p>Watch large prompts that may trigger provider failures or client-limit rejections.</p></article>
        <article><h3>Account state</h3><p>Disabled, cooling-down, failed, and exhausted accounts are excluded from normal routing.</p></article>
        <article><h3>Route selection</h3><p>Compare expected priority or custom-model behavior against observed account usage.</p></article>
      </div>
      <h2>Useful endpoints</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Endpoint</th><th>Use</th></tr></thead><tbody>
        <tr><td><code>GET /health</code></td><td>Basic process health.</td></tr>
        <tr><td><code>GET /ready</code></td><td>Readiness check for dependencies and route serving.</td></tr>
        <tr><td><code>GET /admin/account-routing</code></td><td>Inspect priority-routing configuration.</td></tr>
        <tr><td><code>GET /api-docs/openapi.json</code></td><td>Runtime OpenAPI JSON from the gateway app.</td></tr>
      </tbody></table></div>
      <h2>Investigate unexpected usage</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Check the managed API key.</strong><p>Confirm the key allows the resolved provider and account. A custom alias is authorized through its concrete targets, not as a separate scope entry.</p></div></li>
        <li><span>2</span><div><strong>Check priority routing.</strong><p>Priority accounts should receive traffic before normal accounts while eligible.</p></div></li>
        <li><span>3</span><div><strong>Check fallback behavior.</strong><p>Provider failures can move traffic to another target in a custom model.</p></div></li>
      </ol>
    `,
  },
  {
    slug: "routing-and-models",
    title: "Routing and models",
    group: "Explanation",
    type: "Explanation",
    appliesTo: "Model routing",
    introduced: docsVersion,
    updated,
    categories: ["Explanation", "Routing"],
    keywords: "routing models v1 responses chat completions claude messages prefix ctm alias failover",
    summary: "Understand gateway endpoints, model prefixes, custom aliases, failover, and routing rules.",
    seeAlso: ["custom-models", "priority-routing", "api-keys"],
    body: `
      <p class="docs-lead">A request crosses several deliberate boundaries before it reaches an upstream account: client key, requested model or alias, account eligibility, priority policy, and fallback. This page makes that route inspectable.</p>
      <h2>Choose the client surface</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Client need</th><th>Gateway route</th></tr></thead><tbody>
        <tr><td>Discover models available to the caller</td><td><code>GET /v1/models</code></td></tr>
        <tr><td>OpenAI Responses API</td><td><code>POST /v1/responses</code></td></tr>
        <tr><td>OpenAI Chat Completions API</td><td><code>POST /v1/chat/completions</code></td></tr>
        <tr><td>Anthropic Messages API</td><td><code>POST /claude/v1/messages</code> or <code>POST /claude/messages</code></td></tr>
        <tr><td>Codex-path catalog and responses</td><td><code>GET /codex/models</code> and <code>POST /codex/responses</code></td></tr>
      </tbody></table></div>
      <p>Clients stay on one gateway base URL. Change the <code>model</code> field to select an upstream provider; do not invent a separate URL for every provider.</p>
      <h2>Selection order</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Accept the client key.</strong><p>The request must pass the shared gateway key or the managed API-key rules before any provider is contacted.</p></div></li>
        <li><span>2</span><div><strong>Resolve the requested model.</strong><p>A natural model name selects its provider, a three-letter prefix forces one, and <code>ctm:</code> resolves a custom alias.</p></div></li>
        <li><span>3</span><div><strong>Apply the boundary.</strong><p>Managed-key provider/account rules and input controls are applied to each concrete route target. Aliases are routing policies, not independent key-scope entries.</p></div></li>
        <li><span>4</span><div><strong>Build an eligible account pool.</strong><p>Disabled, failed, cooling-down, and quota-exhausted accounts are excluded before selection.</p></div></li>
        <li><span>5</span><div><strong>Use priority where present.</strong><p>Eligible priority accounts are selected before the normal pool for that provider.</p></div></li>
        <li><span>6</span><div><strong>Fail over and leave evidence.</strong><p>A custom alias may try another target; usage history records the resulting route so the decision can be checked.</p></div></li>
      </ol>
      <h2>Force a provider when the name is ambiguous</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Prefix</th><th>Provider</th></tr></thead><tbody>
        <tr><td><code>cod:</code></td><td>Codex / OpenAI</td></tr>
        <tr><td><code>cld:</code></td><td>Claude</td></tr>
        <tr><td><code>gem:</code></td><td>Gemini</td></tr>
        <tr><td><code>agw:</code></td><td>Antigravity</td></tr>
        <tr><td><code>qwn:</code></td><td>Qwen</td></tr>
        <tr><td><code>dsk:</code></td><td>DeepSeek</td></tr>
        <tr><td><code>min:</code></td><td>MiniMax</td></tr>
        <tr><td><code>grk:</code></td><td>Grok</td></tr>
        <tr><td><code>cop:</code></td><td>GitHub Copilot</td></tr>
        <tr><td><code>glm:</code></td><td>GLM / Z.AI</td></tr>
        <tr><td><code>ctm:</code></td><td>A custom model alias</td></tr>
      </tbody></table></div>
      <p>For example, <code>gem:gemini-2.5-pro</code> forces Gemini even when another provider could recognize a similar model name. Use a <code>ctm:</code> alias when clients should see a stable name while you manage targets and fallbacks behind it.</p>
      <h2>Choose the right control</h2>
      <p>Use <a class="docs-inline-link" href="/docs/priority-routing/">priority routing</a> for temporary account draining. Use <a class="docs-inline-link" href="/docs/custom-models/">custom models</a> for stable client-facing aliases and cross-provider failover. Use <a class="docs-inline-link" href="/docs/api-keys/">API keys</a> for tenant or client access boundaries.</p>
    `,
  },
  {
    slug: "deployment",
    title: "Deployment",
    group: "Operations",
    type: "Operations",
    appliesTo: "GitHub tag releases, production deployment, and GitHub Pages",
    introduced: docsVersion,
    updated: "September 1, 2026",
    categories: ["Operations", "Deployment"],
    keywords: "deployment release tags github actions pages health ready artifact systemd ci cd",
    summary: "Release with tag-triggered app deployment and push-triggered GitHub Pages deployment.",
    seeAlso: ["configuration", "troubleshooting", "quick-start"],
    body: `
      <p class="docs-lead">A version tag builds native release archives, verifies the application build, publishes downloadable assets and installers to GitHub Releases, and deploys the Linux production binary when application code changed. Static product pages deploy separately.</p>
      <h2>Pipeline triggers</h2>
      <ul class="docs-list">
        <li><strong>Tag release</strong> Pushing a <code>v*</code> tag packages every supported platform and creates or updates the matching GitHub Release.</li>
        <li><strong>Production deployment</strong> When the tagged diff includes an application path, the GitHub-built Linux x86_64 binary is deployed and <code>io-gateway.service</code> is restarted. A documentation-only tag still publishes release assets but skips production deployment.</li>
        <li><strong>Pages deployment</strong> Pushes to <code>master</code> or <code>main</code> deploy GitHub Pages when <code>site/</code> or the Pages workflow changed.</li>
        <li><strong>Manual dispatch</strong> A manual run can validate and deploy the current checkout, but publishing a GitHub Release requires a pushed version tag.</li>
      </ul>
      <h2>Published release assets</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Platform</th><th>Release asset</th></tr></thead><tbody>
        <tr><td>Linux x86_64</td><td><code>io-gateway-&lt;tag&gt;-linux-x86_64.tar.gz</code></td></tr>
        <tr><td>Linux ARM64</td><td><code>io-gateway-&lt;tag&gt;-linux-aarch64.tar.gz</code></td></tr>
        <tr><td>macOS Intel</td><td><code>io-gateway-&lt;tag&gt;-macos-x86_64.tar.gz</code></td></tr>
        <tr><td>macOS Apple Silicon</td><td><code>io-gateway-&lt;tag&gt;-macos-aarch64.tar.gz</code></td></tr>
        <tr><td>Windows x86_64</td><td><code>io-gateway-&lt;tag&gt;-windows-x86_64.zip</code></td></tr>
        <tr><td>Windows ARM64</td><td><code>io-gateway-&lt;tag&gt;-windows-aarch64.zip</code></td></tr>
      </tbody></table></div>
      <p>Every release also includes <code>SHA256SUMS</code>, <code>install.sh</code>, and <code>install.ps1</code>. The checksum file covers every archive and both installers. Each archive contains <code>io-gateway</code>, <code>iogw</code>, and <code>config.example.json</code> (with <code>.exe</code> names on Windows).</p>
      <p>The installer URLs use GitHub’s <code>releases/latest/download/</code> endpoint. They become available after a version tag has completed the release workflow successfully.</p>
      <h2>Tag release flow</h2>
      <ol class="docs-steps">
        <li><span>1</span><div><strong>Push the finished commit and a version tag.</strong><p>For example: <code>git tag vX.Y.Z</code>, then <code>git push origin vX.Y.Z</code>.</p></div></li>
        <li><span>2</span><div><strong>Validate the application.</strong><p>When application paths changed, the workflow checks formatting, dashboard JavaScript syntax, tests, and the Linux release build.</p></div></li>
        <li><span>3</span><div><strong>Build six native archives.</strong><p>GitHub Actions builds Linux x86_64/ARM64, macOS Intel/Apple Silicon, and Windows x86_64/ARM64 binaries on their matching runners.</p></div></li>
        <li><span>4</span><div><strong>Publish the GitHub Release.</strong><p>The workflow collects the archives, installers, and <code>SHA256SUMS</code>, then creates the tag’s Release or replaces matching assets on an existing Release.</p></div></li>
        <li><span>5</span><div><strong>Deploy production when applicable.</strong><p>The server receives the GitHub-built Linux artifact rather than a binary from a local checkout. The deployment keeps the previous binary as a backup, installs the new version, restarts <code>io-gateway.service</code>, and checks readiness.</p></div></li>
      </ol>
      <h2>Production checks</h2>
      <pre><code>curl http://127.0.0.1:8319/health
curl http://127.0.0.1:8319/ready</code></pre>
      <h2>Rollback</h2>
      <p>Keep the previous binary during deployment. If readiness fails after restart, restore the previous binary, restart the service, and check <a class="docs-inline-link" href="/docs/troubleshooting/">troubleshooting</a> before creating a new tag.</p>
    `,
  },
  {
    slug: "troubleshooting",
    title: "Troubleshooting",
    group: "Troubleshooting",
    type: "Troubleshooting",
    appliesTo: "Operational diagnosis",
    introduced: docsVersion,
    updated,
    categories: ["Troubleshooting", "Configuration"],
    keywords: "troubleshooting ready 401 custom model provider auth logs quota access rules notifications",
    summary: "Diagnose readiness failures, 401s, provider auth errors, custom-model misses, and notification failures.",
    seeAlso: ["usage-and-quota", "configuration", "deployment"],
    body: `
      <p class="docs-lead">Use this page when IO Gateway starts but requests, dashboard actions, provider accounts, or release checks do not behave as expected.</p>
      <h2>Diagnostic order</h2>
      <ol class="docs-steps compact">
        <li><span>1</span><div><strong>Check process health.</strong><p>Start with <code>/health</code>, <code>/ready</code>, and service logs.</p></div></li>
        <li><span>2</span><div><strong>Check authentication.</strong><p>Separate dashboard auth failures from client API-key failures.</p></div></li>
        <li><span>3</span><div><strong>Check provider accounts.</strong><p>Look for disabled, expired, cooling-down, failed, or quota-exhausted accounts.</p></div></li>
        <li><span>4</span><div><strong>Check routing rules.</strong><p>Inspect custom models, managed-key scopes, priority routing, and fallback behavior.</p></div></li>
      </ol>
      <h2>Common symptoms</h2>
      <div class="docs-table-wrap"><table class="docs-table"><thead><tr><th>Symptom</th><th>Likely check</th></tr></thead><tbody>
        <tr><td><code>401</code> from client request</td><td>Wrong shared proxy key or managed API key.</td></tr>
        <tr><td>Dashboard login fails</td><td>Admin auth key, TOTP secret, cookie security, or session settings.</td></tr>
        <tr><td>Model alias not found</td><td>Custom model name, <code>ctm:</code> prefix, or saved alias state.</td></tr>
        <tr><td>Unexpected account used</td><td>Priority routing, account health, key scope, or custom-model fallback.</td></tr>
        <tr><td>Notification missing</td><td>Destination token/webhook, outbound network access, or dashboard notification settings.</td></tr>
      </tbody></table></div>
      <h2>Useful commands</h2>
      <pre><code>curl http://127.0.0.1:8319/health
curl http://127.0.0.1:8319/ready
curl http://127.0.0.1:8319/api-docs/openapi.json</code></pre>
    `,
  },
];

const journeys = [
  {
    number: "01",
    title: "Bring online",
    description: "Install the gateway, keep its first listener local, and make readiness a fact before any client depends on it.",
    pageSlugs: ["quick-start", "configuration"],
  },
  {
    number: "02",
    title: "Connect capacity",
    description: "Add provider accounts, understand their state, and use the dashboard as the place to see what can serve traffic.",
    pageSlugs: ["provider-accounts", "codex-reset-credit-automation", "dashboard"],
  },
  {
    number: "03",
    title: "Set route policy",
    description: "Draw client boundaries, choose model behavior, and decide when an account or fallback should be preferred.",
    pageSlugs: ["api-keys", "routing-and-models", "priority-routing", "custom-models"],
  },
  {
    number: "04",
    title: "Prove and observe",
    description: "Send a real client request, test a route deliberately, and read the account and quota evidence it leaves behind.",
    pageSlugs: ["first-client-request", "test-api", "usage-and-quota", "notifications"],
  },
  {
    number: "05",
    title: "Repair and release",
    description: "Work from the symptom outward, then use the release process only after the service is known to be healthy.",
    pageSlugs: ["troubleshooting", "deployment"],
  },
];

const topicCategories = [
  { title: "Routing", slug: "routing", description: "Priority routing, custom models, model prefixes, fallback, and account selection." },
  { title: "Accounts", slug: "accounts", description: "Upstream provider credentials, account health, quota use, reset credits, and priority membership." },
  { title: "Limits", slug: "limits", description: "Per-request input caps, input-token budgets, usage history, quota views, and managed API-key controls." },
  { title: "Deployment", slug: "deployment", description: "Release tags, production service updates, GitHub Pages deployment, and rollback." },
  { title: "Notifications", slug: "notifications", description: "Telegram and Google Chat alerts for provider, quota, and operational events." },
  { title: "Configuration", slug: "configuration", description: "Config files, environment overrides, dashboard auth, and proxy safety." },
  { title: "API access", slug: "api-access", description: "Managed API keys, Test API, runtime API docs, and client-facing model access." },
];

const categoryDefinitions = [
  ...groups.map((group) => ({ ...group, kind: "Documentation type", match: (page) => page.group === group.title })),
  ...topicCategories.map((category) => ({ ...category, kind: "Topic", match: (page) => page.categories.includes(category.title) })),
];

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function docsFigure(fileName, alt, caption) {
  const src = `/assets/${fileName}?v=${assetVersion}`;
  const [width, height] = docsFigureDimensions[fileName] || [];
  const dimensions = width && height ? ` width="${width}" height="${height}"` : "";
  return `
      <figure class="docs-figure">
        <div class="docs-figure-frame">
          <img src="${escapeHtml(src)}" alt="${escapeHtml(alt)}" loading="lazy"${dimensions}>
        </div>
        <figcaption>${escapeHtml(caption)}</figcaption>
      </figure>`;
}

function plainText(value) {
  return String(value)
    .replace(/<script[\s\S]*?<\/script>/gi, " ")
    .replace(/<style[\s\S]*?<\/style>/gi, " ")
    .replace(/<[^>]+>/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

function pageUrl(page) {
  return `/docs/${page.slug}/`;
}

function categoryUrl(category) {
  return `/docs/category/${category.slug}/`;
}

function renderHeader(active = "docs") {
  return `
    <a class="skip-link" href="#docs-content">Skip to docs content</a>
    <header class="site-header">
      <div class="site-header-inner">
        <a class="brand" href="/" aria-label="IO Gateway home">
          <img class="brand-signal" src="/brand-mark.svg?v=${assetVersion}" alt="">
          <span>IO Gateway</span>
        </a>
        <span class="docs-header-kicker"><i aria-hidden="true"></i>Documentation</span>
        <div class="site-header-actions">
          <nav aria-label="Site navigation">
            <a href="/">Home</a>
            <a href="/docs/"${active === "docs" ? ' aria-current="page"' : ""}>Docs</a>
            <a href="https://github.com/giofahreza/io-gateway">GitHub</a>
            <a class="nav-action" href="https://github.com/giofahreza/io-gateway/releases">Releases</a>
          </nav>
          <button class="theme-toggle" type="button" data-theme-toggle aria-pressed="false" aria-label="Switch theme">
            <span class="theme-toggle-led" aria-hidden="true"></span>
            <span data-theme-label>Dark</span>
          </button>
        </div>
      </div>
    </header>`;
}

function renderSidebar(activeSlug = "") {
  const journeysHtml = journeys
    .map((journey) => {
      const journeyPages = journey.pageSlugs
        .map((slug) => pages.find((page) => page.slug === slug))
        .filter(Boolean);
      return `
            <div class="docs-nav-group docs-journey-nav">
              <p class="docs-toc-label docs-journey-label"><span>${escapeHtml(journey.number)}</span>${escapeHtml(journey.title)}</p>
              ${journeyPages.map((page) => renderSidebarLink(page, activeSlug)).join("\n")}
            </div>`;
    })
    .join("\n");

  return `
        <aside class="docs-toc" aria-label="Documentation navigation">
          <button class="docs-nav-toggle" type="button" aria-expanded="false" aria-controls="docs-nav-panel">
            <span>Browse routebook</span>
            <span aria-hidden="true">+</span>
          </button>
          <div id="docs-nav-panel" class="docs-toc-inner">
            <label class="docs-search" for="docs-search">
              <span class="docs-search-label">Find a task <kbd aria-hidden="true">/</kbd></span>
              <input id="docs-search" type="search" autocomplete="off" placeholder="Search the routebook" aria-describedby="docs-search-help">
            </label>
            <p id="docs-search-help" class="docs-search-hint">Search setup, scope, routing, or recovery.</p>
            <div id="docs-search-results" class="docs-search-results" aria-live="polite"></div>
            <a class="docs-home-link${activeSlug === "" ? " is-active" : ""}" href="/docs/"${activeSlug === "" ? ' aria-current="page"' : ""}>Routebook index</a>
            ${journeysHtml}
          </div>
        </aside>`;
}

function renderSidebarLink(page, activeSlug) {
  const active = page.slug === activeSlug;
  return `
              <a href="${pageUrl(page)}" data-title="${escapeHtml(page.title)}" data-category="${escapeHtml(page.group)}" data-summary="${escapeHtml(page.summary)}" data-keywords="${escapeHtml(page.keywords)}"${active ? ' aria-current="page" class="is-active"' : ""}>
                <span>${escapeHtml(page.title)}</span>
                <small>${escapeHtml(page.summary)}</small>
              </a>`;
}

function renderMeta(page) {
  const rows = [
    ["Use when", page.appliesTo],
    page.storage ? ["Storage", `<code>${escapeHtml(page.storage)}</code>`] : null,
    ["Updated", page.updated],
  ].filter(Boolean);

  return `<dl class="docs-meta">${rows.map(([dt, dd]) => `<div><dt>${escapeHtml(dt)}</dt><dd>${dd}</dd></div>`).join("")}</dl>`;
}

function renderArticle(page) {
  return `
        <article id="docs-content" class="docs-content docs-article" data-page-slug="${escapeHtml(page.slug)}">
          <div class="docs-heading">
            <p class="eyebrow">${escapeHtml(page.group)}</p>
            <h1>${escapeHtml(page.title)}</h1>
            ${renderMeta(page)}
          </div>
          ${page.body}
          ${renderSeeAlso(page)}
        </article>`;
}

function renderSeeAlso(page) {
  if (!page.seeAlso?.length) return "";
  const links = page.seeAlso
    .map((slug) => pages.find((item) => item.slug === slug))
    .filter(Boolean)
    .map((item) => `<li><a href="${pageUrl(item)}">${escapeHtml(item.title)}</a> - ${escapeHtml(item.summary)}</li>`)
    .join("\n");

  return `
          <section class="docs-related" aria-labelledby="continue-the-route">
            <h2 id="continue-the-route">Continue the route</h2>
            <ul class="docs-link-list">
              ${links}
            </ul>
          </section>`;
}

function renderRightOutline() {
  return `
        <aside class="docs-on-page" aria-label="On this page">
          <p class="docs-toc-label">On this page</p>
          <nav id="on-this-page"></nav>
        </aside>`;
}

function renderShell({ title, description, canonicalPath, activeSlug = "", article }) {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta name="color-scheme" content="light dark">
    <meta name="theme-color" content="#080d18" data-theme-color-light="#18375f" data-theme-color-dark="#080d18">
    <meta name="description" content="${escapeHtml(description)}">
    <title>${escapeHtml(title)}</title>
    <link rel="manifest" href="/manifest.webmanifest">
    <link rel="icon" href="/icon.svg" type="image/svg+xml">
    <link rel="canonical" href="https://gateway.giofahreza.com${canonicalPath}">
    <link rel="preload" href="/brand-loader.svg?v=${assetVersion}" as="image" type="image/svg+xml">
    <link rel="preload" href="/assets/fonts/ibm-plex-sans-latin.woff2?v=${assetVersion}" as="font" type="font/woff2" crossorigin>
    <link rel="preload" href="/assets/fonts/ibm-plex-mono-latin.woff2?v=${assetVersion}" as="font" type="font/woff2" crossorigin>
    <script src="/theme.js?v=${assetVersion}"></script>
    <link rel="stylesheet" href="/brand.css?v=${assetVersion}">
    <link rel="stylesheet" href="/docs.css?v=${assetVersion}">
  </head>
  <body>
    <div class="brand-loader" data-brand-loader role="status" aria-live="polite" aria-label="Opening IO Gateway">
      <div class="brand-loader__inner">
        <img class="brand-loader__mark" src="/brand-loader.svg?v=${assetVersion}" alt="">
        <span class="brand-loader__label">Opening IO Gateway</span>
      </div>
    </div>
${renderHeader("docs")}
    <main class="docs-page-shell">
      <div class="docs-layout">
${renderSidebar(activeSlug)}
${article}
${renderRightOutline()}
      </div>
    </main>
    <footer class="site-footer">
      <div class="site-footer-inner">
        <a class="site-footer-brand" href="/" aria-label="IO Gateway home">
          <img class="brand-signal" src="/brand-mark.svg?v=${assetVersion}" alt="">
          <span><strong>IO Gateway</strong><small>Operator routebook</small></span>
        </a>
        <p>Keep provider credentials on the host. Keep client routes deliberate.</p>
        <nav class="site-footer-links" aria-label="Footer navigation">
          <a href="/">Product</a>
          <a href="https://github.com/giofahreza/io-gateway">GitHub <span aria-hidden="true">↗</span></a>
        </nav>
      </div>
    </footer>
    <script src="/docs.js?v=${assetVersion}" defer></script>
  </body>
</html>
`;
}

function renderMainPage() {
  const article = `
        <article id="docs-content" class="docs-content docs-article docs-index-page" data-page-slug="">
          <div class="docs-heading">
            <p class="eyebrow">IO Gateway documentation</p>
            <h1>Routebook</h1>
            <dl class="docs-meta">
              <div><dt>Scope</dt><dd>Client traffic and provider operations</dd></div>
              <div><dt>Updated</dt><dd>${updated}</dd></div>
            </dl>
          </div>
          <p class="docs-lead">A working guide for the point where client requests meet provider accounts. Start with the request path, then take the journey that matches the work in front of you.</p>
          <section class="docs-routebook" aria-labelledby="request-path">
            <div class="docs-routebook-intro">
              <p class="docs-routebook-label">The request path</p>
              <h2 id="request-path">Know where a request can change.</h2>
              <p>IO Gateway is not a provider catalog. It is the decision point between a client’s credentials, the route it asks for, and the accounts allowed to carry that request.</p>
            </div>
            <ol class="docs-request-path">
              <li><span>01</span><a href="/docs/first-client-request/"><strong>Client</strong><small>base URL + bearer key</small></a></li>
              <li><span>02</span><a href="/docs/api-keys/"><strong>Key scope</strong><small>what this client may use</small></a></li>
              <li><span>03</span><a href="/docs/routing-and-models/"><strong>Model / <code>ctm:</code></strong><small>provider choice or alias policy</small></a></li>
              <li><span>04</span><a href="/docs/provider-accounts/"><strong>Eligible account</strong><small>health, quota, priority</small></a></li>
              <li><span>05</span><a href="/docs/usage-and-quota/"><strong>Receipt</strong><small>usage and route evidence</small></a></li>
            </ol>
          </section>
          <section class="docs-journeys" aria-labelledby="choose-your-work">
            <p class="docs-routebook-label">Operator journeys</p>
            <h2 id="choose-your-work">Choose the work in front of you.</h2>
            <p>Each route begins with an action and ends with something you can verify. Open the page closest to the decision you need to make, not a generic feature bucket.</p>
            <ol class="docs-journey-list">
              ${journeys.map((journey) => renderJourneyBlock(journey)).join("\n")}
            </ol>
          </section>
          <section class="docs-routebook-footer" aria-labelledby="runtime-reference">
            <h2 id="runtime-reference">Need the live API surface?</h2>
            <p>The running gateway publishes its own runtime API reference at <code>/docs/</code> and <code>/api-docs/openapi.json</code> on the gateway base URL. Use the routebook for operator intent; use the runtime reference for the exact request shape your installed version serves.</p>
          </section>
        </article>`;

  return renderShell({
    title: "Routebook - IO Gateway Docs",
    description: "The IO Gateway routebook for bringing a gateway online, connecting accounts, setting route policy, proving client traffic, and recovering service.",
    canonicalPath: "/docs/",
    activeSlug: "",
    article,
  });
}

function renderJourneyBlock(journey) {
  const journeyPages = journey.pageSlugs
    .map((slug) => pages.find((page) => page.slug === slug))
    .filter(Boolean);

  return `
              <li>
                <span class="docs-journey-number">${escapeHtml(journey.number)}</span>
                <div class="docs-journey-copy">
                  <h3>${escapeHtml(journey.title)}</h3>
                  <p>${escapeHtml(journey.description)}</p>
                </div>
                <ul class="docs-journey-pages">
                  ${journeyPages.map((page) => `<li><a href="${pageUrl(page)}"><span>${escapeHtml(page.title)}</span><small>${escapeHtml(page.summary)}</small></a></li>`).join("\n")}
                </ul>
              </li>`;
}

function renderCategoryPage(category) {
  const categoryPages = pages.filter(category.match);
  const article = `
        <article id="docs-content" class="docs-content docs-article docs-category-page" data-page-slug="category-${escapeHtml(category.slug)}">
          <div class="docs-heading">
            <p class="eyebrow">${escapeHtml(category.kind)}</p>
            <h1>${escapeHtml(category.title)}</h1>
            <dl class="docs-meta">
              <div><dt>Pages</dt><dd>${categoryPages.length}</dd></div>
              <div><dt>Updated</dt><dd>${updated}</dd></div>
            </dl>
          </div>
          <p class="docs-lead">${escapeHtml(category.description)}</p>
          <section aria-labelledby="pages-in-${category.slug}">
            <h2 id="pages-in-${category.slug}">Pages in this category</h2>
            <ul class="docs-page-list">
              ${categoryPages.map((page) => `<li><a href="${pageUrl(page)}"><span>${escapeHtml(page.title)}</span><small>${escapeHtml(page.summary)}</small></a></li>`).join("\n")}
            </ul>
          </section>
          <section aria-labelledby="related-categories">
            <h2 id="related-categories">Related categories</h2>
            <ul class="docs-link-list">
              ${categoryDefinitions
                .filter((item) => item.slug !== category.slug)
                .filter((item) => categoryPages.some((page) => item.match(page)))
                .slice(0, 6)
                .map((item) => `<li><a href="${categoryUrl(item)}">${escapeHtml(item.title)}</a> - ${escapeHtml(item.description)}</li>`)
                .join("\n")}
            </ul>
          </section>
        </article>`;

  return renderShell({
    title: `${category.title} - IO Gateway Docs`,
    description: category.description,
    canonicalPath: categoryUrl(category),
    activeSlug: `category-${category.slug}`,
    article,
  });
}

function buildSearchIndex() {
  const articleRecords = pages.map((page) => ({
    title: page.title,
    category: page.group,
    type: page.type,
    summary: page.summary,
    href: pageUrl(page),
    keywords: page.keywords,
    headings: [...page.body.matchAll(/<h[23][^>]*>(.*?)<\/h[23]>/g)].map((match) => plainText(match[1])),
    excerpt: plainText(page.body).slice(0, 360),
  }));

  const categoryRecords = categoryDefinitions.map((category) => ({
    title: category.title,
    category: category.kind,
    type: "Category",
    summary: category.description,
    href: categoryUrl(category),
    keywords: `${category.title} ${category.kind}`,
    headings: ["Pages in this category", "Related categories"],
    excerpt: category.description,
  }));

  return {
    generatedAt: "2026-09-04",
    docsVersion,
    pages: [
      {
        title: "Routebook",
        category: "Product documentation",
        type: "Main page",
        summary: "IO Gateway routebook for bringing a gateway online, connecting accounts, setting route policy, proving traffic, and recovering service.",
        href: "/docs/",
        keywords: "docs documentation routebook setup account routing policy client request usage deployment recovery",
        headings: ["Know where a request can change", "Choose the work in front of you", "Need the live API surface"],
        excerpt: "A working guide for the point where client requests meet provider accounts.",
      },
      ...articleRecords,
      ...categoryRecords,
    ],
  };
}

function writeOutput(path, content) {
  const target = join("site", path);
  mkdirSync(dirname(target), { recursive: true });
  const normalized = content.replace(/[ \t]+$/gm, "").replace(/\n*$/, "\n");
  writeFileSync(target, normalized);
}

rmSync(outputDir, { recursive: true, force: true });
mkdirSync(outputDir, { recursive: true });

writeOutput("docs/index.html", renderMainPage());

pages.forEach((page) => {
  writeOutput(`docs/${page.slug}/index.html`, renderShell({
    title: `${page.title} - IO Gateway Docs`,
    description: page.summary,
    canonicalPath: pageUrl(page),
    activeSlug: page.slug,
    article: renderArticle(page),
  }));
});

categoryDefinitions.forEach((category) => {
  writeOutput(`docs/category/${category.slug}/index.html`, renderCategoryPage(category));
});

writeOutput("docs/search-index.json", `${JSON.stringify(buildSearchIndex(), null, 2)}\n`);
