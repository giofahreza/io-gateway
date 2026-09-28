# Durable renewable API-key quotas

Status: implemented and locally verified on 2026-09-20. This document
records the implementation contract, delivered behavior, and verification evidence.

## Objective

Add independent per-key quotas for requests, total/uncached input tokens, output
tokens (including reasoning), cache reads, cache writes, and combined cache use.
Support simultaneous daily, weekly, and monthly rules. Preserve existing input
request caps and conservative lifetime/calendar-month budgets without silently
changing their meaning. A server restart must never restore spent allowance.

## Chosen defaults

- Rules are opt-in. Existing keys keep their existing policy.
- A quota policy has a common first-use anchor, a timezone, and resource/period
  rules. All applicable rules must admit a request atomically.
- First accepted generation admission starts the timer; key creation, health,
  catalog/admin calls, malformed input, and local denials do not.
- Daily means 24 hours; weekly means 168 hours. These are anchored fixed windows,
  not sliding windows or inactivity timers.
- Monthly means the same local calendar date/time, clamped to the last valid day
  of short months. Calculate every boundary from the original anchor:
  January 31 -> February 28/29 -> March 31. The configured timezone defaults to UTC.
  For DST gaps advance to the first valid local time; for overlaps use the earlier
  instant. Daily/weekly elapsed durations may shift local time in DST zones.
- Inactivity preserves the original cadence. No rollover of unused allowance.
- Window boundaries are half-open: [start, end). Renewals require no cron job.
- One client generation request counts once when admitted to dispatch. Internal
  retries count additional token attempts, not additional client requests. A new
  client retry is a new request unless a real replay facility deduplicates it.
- Failures/disconnects after possible upstream dispatch are not automatically
  refunded. Provably local failures may release their unused reservations.
- Strict quotas fail closed on unsupported/unmeasurable routes. No silent
  downgrade to an overage-permitting mode.

## Persistence and authority

Use `<absolute persistent auth_dir>/api-key-policy.sqlite3` for managed key hashes,
revocation/access state, quota rules, anchors, windows, reservations, and the
accounting ledger. Provider credentials remain in their existing files; usage
history remains a reporting projection, never the quota authority.

Maintain stable key/rule identities independently from revisions. Ordinary limit
edits preserve usage. Structural schedule/resource changes after activation must
be rejected or explicitly migrated; they must not silently reset an active plan.
Secret rotation, rule disable/re-enable, history deletion, and restarts must not
erase balances. Record administrative transitions.

Use WAL, synchronous=FULL, checked signed-64-bit arithmetic, bounded lock waits,
and short BEGIN IMMEDIATE transactions. Upgrade bundled SQLite to a release with
the WAL-reset corruption fix (3.51.3 or later, or an official fixed backport), and
test the runtime version. All participating processes must use local storage on
the same host. Multi-host deployment requires a central transactional database.

Explicit initialization/migration pins a database identity in private persistent
metadata. Normal access refuses a missing/corrupt/mismatched established store.
Resolve the data directory once at startup. Protect the directory and database,
WAL, migration backup, and marker files from other users.

Import existing JSON identities and access atomically without changing IDs,
hashes, revocation, old balances, or active reservations. Retain a private recovery
backup and prevent old binaries from treating the legacy JSON as live authority.
Migration must be restartable. Never reinterpret conservative historical units as
new exact token counts or existing calendar windows as first-use windows.

## Accounting contract

Normalize usage before protocol conversion. Preserve optional numeric components:

- total input, uncached input, cache read, cache write;
- total output including reasoning exactly once, plus optional reasoning subset;
- provenance and completeness (reported/estimated/unknown; final/partial).

Cache reads/writes are input subsets; reasoning is an output subset. Do not add
them again to input+output totals. Missing is not zero. Provider/model differences
must have fixtures, including OpenAI cache_write_tokens, Claude input+read+write,
and Gemini candidate output plus thinking.

Keep observed actual usage, reserved upper bounds, and uncertain conservative
charges separate. Only complete trustworthy usage may refund unused reservation.
Partial/missing usage retains allowance conservatively. Record actual overages
without clamping them to a limit; never manufacture free capacity.

