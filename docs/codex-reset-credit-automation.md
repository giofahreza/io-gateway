# Codex reset-credit automation

IO Gateway can preserve a Codex usage-limit reset credit that would otherwise
expire, then redeem that exact credit only when the associated ChatGPT account
has a currently reached Codex rate-limit window. It is an opt-in, per-account
operator feature. It remains unavailable until both an administrator enables a
policy and configures the explicit local Codex App Server integration below.

This feature is intentionally conservative. It is designed to avoid a lost
credit without treating a cached dashboard response, a renamed credential, or
an ambiguous network result as authority to spend one.

## What it automates

For each enabled policy, IO Gateway:

1. Reads the account's Codex rate-limit state and reset-credit details through
   Codex App Server's documented `account/rateLimits/read` operation.
2. Looks only for an explicit, available Codex-rate-limits credit (the App
   Server form is `codexRateLimits`) with a known expiry **at least 30 minutes
   and no later than the configured upper expiry window** from the fresh read.
3. Creates a durable action for the earliest-expiring eligible credit.
4. Uses a fresh second read before redemption to confirm that a Codex limit is
   reached and that the exact credit is still available.
5. Marks the action as submitted before its upstream POST, then verifies a
   successful or idempotent response with another fresh rate-limit read.

It does not spend a credit simply because one exists. It also does not use the
dashboard quota cache to make a redemption decision.

## Local Codex App Server requirement

Reset-credit automation uses the documented Codex App Server JSON-RPC account
API, not the gateway's direct HTTP/Wham quota adapter. It starts a local,
short-lived `codex app-server --stdio` child for
`account/rateLimits/read` and `account/rateLimitResetCredit/consume`.
`upstream_base` does not configure or authorize these reset-credit operations.

Codex App Server is experimental and unsupported for production workloads.
IO Gateway therefore requires an explicit opt-in and never falls back to an
undocumented direct backend call when the App Server configuration is missing
or invalid.

Configure a trusted local executable and one managed profile for each stable
gateway account that may redeem a credit:

```json
{
  "codex_reset_credit_app_server": {
    "experimental_opt_in": true,
    "command": "/usr/local/bin/codex",
    "profile_root": "/var/lib/io-gateway/codex-app-server-profiles",
    "profiles": [
      {
        "account_key": "codex:account_id:org_123",
        "profile": "org_123",
        "expected_email": "ops@example.com"
      }
    ]
  }
}
```

- `command` is an absolute path to a trusted, executable `codex` file. The
  gateway fixes the arguments to `app-server --stdio -c
  cli_auth_credentials_store="file"`; it does not run a shell or accept
  configurable arguments. The fixed file-backed credential override prevents a
  managed profile from silently using a shared OS keyring.
- `profile_root` is an existing absolute, private directory. Each `profile` is
  an existing private child directory with a simple name made only of letters,
  digits, `_`, and `-`; it is not a path. On Unix, the root and every profile
  must deny group and other access (normally mode `0700`) and must not be a
  symlink.
- Each `account_key` and profile directory may appear only once. `account_key`
  must match the stable `codex:account_id:<id>` identity of the enabled gateway
  credential. Every enabled profile binding must provide `expected_email`, and
  it must match the email returned by App Server's `account/read` check. This
  confirms the managed profile's ChatGPT email; it does not establish or replace
  the stable upstream account identity, which remains the `account_key` mapping.
- An administrator must create each profile and complete Codex's managed
  ChatGPT login in it before enabling the policy. At every launch, the gateway
  checks `account/read`: it accepts only managed ChatGPT authentication, not an
  API key or externally supplied ChatGPT tokens.
- Each profile must contain its own private, regular `auth.json`. If it has a
  `config.toml`, that file must also be private and may not set `sqlite_home`;
  an explicit `cli_auth_credentials_store` is allowed only when it is `"file"`.
  These checks keep the managed profile from escaping to a shared credential or
  SQLite store.

The child receives a scrubbed environment with `HOME`, `CODEX_HOME`, and
`CODEX_SQLITE_HOME` pointed at that one profile. IO Gateway does not pass the
gateway's stored Codex bearer credential to App Server.

## Prerequisites and account identity

- Configure dashboard authentication before exposing the administrative
  endpoints.
- Add a Codex (ChatGPT) credential and keep it enabled.
- Complete the App Server configuration and managed ChatGPT profile setup
  above. Without it, neither automatic nor manual redemption is enabled.
