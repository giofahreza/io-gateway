import { AlertTriangle, CheckCircle2 } from 'lucide-react';
import { AccountUsage, DashboardSnapshot } from '../api/gateway';

type Props = {
  accounts: AccountUsage[];
  snapshot: DashboardSnapshot | null;
  compact: boolean;
};

type QuotaBar = {
  label: string;
  hint: string;
  percent: number;
  title?: string;
};

type QuotaAccount = {
  provider: string;
  label: string;
  keys: string[];
  bars: QuotaBar[];
  balances: QuotaBalance[];
  status: string | null;
};

type QuotaBalance = {
  label: string;
  value: string;
};

type MergedRow = {
  provider: string;
  key: string;
  displayName: string;
  requests: number;
  errors: number;
  totalTokens: number;
  inputTokens: number;
  outputTokens: number;
  lastSuccessAt?: string | null;
  lastErrorMessage?: string | null;
  quota: QuotaAccount | null;
};

const PROVIDERS = ['codex', 'agw', 'gemini', 'qwen', 'deepseek', 'grok', 'minimax', 'copilot', 'claude', 'glm'];

export function AccountUsageTable({ accounts, snapshot, compact }: Props) {
  const quotaAccounts = allQuotaAccounts(snapshot);
  const profiles = allAccountProfiles(snapshot);
  const rows = mergeRows(accounts, quotaAccounts, profiles);
  const groupedRows = groupRows(rows);

  return (
    <section className={`panel table-panel merged-table-panel ${compact ? 'compact-account-panel' : ''}`}>
      <div className="panel-header">
        <div>
          <h2>Accounts</h2>
          <p>{compact ? 'Minimized by provider' : `${rows.length} usage and quota rows`}</p>
        </div>
      </div>
      {compact ? (
        <div className="compact-account-groups">
          {groupedRows.map((group) => (
            <div className="compact-provider-group" key={group.provider}>
              <div className="compact-provider-title">
                <span className="provider-badge">{providerLabel(group.provider)}</span>
                <span>{group.rows.length}</span>
              </div>
              <div className="compact-account-list">
                {group.rows.map((row) => (
                  <div className="compact-account-row" key={`${row.provider}:${row.key}:${row.quota?.label ?? 'usage'}`}>
                    <span className="compact-account-name" title={row.key}>{row.displayName}</span>
                    <span className="compact-account-percent"><MiniQuota quota={row.quota} /></span>
                    <span className="compact-account-tokens">{formatNumber(row.totalTokens)} tok</span>
                  </div>
                ))}
              </div>
            </div>
          ))}
          {rows.length === 0 && <div className="empty-state inline">No account usage or quota recorded yet.</div>}
        </div>
      ) : (
      <div className="table-wrap">
        <table className="account-table">
          <thead>
            <tr>
              <th>Provider</th>
              <th>Account</th>
              <th className="num">Req</th>
              <th className="num">Err</th>
              <th className="num">Tokens</th>
              <th>Usage %</th>
              <th>Status</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={`${row.provider}:${row.key}:${row.quota?.label ?? 'usage'}`}>
                <td data-label="Provider">
                  <span className="provider-badge">{providerLabel(row.provider)}</span>
                </td>
                <td data-label="Account" className="account-key">
                  <span title={row.key}>{row.displayName}</span>
                </td>
                <td data-label="Requests" className="num">
                  {formatNumber(row.requests)}
                </td>
                <td data-label="Errors" className="num">
                  {formatNumber(row.errors)}
                </td>
                <td data-label="Tokens" className="num">
                  <span title={`${formatNumber(row.inputTokens)} input / ${formatNumber(row.outputTokens)} output`}>
                    {formatNumber(row.totalTokens)}
                  </span>
                </td>
                <td data-label="Usage %">
                  <QuotaBars quota={row.quota} />
                </td>
                <td data-label="Status">
                  <span className={`row-status ${row.lastErrorMessage ? 'bad' : 'ok'}`}>
                    {row.lastErrorMessage ? <AlertTriangle size={14} /> : <CheckCircle2 size={14} />}
                    {row.lastErrorMessage || row.lastSuccessAt || (row.quota ? 'Quota only' : 'No activity')}
                  </span>
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={7} className="empty-cell">
                  No account usage or quota recorded yet.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      )}
    </section>
  );
}

