# Compatible APIs

Connect services that accept OpenAI Chat Completions, Anthropic Messages, or both. Compatible accounts have their own **Compatible APIs** section, usage tracking, priority routing, scoped gateway keys, and custom-model targets. They do not require a built-in provider connector.

## Web dashboard

1. Choose **Add account → Compatible API (OpenAI / Anthropic)**, or **+ Add** in the Compatible APIs section.
2. Enter a name and select **OpenAI compatible**, **Anthropic compatible**, or **Both formats**.
3. Enter the service's base URL and API key. Both formats share the saved key; use separate accounts if the keys differ.
4. Enter upstream model IDs, separated by commas or new lines. **Fetch models** requests the upstream model list; manual entry works for services without a model-list endpoint.
5. Save. The account card matches the other providers: state and usage at the top, models and **Connection details** (URLs) below. Choose **Edit** in the card's ⋯ menu to change the name, URLs, key, or model list. Leave the key blank to retain it.

Use the account action menu to set priority, enable, disable, or delete. As with built-in providers, routing and retries use the usable priority pool when one exists. Disabling an account stops new requests from selecting it. Deleting removes its credential file.

## Terminal UI

Run `iogw`, focus **Account usage**, and press **n**. Choose **11 / Compatible API** and enter the name, either or both base URLs, API key, and comma-separated upstream model IDs. Key input is hidden.

Select an account and press **Enter** to view details, **i** to edit it, **e/d** to enable or disable, or **x** to delete. During editing, blank fields keep their saved values; enter `-` to clear a URL. A blank key retains the saved key. The existing priority and refresh controls also work.

## Base URLs and client requests

The OpenAI URL is the API root immediately before `/chat/completions`, such as `https://example.com/v1`. The Anthropic URL may end in `/v1` or name the service root; the gateway appends `/v1/messages` when needed. Full `/chat/completions` or `/v1/messages` URLs are also accepted.

For Qwen Coding Plan, for example:

```text
OpenAI:    https://token-plan.maas.qwencloudapi.com/compatible-mode/v1
Anthropic: https://token-plan.maas.qwencloudapi.com/apps/anthropic
Model:     qwen3.7-plus
```

Clients select **`cmp:qwen3.7-plus`**. The prefix chooses the compatible connector and is removed before forwarding. Models with tags, such as `cmp:qwen3:8b`, retain their upstream `qwen3:8b` ID. Model IDs are case sensitive. Accounts offering the same model participate in load balancing and failover; accounts that do not list it are never selected. To pin a model to one account, use a custom-model route with an account selector.

```sh
curl http://127.0.0.1:8319/v1/chat/completions \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"cmp:qwen3.7-plus","messages":[{"role":"user","content":"Hello"}]}'
```

OpenAI clients use `/v1/chat/completions` or `/v1/responses`. Anthropic clients set their base URL to `http://127.0.0.1:8319/claude` and use `/claude/v1/messages`. Compatible models appear in `/v1/models` and the Codex model catalog.

When both URLs are configured, each native client format uses its matching URL. With one URL, the gateway translates the other format. OpenAI Responses uses translation through Chat Completions. Native Chat Completions and Messages streams are forwarded as they arrive; Anthropic-to-Responses and other cross-format streams are emitted after collecting the upstream response.

Compatible credentials are stored privately in the configured auth directory and never returned by management endpoints. Account names and IDs remain stable through edits. Management requires the same administrator session as the rest of the dashboard and TUI. The provider name for managed-key access rules is `compatible`.

Usage is tracked from upstream token counts. Services without quota or balance APIs do not report an upstream balance. Renewable request quotas and account/provider scopes apply normally; output-token quotas require a provable upstream output bound, which arbitrary compatible services may not provide.

## Verification

```sh
cargo build --locked --bins
python3 scripts/test-compatible-apis.py
```

The integration check uses a local mock service and temporary gateway configuration. It covers management authentication, persistence, model discovery, both protocols, streaming, cross-format translation, scoped keys, request quotas, and enabling/disabling/deleting accounts without external credentials.

## Qwen Cloud Token Plan and Pay-as-you-go