- Enable automation only for a stable account key in the form
  `codex:account_id:<id>`.

The account ID must resolve to exactly one enabled Codex credential at the time
the policy is enabled. Labels, filenames, and anonymous `manual-N` identities
are deliberately rejected for unattended use because they can drift to a
different account after a reload. If duplicate enabled credentials have the
same account ID, resolve the duplicate before enabling automation.

Manual redemption uses the same coordinator and also requires an account that
can be resolved to this stable identity and a matching managed App Server
profile.

## Default policy

| Setting | Default | Meaning |
|---|---:|---|
| `scan_interval_minutes` | 30 | How often the account is scanned for a newly eligible credit. Enabling or editing an enabled policy also schedules one immediate scan. |
| `expiry_window_minutes` | 60 | The upper selection bound. A new automatic action is created only when `30 minutes <= expires_at - now <= expiry_window_minutes`. |
| `final_attempt_minutes` | 5 | For a deferred action, schedule a final pre-expiry check this many minutes before expiry when possible. |
| `min_natural_reset_remaining_minutes` | 10 | Preserve the credit when the reached ordinary rate-limit window will reset at or within this threshold. |

The background worker wakes once per minute to honor durable action deadlines.
That does not make every account a one-minute polling loop: policy scans remain
on the configured cadence. Enabling or editing an enabled policy schedules an
immediate fresh scan, and durable deadlines survive a gateway restart.

With the default 30-minute scan and 60-minute upper window, a newly discovered
credit is selected only in the intended 30–60 minute interval before expiry.
An already selected durable action may still be rechecked closer to expiry after
a deferral, retry, or temporary upstream failure; that does not make a newly
discovered credit under 30 minutes old eligible.

`scan_interval_minutes` must be 1 through 1,440. `expiry_window_minutes` must
be 30 through 10,080. `final_attempt_minutes` must be 1 through one minute
less than the expiry window. `min_natural_reset_remaining_minutes` may be 0
through 10,080; a value of `0` disables the ordinary-reset-nearby safeguard.

## Eligibility and fail-closed behavior

An automatic redemption requires all of the following:

- The upstream-reported available-credit count is positive.
- Upstream provides a concrete, nonempty credit ID and an unambiguous expiry.
- The credit has status exactly `available`, the App Server reset type exactly
  `codexRateLimits`, and has not expired.
- Its expiry is at least 30 minutes away and no later than the configured upper
  window. If several qualify, the earliest expiry is chosen.
- A fresh upstream read says a resettable Codex limit is reached.
- A fresh upstream read exposes a natural reset time beyond the configured
  nearby-reset threshold.

If upstream reports only a credit count but no concrete credit records, the
count and details do not establish the same selectable credit, an expiry is
missing, fresh state cannot be read, or a safe natural-reset time is unknown,
the automation does not spend a credit. It skips a new action or defers an
existing automatic action for a later fresh check instead. This means a
temporary upstream outage may leave a credit unused; it will never make the
gateway choose an unspecified “next” credit.

When the normal rate-limit reset is near, the action is deferred so the earned
credit is kept. A deferred action is normally retried at the earlier of the next
policy scan or `expires_at - final_attempt_minutes`; the final-attempt value is
therefore always positive. When the final deadline passes without a safe
redemption, the action becomes `expired`. A process outage lasting beyond expiry
cannot be recovered automatically.

## Configure and inspect policies

Read the available stable accounts, policies, defaults, and browser-safe action
status:

```bash
curl -sS 'http://127.0.0.1:8319/admin/codex/reset-credit-automation' \
  -b "$ADMIN_COOKIE"
```

The `accounts` entries include `account_key`, display metadata, and `eligible`.
Copy an account key that is both enabled and eligible; do not construct a key
from a label or filename.

Enable the default policy:

```bash
curl -sS -X POST 'http://127.0.0.1:8319/admin/codex/reset-credit-automation' \
  -b "$ADMIN_COOKIE" \
  -H 'Content-Type: application/json' \
  --data '{
    "account_key": "codex:account_id:org_123",
    "enabled": true,
    "scan_interval_minutes": 30,
    "expiry_window_minutes": 60,
    "final_attempt_minutes": 5,
    "min_natural_reset_remaining_minutes": 10
  }'
```

Disable it with the same stable `account_key` and `"enabled": false`. A policy
may be disabled even after its credential is disabled or removed, so an
operator can stop future automatic work. Existing actions are retained for
audit and safe reconciliation; disabling does not select a replacement credit
or allow a previously submitted action to make a new automatic spend.

