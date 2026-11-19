# Account credits and limit resets

Codex and Claude account cards show **Available credits** independently from
**Available reset limit**. Credit balances pay for usage; reset allowances refill
specific provider limits. Neither is a gateway API-key quota.

- Codex reports its credit balance in upstream credit units. The gateway does
  not add a currency symbol or convert those units into money.
- Claude reports monetary values with their currency and decimal scale. The UI
  also shows whether usage credits are enabled, reported spending, and a spend
  cap when supplied. A cap minus spending is not presented as a prepaid balance.
- Zero, unlimited, disabled, and **Not reported** are distinct states. Disabled
  usage credits do not prove a zero account balance.

## Web dashboard

Expand **Available reset limit** on a Codex or Claude account to inspect reset
allowances. Claude grants include the remaining count, expiry, affected limits,
and current eligibility. A weekly 5-hour reset, when offered by Claude, leaves
weekly limits in place.

Choose **Reset limit** and confirm the selected grant to redeem it. Merely
viewing an account, refreshing quota, or fetching models does not redeem a reset.
Unavailable grants remain visible without a redemption button.

Claude availability comes from the OAuth usage endpoint with its reset-status
query flags. Its availability depends on the subscription, account, rollout,
and live provider response. The gateway does not manufacture reset grants or
promise every account has one.

## Terminal UI

Select a Codex or Claude account and press **Enter** for credit and reset details.
Press **z** in the account list or details, choose a listed reset number, then
confirm. Blank input cancels. The terminal schedules redemption in its background
request worker. The gateway rechecks eligibility before spending.

## io-workbench

In the native Android gateway account view, credit information appears on the
quota card and in account details. Account details expose the same reset grants
and confirmation controls. Both `/dashboard/snapshot.json` and the smaller
`/usage/mobile.json` payload retain the credit and eligibility fields.

## Claude execution and recovery

Manual Claude redemption uses authenticated administrator endpoint
`POST /claude/rate-limit-reset-credit/consume`, with form fields `file_name`,
`credit_id`, and `program`. The UI sends the exact grant ID and program reported
by the account. Reading these endpoints never triggers redemption.

The gateway reads fresh upstream status, requires an enabled, identified account,
and records the request in `<auth_dir>/claude-reset-actions.sqlite3` before the
upstream POST. It generates the grant request ID itself. Concurrent submissions
and a gateway restart cannot replay the same entitlement. A successful provider
response is checked against another fresh usage read.

A timeout, unknown result, interrupted submission, or unverified outcome blocks
further resets for that account. **Last reset** shows `submitted` or
`manual_review`; inspect Claude usage and the ledger before attempting recovery.
Submitted requests are never retried automatically, and uncertain requests block selection of another grant. Keep this SQLite
ledger on persistent local storage and include it in operational backups using
SQLite's backup API; do not copy only a live database file without its WAL.

Codex redemption continues to require its existing managed local App Server
profile. See [Codex reset-credit automation](codex-reset-credit-automation.md)
for its separate prerequisites and opt-in scheduling.

## Opt-in Claude automatic scheduling

Web: **Settings → Reset credits → Automatic Claude resets**. TUI: select a
Claude account and press **s** in the account list or details. Workbench Android:
open the account details and choose **Manage automatic resets**. All policies
start disabled. Enabling requires confirmation because it can spend allowances.

Each account has its own durable policy: scan interval (default 30 minutes),
upper expiry window (60 minutes), final check (5 minutes before expiry), and
minimum time to natural reset (10 minutes). Manage it through authenticated
`GET/POST /admin/claude/reset-credit-automation`. Disabling cancels unsubmitted
actions. Disabled or removed credentials cannot submit scheduled resets.

The worker selects one concrete, expiring Claude grant between 30 minutes and
the configured upper window before expiry. It checks fresh usage again before
submission and only spends if a reached limit is covered by that grant, with
known natural reset times beyond the configured minimum. Unknown eligibility,
nearby natural resets, and ordinary usage below the limit defer the action.
Deferred actions are checked again before expiry; expiry alone never causes a
reset. The recurring weekly 5-hour offer is available manually when reported,
but is not automatically scheduled because it lacks a concrete expiring grant.

Policies, selected grant IDs, deadlines, leases and submission results are saved
in the same SQLite ledger. Concurrent manual and automatic requests share the
account gate. After a crash or ambiguous response the scheduler never replays a
submitted reset. The web settings view shows scan status and recent actions;
account details show the last reset outcome. No production policy is enabled by
the test or deployment workflow.

## Verification without spending real allowances

```sh
cargo test --locked
cargo build --locked --bins
node scripts/test-account-credits.mjs
python3 scripts/test-claude-resets.py
python3 scripts/test-claude-resets.py --automation
```

The integration test uses temporary synthetic credentials and a local mock
provider. It checks balances, mobile data, eligibility, concurrent submissions,
restart durability, ambiguous results, and administrator authentication. It never
reads production credentials or contacts a real provider.