function MiniQuota({ quota }: { quota: QuotaAccount | null }) {
  const max = quota?.bars.length ? Math.max(...quota.bars.map((bar) => bar.percent)) : null;
  if (max == null || !Number.isFinite(max)) return <span className="muted">No quota</span>;
  return (
    <>
      <span>{max.toFixed(1)}%</span>
      <span className="mini-quota-meter">
        <span style={{ width: `${clamp(max)}%`, ['--quota-color' as string]: quotaColor(max) }} />
      </span>
    </>
  );
}

function QuotaBars({ quota }: { quota: QuotaAccount | null }) {
  if (!quota || (quota.bars.length === 0 && quota.balances.length === 0)) {
    return <span className="quota-compact muted">{quota?.status || 'No quota'}</span>;
  }
  return (
    <div className="quota-bars" title={quota.status || undefined}>
      {quota.bars.map((bar, index) => (
        <div className="quota-bar-row" key={`${bar.label}:${index}`} title={bar.title || `${bar.label} ${bar.hint}`}>
          <div className="quota-bar-labels">
            <span>{bar.label}</span>
            <span>{bar.hint}</span>
          </div>
          <div className="quota-meter compact">
            <div
              className={quotaTone(bar.percent)}
              style={{
                width: `${clamp(bar.percent)}%`,
                ['--quota-color' as string]: quotaColor(bar.percent),
              }}
            />
          </div>
        </div>
      ))}
      {quota.balances.map((balance, index) => (
        <div className="quota-bar-row" key={`${balance.label}:${index}`}>
          <div className="quota-bar-labels">
            <span>{balance.label}</span>
            <span>{balance.value}</span>
          </div>
        </div>
      ))}
      {quota.status && <div className="quota-compact-status">{quota.status}</div>}
    </div>
  );
}

type AccountProfile = {
  provider: string;
  keys: string[];
  displayName: string;
};

function mergeRows(accounts: AccountUsage[], quotaAccounts: QuotaAccount[], profiles: AccountProfile[]): MergedRow[] {
  const usedQuota = new Set<number>();
  const usedUsage = new Set<number>();
  const rows: MergedRow[] = profiles.map((profile) => {
    const usage = aggregateUsageForProfile(profile, accounts, usedUsage);
    const key = profile.keys[0] || profile.displayName;
    const quotaIndex = bestQuotaIndexForKeys(profile.provider, profile.keys, quotaAccounts, usedQuota);
    const quota = quotaIndex == null ? null : quotaAccounts[quotaIndex];
    if (quotaIndex != null) usedQuota.add(quotaIndex);
    return {
      provider: profile.provider,
      key,
      displayName: profile.displayName || quota?.label || readableAccountName(key),
      requests: usage?.requests ?? 0,
      errors: usage?.errors ?? 0,
      totalTokens: usage?.totalTokens ?? 0,
      inputTokens: usage?.inputTokens ?? 0,
      outputTokens: usage?.outputTokens ?? 0,
      lastSuccessAt: usage?.lastSuccessAt,
      lastErrorMessage: usage?.lastErrorMessage,
      quota,
    };
  });

  return rows.sort((a, b) => b.totalTokens - a.totalTokens || b.requests - a.requests || a.provider.localeCompare(b.provider));
}

function groupRows(rows: MergedRow[]): Array<{ provider: string; rows: MergedRow[] }> {
  const groups = new Map<string, MergedRow[]>();
  rows.forEach((row) => {
    const group = groups.get(row.provider) ?? [];
    group.push(row);
    groups.set(row.provider, group);
  });
  return [...groups.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([provider, groupRows]) => ({ provider, rows: groupRows }));
}