The endpoint accepts only this JSON shape. Unknown fields are rejected. A
successful response omits internal idempotency keys.

## Action states

`GET /admin/codex/reset-credit-automation` returns the following safe state for
each action. It includes opaque credit IDs and timestamps, but not provider
credentials, prompts, raw request bodies, raw upstream responses, or the
idempotency key.

| State | Meaning |
|---|---|
| `pending` | A concrete credit was selected and is waiting for its first durable attempt. |
| `deferred` | The credit is retained for a later check: no reached limit, a nearby/unknown natural reset, disabled policy, or a safe retry schedule. |
| `submitted` | The exact credit and durable idempotency key were recorded before an upstream POST; retries reuse both values. |
| `verified` | Upstream reported `reset` or `alreadyRedeemed`, and a fresh read confirmed the reached rate limit cleared. |
| `no_credit` | The selected credit is no longer available. |
| `manual_review` | A response or post-consume verification was ambiguous, or a legacy direct-HTTP submission needs review. Automation stops instead of spending another credit. |
| `expired` | The selected credit expired before a safe redemption was possible. |

Network failure or a timeout after submission is deliberately not considered a
failed redemption. The action remains durable and retries the same opaque
credit ID with the same idempotency key. If post-consume verification cannot
prove that the limit cleared, it moves to `manual_review`; it never
automatically chooses a second credit.

## Manual redemption

Manual and automatic redemption share the same SQLite action ledger and the
same managed App Server profile binding. Select an exact credit shown by the
account's reset-credit data:

```bash
curl -sS -X POST 'http://127.0.0.1:8319/codex/rate-limit-reset-credit/consume' \
  -b "$ADMIN_COOKIE" \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  -d 'file_name=my-codex-account.json' \
  -d 'credit_id=credit_...'
```

The `credit_id` is required. The gateway creates and persists the idempotency
key itself before contacting upstream. Do not ask upstream to choose the next
available credit, and do not reuse or supply a client idempotency key. Manual
redemption may use a selected detailed credit without an upstream expiry, but
automatic redemption always requires a known, future expiry.

The dashboard quota view is informational and can be stale. Before the manual
endpoint consumes anything, it re-reads the selected credit and rate-limit
state through the mapped App Server profile. It refuses a missing, changed, or
ineligible App Server credit rather than treating a cached dashboard row as
authorization to spend.

A manual response can be immediately `verified`, `deferred`, `submitted`,
`no_credit`, `manual_review`, or `expired`. `submitted` means the result needs
safe recovery/verification, not that another request should be sent with a new
credit ID.

## Upgrading from the legacy direct-HTTP adapter

Existing action ledgers are migrated safely when this version starts. A legacy
direct-HTTP action that was already `submitted` is changed to `manual_review`:
its old idempotency key is never replayed through a different App Server
transport. A legacy `pending` or `deferred` action never recorded a POST, so it
is retained and revalidated through App Server before any future redemption.
Review `manual_review` actions rather than creating a second automatic spend.

## Durability, backup, and deployment

Policies, action state, attempts, opaque credit IDs, and idempotency keys are
stored in `<auth_dir>/api-key-policy.sqlite3`, alongside managed API-key policy
and quota accounting. The database uses durable SQLite transactions and leases
so a restart or another process using the same local database cannot race into
a second redemption of the same credit.

Keep `auth_dir` on persistent local storage and back it up with the gateway's
policy snapshot command:

```bash
io-gateway --config /absolute/config.json --backup-policy /backups/new-policy-snapshot
```

Preserve the database identity file and the SQLite WAL-consistent snapshot as
described in the [operator guide](operator-guide.md#durable-storage-and-migration).
Do not put the live SQLite database on NFS or another shared network filesystem.
For a multi-host deployment, use a central transactional coordinator before
enabling automatic redemption.

## Operational checks

- Review action state after enabling a policy and after an upstream limit is
  reached. `verified` is the only confirmed successful redemption.
- Treat `manual_review` as an operator task. Inspect the upstream account and
  do not create a second action merely to “try again.”
- If an account no longer appears as eligible, check whether its credential was
  disabled, removed, or duplicated under the same stable account ID.
- Keep the policy disabled unless preserving expiring earned credits is an
  intentional account-management choice. The feature does not increase quota;
  it only redeems credits ChatGPT has already issued.
