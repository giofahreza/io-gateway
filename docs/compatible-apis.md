# Compatible APIs

Connect services that accept OpenAI Chat Completions, Anthropic Messages, or both. Compatible accounts have their own **Compatible APIs** section, usage tracking, priority routing, scoped gateway keys, and custom-model targets. They do not require a built-in provider connector.

## Web dashboard

1. Choose **Add account → Compatible API (OpenAI / Anthropic)**, or **+ Add** in the Compatible APIs section.
2. Enter a name and select **OpenAI compatible**, **Anthropic compatible**, or **Both formats**.
3. Enter the service's base URL and API key. Both formats share the saved key; use separate accounts if the keys differ.
4. Enter upstream model IDs, separated by commas or new lines. **Fetch models** requests the upstream model list; manual entry works for services without a model-list endpoint.
5. Save. The account card shows its formats, URLs, models, state, and usage. **Edit account** changes the name, URLs, key, or model list. Leave the key blank to retain it.

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