function bestQuotaIndexForKeys(provider: string, keys: string[], rows: QuotaAccount[], used: Set<number>): number | null {
  const accountKeys = normalizedMatchKeys(keys);
  const providerRows = rows
    .map((row, index) => ({ row, index }))
    .filter(({ row, index }) => row.provider === provider && !used.has(index));
  if (providerRows.length === 0) return null;
  const exact = providerRows.find(({ row }) => normalizedMatchKeys(row.keys).some((key) => accountKeys.includes(key)));
  if (exact) return exact.index;
  const fuzzy = providerRows.find(({ row }) =>
    normalizedMatchKeys(row.keys).some((key) => accountKeys.some((accountKey) => fuzzyKeyMatch(key, accountKey))),
  );
  if (fuzzy) return fuzzy.index;
  return providerRows.length === 1 ? providerRows[0].index : null;
}

function aggregateUsageForProfile(profile: AccountProfile, accounts: AccountUsage[], used: Set<number>): AccountUsage | null {
  const matches = bestUsageIndexes(profile, accounts, used);
  matches.forEach((index) => used.add(index));
  if (matches.length === 0) return null;

  let lastErrorAt: string | null = null;
  let lastErrorMessage: string | null = null;
  const usage = matches.reduce<AccountUsage>(
    (total, index) => {
      const account = accounts[index];
      total.requests += account.requests;
      total.errors += account.errors;
      total.inputTokens += account.inputTokens;
      total.outputTokens += account.outputTokens;
      total.totalTokens += account.totalTokens;
      total.cacheTokens += account.cacheTokens;
      total.reasoningTokens += account.reasoningTokens;
      total.lastSuccessAt = latestTimestamp(total.lastSuccessAt, account.lastSuccessAt);
      if (account.lastErrorAt && (!lastErrorAt || account.lastErrorAt > lastErrorAt)) {
        lastErrorAt = account.lastErrorAt;
        lastErrorMessage = account.lastErrorMessage ?? null;
      } else if (!lastErrorMessage && account.lastErrorMessage) {
        lastErrorMessage = account.lastErrorMessage;
      }
      return total;
    },
    {
      key: profile.keys[0] || profile.displayName,
      provider: profile.provider,
      requests: 0,
      errors: 0,
      inputTokens: 0,
      outputTokens: 0,
      totalTokens: 0,
      cacheTokens: 0,
      reasoningTokens: 0,
      lastSuccessAt: null,
      lastErrorAt: null,
      lastErrorMessage: null,
    },
  );
  usage.lastErrorAt = lastErrorAt;
  usage.lastErrorMessage = lastErrorMessage;
  return usage;
}

function bestUsageIndexes(profile: AccountProfile, accounts: AccountUsage[], used: Set<number>): number[] {
  const provider = normalizeProvider(profile.provider);
  const profileKeys = normalizedMatchKeys(profile.keys);
  const providerRows = accounts
    .map((account, index) => ({ account, index }))
    .filter(({ account, index }) => normalizeProvider(account.provider) === provider && !used.has(index));
  const exact = providerRows.filter(({ account }) => profileKeys.includes(normalize(account.key)));
  if (exact.length > 0) return exact.map(({ index }) => index);
  return providerRows
    .filter(({ account }) => profileKeys.some((key) => fuzzyKeyMatch(key, normalize(account.key))))
    .map(({ index }) => index);
}

function allQuotaAccounts(snapshot: DashboardSnapshot | null): QuotaAccount[] {
  if (!snapshot) return [];
  return PROVIDERS.flatMap((provider) => quotaAccounts(provider, quotaPayload(snapshot, provider)));
}

function allAccountProfiles(snapshot: DashboardSnapshot | null): AccountProfile[] {
  if (!snapshot) return [];
  const providers = asRecord(snapshot.providers);
  const dashboard = asRecord(snapshot.dashboard);
  const profiles: AccountProfile[] = [];
  const codexAccounts = Array.isArray(dashboard?.accounts) ? dashboard.accounts : [];
  profiles.push(...codexAccounts.map((account) => accountProfile('codex', account)).filter(Boolean) as AccountProfile[]);
  PROVIDERS.filter((provider) => provider !== 'codex').forEach((provider) => {
    const payload = asRecord(providers?.[provider]) ?? asRecord(providers?.[provider === 'agw' ? 'antigravity' : provider]);
    const accounts = Array.isArray(payload?.accounts) ? payload.accounts : [];
    profiles.push(...accounts.map((account) => accountProfile(provider, account)).filter(Boolean) as AccountProfile[]);
  });
  return profiles;
}