Admission transaction:

1. Recheck current key status/access and quota policy.
2. Derive applicable persisted windows and any first-use anchor.
3. Check every resource against confirmed + reserved + uncertain usage.
4. Persist request/attempt identifiers, all reservations, and admission event.
5. Commit before upstream dispatch; no HTTP calls while holding the transaction.

Settlement atomically updates the exact saved windows and writes an idempotent
ledger event. Duplicate settlement cannot double-charge. Existing reservation
replay uses its saved window even after renewal. New upstream attempts reserve
again under the current policy. Reporting can be asynchronous; authoritative
settlement cannot rely solely on an in-memory queue.

Strict output quotas require a provider-enforced output ceiling on the final
request. Cache reservations use conservative worst-case bounds, since cache hits
are not known in advance. Unsupported models/routes, remote context, unbounded
hosted tools, and unsafe multiplicity fail closed. A disconnected local stream is
not proof that upstream computation stopped.

Recovery converts abandoned holds to explicitly uncertain consumption, not free
allowance. Later authoritative evidence can reconcile them idempotently. Persist
clock/window high-water state so a backward clock jump cannot resurrect capacity;
trusted synchronized UTC remains an operating assumption.

## API and operator experience

Expose quota policy through managed-key create/update/list and CLI configuration.
Expose current per-rule windows, reset timestamps, confirmed/reserved/uncertain
amounts, remaining allowance, and capability errors. Dashboard controls must make
input/cache overlap and first-use timing explicit and must preserve existing
provider/account restrictions. Managed-key Test API uses the same enforcement.

Quota exhaustion returns protocol-native 429 with reset/retry information;
unsupported measurement returns an explicit local policy error; storage failure
fails closed. Never store prompts, response text, secrets, or arbitrary model
strings in the quota ledger.

Back up through SQLite Online Backup or VACUUM INTO, not live copying of only the
main file. A restart is not a restore: restoring an old snapshot necessarily
requires reconciliation for the missing interval. History retention cannot modify
quota balances. Request/day quotas do not replace burst/concurrency throttling.

## Implementation and verification sequence

1. Save this contract; preserve the existing worktree and establish baseline tests.
2. Implement typed rules, normalization, anchored windows, and atomic multi-resource
   ledger. Test time boundaries, all-or-nothing admission, duplicate settlement,
   concurrency, uncertain recovery, and persistence.
3. Migrate key authority and upgrade SQLite. Test legacy preservation, wrong/missing
   stores, concurrent edits/revocation, and runtime SQLite version.
4. Implement provider usage normalization and capability-gated bounds. Test each
   provider fixture, missing/partial usage, cache overlap, and reasoning inclusion.
5. Integrate admission/settlement with every request, retry, stream, alias, and
   managed-key Test API path. Test no-upstream-call denials and per-attempt billing.
6. Add API summaries, dashboard/CLI controls, and operator documentation. Build UI
   and exercise create/edit/list flows.
7. Run full Rust tests, formatting, frontend build, and an isolated localhost mock
   matrix. Include restart/kill recovery, simultaneous first use, rollover during
   streams, multi-rule boundaries, failed storage, and migration.

Provider calls are unnecessary for deterministic quota tests. Do not modify
production, deploy, or spend production quota as part of this implementation.
Any unverified provider capability remains unsupported rather than guessed.

## Research sources

- https://www.sqlite.org/wal.html (host restrictions, WAL state, WAL-reset bug)
- https://www.sqlite.org/pragma.html#pragma_synchronous (durability)
- https://www.sqlite.org/backup.html (consistent live backups)
- https://docs.stripe.com/billing/subscriptions/billing-cycle (calendar anchors)
- https://developers.openai.com/api/docs/guides/prompt-caching (cache accounting)
- https://developers.openai.com/api/docs/guides/reasoning (reasoning/output limits)
- https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events
- https://platform.claude.com/docs/en/build-with-claude/prompt-caching
- https://ai.google.dev/api/generate-content#UsageMetadata

## Verification record