Choose **Qwen Cloud** in the web **Add account** menu or TUI option **12**, then enter your API key. The account type follows the key: `sk-sp-` keys are Token Plan, and any other Qwen Cloud key (`sk-ws-`, or the older `sk-`) is Pay-as-you-go. The web editor shows the detected type and lets you override it, and the TUI asks for the key first. You can also change the account type in an existing compatible account's editor. Account IDs, usage history, `cmp:` model names and account routing remain the same. Token Plan and pay-as-you-go cards appear in the **Qwen** group in both web and TUI; only custom services appear in **Compatible APIs**. Official Qwen Cloud URLs are recognized even when an older credential was saved as custom. Selecting a Qwen preset fills the URLs automatically; changing presets replaces the previous preset URLs.

The presets use both OpenAI and Anthropic formats:

| Account | OpenAI base URL | Anthropic base URL |
| --- | --- | --- |
| Token Plan | `https://token-plan.maas.qwencloudapi.com/compatible-mode/v1` | `https://token-plan.maas.qwencloudapi.com/apps/anthropic` |
| Pay-as-you-go (Singapore) | `https://ap-southeast-1.maas.qwencloudapi.com/compatible-mode/v1` | `https://ap-southeast-1.maas.qwencloudapi.com/apps/anthropic` |

Use a plan-specific `sk-sp-` key for Token Plan and a separate pay-as-you-go key for Pay-as-you-go. The editor rejects mixing these key types. Pay-as-you-go URLs can be changed to another [Qwen Cloud region](https://docs.qwencloud.com/developer-guides/getting-started/regions).

**Optional to show quota.** Your API key works without connecting a management account.

In the web UI, choose **Connect Qwen Cloud account** in the Token Plan or Pay-as-you-go add/edit window. Cloud account cards show the connection status. This saves the API configuration and opens Qwen Cloud's official browser sign-in. Approve access using the account that owns the API key; the editor checks for completion automatically. A sign-in link remains available if your browser blocks the new tab. The editor shows the connected email and offers **Reconnect** and **Disconnect**. **Qwen Chat** in the provider menu is the separate chat-account login.

In the TUI, edit the Qwen Cloud account and choose **c** at **Connect Qwen Cloud account?**. Approve the browser sign-in, then press Enter to check completion. Choose **d** to disconnect, or Enter to keep the current connection. This works in both normal and animated TUI modes. On a headless machine, open the displayed link on another device.

The gateway implements the official Qwen Cloud CLI's device authorization flow with PKCE directly; installing Node.js or the CLI on the server is unnecessary. The management token is stored privately with the account (owner-only file permissions) and is never returned to web or TUI clients. Login sessions expire: reconnect when prompted. Disconnect removes the saved quota credentials and leaves the inference API configuration in place. Replacing the inference key clears the management connection to avoid showing a different account's quota.

Existing console Cookies still work. The web editor's **Advanced: console cookie** section accepts a Cookie header from a Qwen Cloud console request. Leave it blank to keep the saved cookie, or select **Remove saved console session** to remove it. The management connection takes precedence when both are present.

- **Token Plan Individual:** shows the usage windows and reset times actually reported by Qwen Cloud. Current Individual plans use monthly Credits. Remaining plan credits are estimates from the reported ratio and tier allowance; these estimates exclude Credit Packs and prorated upgrades. Overlapping windows are never added. Subscription credits have no currency symbol.
- **Pay-as-you-go:** shows available account balance, cash balance, currency and credit limit when the billing service reports them. Available account balance may include a credit line. The billing endpoint does not report per-model request/token rate limits; those remain unreported.
- Missing sessions, expired sessions and missing provider fields show **Not reported** with an explanation. Gateway request counters are never presented as upstream quota consumption.

Credit details appear on web account cards, TUI account details, and animated account bubbles when **all quota limits** is enabled (`a`). Quota reads use management information queries and, for Token Plan, exchange the management token for a quota-access token as the official CLI does. Results are cached for 60 seconds. They do not issue inference probes, purchases, renewals or resets. The current quota reader supports the international Qwen Cloud Individual plan and account billing; Team pools and Alibaba Cloud Model Studio/DashScope billing are separate services.

The console endpoints are private provider APIs and may change. Endpoint and plan behavior are described in Qwen Cloud's [Token Plan documentation](https://docs.qwencloud.com/token-plan/personal/token-plan-personal-overview) and [CLI authentication documentation](https://docs.qwencloud.com/api-reference/preparation/cli).