function accountProfile(provider: string, item: unknown): AccountProfile | null {
  const record = asRecord(item);
  if (!record) return null;
  const keys = uniqueStrings([
    firstString(record, ['file_name']),
    firstString(record, ['label']),
    firstString(record, ['email']),
    firstString(record, ['login']),
    firstString(record, ['organization_uuid']),
    firstString(record, ['account_id']),
    firstString(record, ['name']),
  ]);
  const displayName = readableFromRecord(record);
  if (!keys.length && !displayName) return null;
  return {
    provider: normalizeProvider(provider),
    keys: keys.length ? keys : [displayName || 'Account'],
    displayName: displayName || 'Account',
  };
}

function quotaPayload(snapshot: DashboardSnapshot, provider: string): unknown {
  const quotas = asRecord(snapshot.quotas);
  return quotas?.[provider] ?? quotas?.[provider === 'agw' ? 'antigravity' : provider] ?? snapshot[provider];
}

function quotaAccounts(provider: string, value: unknown): QuotaAccount[] {
  const payload = asRecord(value);
  const array = Array.isArray(value) ? value : payload?.accounts;
  if (!Array.isArray(array)) return [];
  return array.map((item) => quotaAccount(provider, item)).filter((item) => item.bars.length > 0 || item.status);
}

function quotaAccount(provider: string, item: unknown): QuotaAccount {
  const record = asRecord(item) ?? {};
  const normalizedProvider = normalizeProvider(provider);
  const label = readableFromRecord(record) || 'Account';
  const keys = uniqueStrings([
    firstString(record, ['file_name']),
    firstString(record, ['label']),
    firstString(record, ['email']),
    firstString(record, ['login']),
    firstString(record, ['organization_uuid']),
    firstString(record, ['account_id']),
    firstString(record, ['name']),
  ]);
  return {
    provider: normalizedProvider,
    label,
    keys: keys.length ? keys : [label],
    bars: quotaBars(normalizedProvider, record),
    balances: quotaBalances(record),
    status: firstString(record, ['status_msg', 'message', 'status', 'note', 'balance_note']),
  };
}

function quotaBars(provider: string, quota: Record<string, unknown>): QuotaBar[] {
  const bars: QuotaBar[] = [];
  const codeGeneration = asRecord(quota.code_generation);
  if (codeGeneration) {
    bars.push(...progressPair('Code Gen', codeGeneration.five_hour, codeGeneration.weekly));
  }
  const codeReview = asRecord(quota.code_review);
  if (codeReview) {
    bars.push(...progressPair('Code Review', codeReview.five_hour, codeReview.weekly));
  }
  const additionalRateLimits = arrayRecords(quota.additional_rate_limits);
  for (const limit of additionalRateLimits) {
    bars.push(...progressPair(firstString(limit, ['display_name', 'limit_name']) || 'Model limit', limit.five_hour, limit.weekly));
  }

  const hideProviderModels = provider === 'agw' || provider === 'gemini';
  if (!hideProviderModels) {
    for (const model of arrayRecords(quota.models)) {
      const bucket = modelQuotaBucket(model);
      if (quotaBucketHasValue(bucket)) {
        bars.push(progressBar(firstString(model, ['display_name', 'model_id']) || 'Model', bucket, 'N/A'));
      }
    }
  }
  if (provider === 'gemini') {
    bars.push(...geminiFamilyBars(quota));
  }
  for (const group of arrayRecords(quota.groups)) {
    bars.push(...progressPair(firstString(group, ['display_name']) || 'Group', group.five_hour, group.weekly));
  }
  for (const limit of arrayRecords(quota.limits)) {
    const label = firstString(limit, ['label', 'scope']) || 'Limit';
    const pct = percentValue(limit) ?? 0;
    // Show the used percentage only; a total percentage is not repeated.
    // Without a percentage the reported counts stay as they are.
    const used = firstString(limit, ['used_text']);
    const total = firstString(limit, ['limit_text']);
    const percent = percentValue(limit) != null
      ? `${pct.toFixed(1)}%`
      : (used && total ? `${used}/${total}` : (used || 'Not reported'));
    const reset = firstString(limit, ['reset_label']) || '';
    bars.push({
      label,
      hint: reset ? `${percent} · ${reset}` : percent,
      percent: pct,
    });
  }
  if (provider === 'grok') {
    bars.push(...grokBars(quota));
  }
  if (asRecord(quota.current_window)) {
    const bucket = asRecord(quota.current_window);
    bars.push(progressBar('5h window', bucket, 'N/A', true));
  }
  if (asRecord(quota.weekly)) {
    const bucket = asRecord(quota.weekly);
    bars.push(progressBar('Weekly window', bucket, 'N/A', true));
  }
  return bars.filter((bar) => Number.isFinite(bar.percent));
}