### Delivered implementation

- `src/api_key_quota.rs`: validated policies, anchored windows, transactional
  multi-resource reservations, idempotent settlement, conservative recovery,
  clock high-water protection, and schema migration.
- `src/api_keys.rs` and `src/api_key_policy_store.rs`: durable key authority,
  legacy migration, pinned storage identity, cross-process coordination, and
  private live-WAL backups. SQLite is bundled at version 3.53.2.
- `src/api_key_quota_runtime.rs` and `src/quota_usage.rs`: final native request
  bounds, provider-native usage normalization, per-attempt accounting, and
  request-lifetime cancellation protection.
- Provider adapters reserve before every generation dispatch, including retries
  and fallback. Client request counts remain distinct from upstream attempts.
- Dashboard controls, API summaries, `iogw keys create/update/quotas`, and
  `io-gateway --config CONFIG --backup-policy NEW_DIRECTORY` are implemented.
  Operator and generated public documentation describe the new controls.

### Bugs found and fixed during implementation/testing

- Missing/dropped quota schema could recreate empty balances: established
  identity/schema checks now fail closed.
- Missing historical accounting during JSON migration could invent a fresh
  allowance: ambiguous budgeted migrations now require restoration/reconciliation.
- Wholly undispatched retries could strand a request charge or first-use anchor:
  both release orders refund correctly; unused provisional schedules are removed,
  while attempt replay retains its original identity/anchor.
- Early/partial SSE usage could be treated as final: only complete trustworthy
  observations release allowance. Known terminal output-ceiling truncation still
  settles its final actual usage.
- Fabricated zero usage and inconsistent cache breakdowns could refund unknown
  consumption: absent fields remain unknown and invalid observations cannot refund.
- An ignored nested `request` field or wrong native cap alias could underreserve
  output: adapters explicitly identify their native protocol, and bounds inspect
  the exact final request using only supported native ceilings.
- Request-only keys could forward malformed/nonobject JSON or missing-model text
  requests and establish an anchor: local validation now rejects them before any
  provider dispatch. Compressed generation requests are also rejected locally.
- An in-flight legacy budget edit could dispatch under a stale snapshot: admission
  rechecks it with the current authority and returns `409 api_key_policy_changed`
  without dispatch or new holds when a client must retry with current policy.
- Quota summaries were stale immediately after dashboard create/edit/revoke:
  mutation responses now include fresh durable summaries.
- An empty, truncated, unrelated, or incomplete legacy SQLite file could be
  mistaken for valid historical accounting during migration. Read-only integrity
  and required-table/column checks now run before schema creation or identity
  pinning; failed migration preserves the original files for recovery.
- A missing schedule row could otherwise recreate a first-use anchor despite
  surviving charges. Reservations and summaries now fail closed when nonreleased
  attempts or nonzero balances remain without their original schedule.
- Cancellation while a blocking admission was committing could strand an
  undispatched hold. An admission lease now owns the context through commit and
  releases a newly created reservation if its waiter disappears. Accepted
  handoffs stay conservative, and unexpected reservation replay never refunds an
  existing possibly dispatched attempt.
- Google native tool/response schemas and native plaintext function arguments
  were falsely rejected as hidden context. Only their exact protocol-selected
  JSON positions are exempted from marker scanning; their complete serialized
  bytes still count. Adjacent media, tool-result attachments, and spoofed paths
  remain blocked.
- Native Claude MCP connectors could add unmeasurable remote context. Nonempty
  connectors now fail closed for affected token rules; empty/null connectors
  and ordinary literal tool arguments remain usable.
- Copilot's Responses-to-Chat bridge used the wrong output-cap field for known
  reasoning models. Its native cap now matches the model capability gate.
- Dashboard edits could silently drop limits above JavaScript's exact integer
  range or widen selected-account scope when an unselected account had a cap.
  Such large policies now require exact CLI/API editing, and account caps never
  implicitly select or authorize an account.
- Combining CLI access JSON with an explicit input cap ignored the explicit
  cap. The CLI now merges it, removes a conflicting deprecated alias, and rejects
  zero before sending a request.