function quotaBalances(quota: Record<string, unknown>): QuotaBalance[] {
  const rows: QuotaBalance[] = [];
  for (const balance of arrayRecords(quota.balances)) {
    if (balance.total_balance == null) continue;
    const currency = firstString(balance, ['currency']) || 'USD';
    rows.push({ label: 'Available balance', value: `${String(balance.total_balance)} ${currency}` });
    const granted = firstString(balance, ['granted_balance']);
    if (granted && granted !== '0.00' && granted !== '0') {
      rows.push({ label: 'Granted balance', value: `${granted} ${currency}` });
    }
    const toppedUp = firstString(balance, ['topped_up_balance']);
    if (toppedUp && toppedUp !== '0.00' && toppedUp !== '0') {
      rows.push({ label: 'Topped up balance', value: `${toppedUp} ${currency}` });
    }
  }
  return rows;
}

function progressPair(label: string, fiveHour: unknown, weekly: unknown): QuotaBar[] {
  const bars: QuotaBar[] = [];
  if (fiveHour || weekly) {
    bars.push(progressBar(`${label} 5h`, asRecord(fiveHour), 'N/A'));
    bars.push(progressBar(`${label} Weekly`, asRecord(weekly), 'N/A'));
  }
  return bars;
}

function progressBar(label: string, bucket: Record<string, unknown> | null, fallback: string, resetPhrase = false): QuotaBar {
  const pct = percentValue(bucket) ?? 0;
  let hint = fallback;
  if (bucket && percentValue(bucket) != null) {
    const percent = `${pct.toFixed(1)}%`;
    const resetLabel = firstString(bucket, ['reset_label']) || '';
    hint = resetPhrase
      ? (resetLabel ? `${percent} · ${resetLabel}` : percent)
      : (resetLabel ? `${percent} ${resetLabel}` : percent);
  }
  return { label, hint, percent: pct };
}

function geminiFamilyBars(quota: Record<string, unknown>): QuotaBar[] {
  const families = new Map<string, { models: string[]; bucket: Record<string, unknown> }>();
  for (const model of arrayRecords(quota.models)) {
    const family = geminiModelFamily(model);
    const bucket = modelQuotaBucket(model);
    if (!family || !quotaBucketHasValue(bucket)) continue;
    const current = families.get(family) ?? { models: [], bucket: { used_percent: null, remaining_percent: null, reset_label: '' } };
    current.models.push(firstString(model, ['model_id', 'id', 'model', 'display_name']) || family);
    const used = percentValue(bucket);
    const remaining = numberValue(bucket?.remaining_percent);
    if (used != null && (percentValue(current.bucket) == null || used > (percentValue(current.bucket) ?? 0))) {
      current.bucket.used_percent = used;
      current.bucket.reset_label = firstString(bucket ?? {}, ['reset_label']) || current.bucket.reset_label;
    }
    if (remaining != null && (numberValue(current.bucket.remaining_percent) == null || remaining < (numberValue(current.bucket.remaining_percent) ?? 0))) {
      current.bucket.remaining_percent = remaining;
    }
    families.set(family, current);
  }
  return [...families.entries()]
    .sort((a, b) => geminiFamilySortValue(a[0]) - geminiFamilySortValue(b[0]))
    .map(([family, summary]) => ({
      ...progressBar(`Gemini ${family}`, summary.bucket, 'N/A'),
      title: summary.models.join(', '),
    }));
}

function grokBars(quota: Record<string, unknown>): QuotaBar[] {
  const kinds = asRecord(quota.kinds);
  if (!kinds) return [];
  const bars: QuotaBar[] = [];
  bars.push(...grokKindBars(asRecord(kinds.DEFAULT_TEXT), true, true));
  bars.push(...grokKindBars(asRecord(kinds.DEFAULT_IMAGE), true, false));
  bars.push(...grokKindBars(asRecord(kinds.DEFAULT_VIDEO), true, false));
  return bars;
}

function grokKindBars(kind: Record<string, unknown> | null, showRequests: boolean, showTokens: boolean): QuotaBar[] {
  const rateLimits = asRecord(kind?.rate_limits);
  if (!rateLimits) return [];
  const bars: QuotaBar[] = [];
  if (showRequests) {
    const requests = asRecord(rateLimits.requests);
    if (requests?.limit != null) bars.push(grokLimitBar('requests', requests));
  }
  if (showTokens) {
    const tokens = asRecord(rateLimits.tokens);
    if (tokens?.limit != null) bars.push(grokLimitBar('tokens', tokens, true));
  }
  return bars;
}

function grokLimitBar(label: string, limit: Record<string, unknown>, roundRemaining = false): QuotaBar {
  const total = numberValue(limit.limit) ?? 0;
  const remaining = numberValue(limit.remaining) ?? total;
  const displayRemaining = roundRemaining ? Math.round(remaining) : remaining;
  return {
    label,
    hint: `${displayRemaining}/${total} (no reset info)`,
    percent: total > 0 ? (100 * (total - remaining)) / total : 0,
  };
}

function modelQuotaBucket(model: Record<string, unknown>): Record<string, unknown> | null {
  return asRecord(model.current) || asRecord(model.quota) || asRecord(model.limit);
}

function quotaBucketHasValue(bucket: Record<string, unknown> | null): boolean {
  return Boolean(
    bucket &&
      (bucket.used_percent != null ||
        bucket.remaining_percent != null ||
        bucket.limit != null ||
        bucket.remaining != null ||
        bucket.limit_text != null ||
        bucket.remaining_text != null ||
        bucket.used_text != null),
  );
}

function percentValue(bucket: Record<string, unknown> | null | undefined): number | null {
  if (!bucket) return null;
  const direct = numberValue(bucket.used_percent) ?? numberValue(bucket.usage_percent) ?? numberValue(bucket.percent_used) ?? numberValue(bucket.usedPercent);
  if (direct != null) return direct;
  const remainingPercent = numberValue(bucket.remaining_percent);
  if (remainingPercent != null) return 100 - remainingPercent;
  const limit = numberValue(bucket.limit);
  const remaining = numberValue(bucket.remaining);
  if (limit != null && limit > 0 && remaining != null) return (100 * (limit - remaining)) / limit;
  return null;
}

function geminiModelFamily(model: Record<string, unknown>): string {
  const raw = firstString(model, ['model_id', 'id', 'slug', 'model', 'model_name', 'name', 'display_name']) || '';
  const lower = raw.toLowerCase();
  if (!lower.includes('gemini')) return '';
  if (lower.includes('flash-lite') || lower.includes('flash_lite') || lower.includes('flash lite')) return 'Flash Lite';
  if (lower.includes('flash')) return 'Flash';
  if (lower.includes('pro')) return 'Pro';
  return '';
}

function geminiFamilySortValue(label: string): number {
  return label === 'Flash' ? 0 : label === 'Flash Lite' ? 1 : label === 'Pro' ? 2 : 99;
}

function normalizeProvider(provider: string): string {
  const value = provider.toLowerCase();
  if (value === 'antigravity') return 'agw';
  return value;
}