- Misspelled admin payload fields could silently create an unrestricted key.
  Create/update payloads and nested access rules now reject unknown fields at the
  HTTP boundary, while legacy stored metadata remains readable.
- Dashboard usage could stay stale after Test API calls or while settings were
  open. Read-only balance refreshes now preserve unsaved edits and ignore stale
  responses. Accounting failures return HTTP 503 with per-key errors rather than
  reporting successful quota retrieval.
- Renewable denial audits now retain their actual HTTP status and measurement
  classification rather than an absent status or misleading input estimate.

### Explicit capability and operational boundaries

- All seven metrics and three periods are implemented. Input/cache reservations
  are conservative; unreported categories remain uncertain rather than zero.
- Strict output ceilings are not available on Codex, Grok, or native MiniMax
  Responses. Such requests receive a local capability error if an output rule is
  active. Other provider/model/protocol combinations are capability-gated too;
  unsupported hosted context, multiplicity, background work, or reasoning modes
  are not silently treated as measurable.
- An activated policy's schedule/resource shape cannot silently change. Ordinary
  limit edits and disable/re-enable preserve usage. Creating a different policy
  requires an explicit new key/migration decision.
- SQLite sharing is single-host/local-filesystem only. Restoring an older backup
  requires reconciliation for the interval after that snapshot.
- API/CLI policies support positive signed-64-bit limits. Dashboard editing is
  limited to JavaScript's exact integer range (9,007,199,254,740,991); it refuses
  unsafe edits instead of rounding or removing existing rules.
- All integration checks use disposable localhost mocks, not production provider
  credentials or billable requests. This verifies gateway enforcement and adapter
  fixtures, not a claim that every live provider preserves these semantics forever.

### Commands and coverage

| Command | Result |
| --- | --- |
| `CARGO_INCREMENTAL=0 cargo test --all-targets --quiet` | 576 passed: 567 gateway + 9 CLI; zero failures |
| `CARGO_INCREMENTAL=0 cargo build --bins --quiet` | Passed |
| `node scripts/test-renewable-quotas.mjs` | 56/56 end-to-end checks passed; 41 synthetic localhost generation attempts |
| `node --test scripts/test-api-key-quota-dashboard.mjs` | 19/19 passed |
| `npm --prefix desktop run build` | Passed TypeScript and Vite production build |
| `cargo fmt --all -- --check` | Passed |
| `git diff --check` | Passed |
| `node scripts/generate-docs.mjs` | Generated public documentation successfully |

The end-to-end suite covers all seven resources across all three periods,
20-way one-slot concurrency (single and multiple processes), first-use anchoring,
cache/input overlap, output reservation/refund, missing/partial/zero usage,
reported overage, native JSON/SSE errors, client cancellation, aliases, retries,
managed-key Test API, CLI combined flags, rejected admin policy typos, native MCP
connectors versus ordinary tool arguments, policy edits, clean restart and SIGKILL,
permissions, live-WAL backup restoration, no-clobber snapshots, and
missing/corrupt/substituted storage. Separate deterministic Rust tests cover exact
daily/weekly/monthly boundaries, idle cadence, January-31/leap-year renewal, DST
gaps/overlaps, backward clock movement, and settlement/replay against original
saved windows. Additional regressions exercise admission cancellation before and
after commit, damaged legacy migration, missing schedule rows, real provider
payload builders, admin validation, and CLI merging. Dashboard tests cover exact
integer safety, selected-account preservation, balance refresh, stale responses,
and editing-state preservation as well as policy construction and validation.

Initial end-to-end failures for malformed JSON and missing-model requests were
fixed and the complete suite rerun successfully. An earlier disk-space interruption
was resolved by cleaning this package's regenerable Cargo artifacts and rebuilding
with incremental compilation disabled; no source, configuration, or credentials
were deleted. The resumed audit's additional fixes were then verified against the
final source and rebuilt binaries using every command above. Builds still emit
non-fatal unused-code warnings. All disposable gateways and fixture directories
were cleaned up. No production deployment or live-provider billing test was
performed.