function providerLabel(provider: string): string {
  return provider === 'agw' ? 'agw' : provider;
}

function firstString(record: Record<string, unknown>, keys: string[]): string | null {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === 'string' && value.trim()) return value;
  }
  return null;
}

function readableFromRecord(record: Record<string, unknown>): string | null {
  const candidates = [
    firstString(record, ['label']),
    firstString(record, ['email']),
    firstString(record, ['login']),
    firstString(record, ['name']),
  ];
  const readable = candidates.find((value) => value && !looksLikeAccountCode(value));
  if (readable) return readable;
  const fallback = firstString(record, ['account_id', 'organization_uuid']);
  return fallback && !looksLikeAccountCode(fallback) ? fallback : null;
}

function readableAccountName(value: string): string {
  if (!value.trim()) return 'Account';
  if (!looksLikeAccountCode(value)) return value;
  const withoutExtension = value.replace(/\.(json|txt|token|credential)$/i, '');
  const cleaned = withoutExtension
    .replace(/[_-]*(credential|credentials|auth|token|account)[_-]*/gi, ' ')
    .replace(/[_-]+/g, ' ')
    .trim();
  if (cleaned && !looksLikeAccountCode(cleaned)) return cleaned;
  return 'Account';
}

function looksLikeAccountCode(value: string): boolean {
  const trimmed = value.trim();
  if (!trimmed) return true;
  if (trimmed.includes('@')) return false;
  if (/\.json$/i.test(trimmed)) return true;
  if (/^(acct|acc|org|user|token|cred|credential)[_-]?[a-z0-9_-]{8,}$/i.test(trimmed)) return true;
  if (/^[a-f0-9]{24,}$/i.test(trimmed)) return true;
  if (/^[a-z0-9_-]{28,}$/i.test(trimmed)) return true;
  return false;
}

function uniqueStrings(values: Array<string | null | undefined>): string[] {
  return [...new Set(values.filter((value): value is string => Boolean(value && value.trim())))];
}

function arrayRecords(value: unknown): Record<string, unknown>[] {
  return Array.isArray(value) ? value.map(asRecord).filter((item): item is Record<string, unknown> => Boolean(item)) : [];
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
}

function numberValue(value: unknown): number | null {
  const number = typeof value === 'number' ? value : typeof value === 'string' && value.trim() ? Number(value) : NaN;
  return Number.isFinite(number) ? number : null;
}

function stringOrNumber(value: unknown): string | null {
  if (typeof value === 'string' && value.trim()) return value;
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  return null;
}

function normalize(value: string): string {
  return value.toLowerCase().replace(/[^a-z0-9]+/g, '');
}

function normalizedMatchKeys(values: string[]): string[] {
  return uniqueStrings(values.map((value) => normalize(value)).filter((value) => value.length >= 3));
}

function fuzzyKeyMatch(a: string, b: string): boolean {
  return Boolean(a.length >= 6 && b.length >= 6 && (a.includes(b) || b.includes(a)));
}

function latestTimestamp(current?: string | null, incoming?: string | null): string | null {
  if (current && incoming) return current > incoming ? current : incoming;
  return current || incoming || null;
}

function clamp(value: number) {
  return Math.max(0, Math.min(100, value));
}

function quotaTone(value: number): string {
  const pct = clamp(value);
  return pct > 80 ? 'high' : pct > 50 ? 'mid' : 'low';
}

function quotaColor(value: number): string {
  const pct = clamp(value);
  const green = [34, 197, 94];
  const amber = [245, 158, 11];
  const red = [239, 68, 68];
  const from = pct <= 50 ? green : amber;
  const to = pct <= 50 ? amber : red;
  const ratio = pct <= 50 ? pct / 50 : (pct - 50) / 50;
  const mixed = from.map((channel, index) => Math.round(channel + (to[index] - channel) * ratio));
  return `rgb(${mixed[0]}, ${mixed[1]}, ${mixed[2]})`;
}

function formatNumber(value: number) {
  return new Intl.NumberFormat().format(value);
}
