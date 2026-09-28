import { Activity, AlertCircle, CheckCircle2, Clock3, KeyRound, Minimize2, Moon, RefreshCcw, Server, Settings2, Sun } from 'lucide-react';
import { FormEvent, useCallback, useEffect, useMemo, useState } from 'react';
import {
  AccountUsage,
  AdminSession,
  ContextHistory,
  DashboardSnapshot,
  UsageSummary,
  flattenUsage,
  gatewayRequest,
  isUnauthorized,
  login,
  responseMessage,
} from './api/gateway';
import { AccountUsageTable } from './components/AccountUsageTable';
import { UsageHistoryChart } from './components/UsageHistoryChart';
import { UsageOverview } from './components/UsageOverview';

const DEFAULT_GATEWAY_URL = 'http://127.0.0.1:8319';
const STORAGE_KEY = 'io-gateway-desktop-settings';

type Settings = {
  baseUrl: string;
  hours: number;
  bucketMinutes: number;
  perModel: boolean;
  theme: 'dark' | 'light';
  compactAccounts: boolean;
};

type LoadState = 'idle' | 'loading' | 'ready' | 'auth' | 'error';

export function App() {
  const [settings, setSettings] = useState<Settings>(() => readSettings());
  const [draftBaseUrl, setDraftBaseUrl] = useState(settings.baseUrl);
  const [session, setSession] = useState<AdminSession | null>(null);
  const [summary, setSummary] = useState<UsageSummary | null>(null);
  const [history, setHistory] = useState<ContextHistory | null>(null);
  const [snapshot, setSnapshot] = useState<DashboardSnapshot | null>(null);
  const [state, setState] = useState<LoadState>('idle');
  const [message, setMessage] = useState<string | null>(null);
  const [otp, setOtp] = useState('');
  const [adminApiKey, setAdminApiKey] = useState('');
  const [loggingIn, setLoggingIn] = useState(false);
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);

  const accounts = useMemo<AccountUsage[]>(() => flattenUsage(summary), [summary]);

  const refresh = useCallback(async () => {
    setState('loading');
    setMessage(null);
    try {
      const sessionResponse = await gatewayRequest<AdminSession>(settings.baseUrl, '/admin/session');
      if (sessionResponse.status >= 400) {
        throw new Error(responseMessage(sessionResponse));
      }
      setSession(sessionResponse.body);

      if (sessionResponse.body.enabled && !sessionResponse.body.authenticated) {
        setState('auth');
        setSummary(null);
        setHistory(null);
        setSnapshot(null);
        return;
      }

      const query = `/usage/context-history.json?hours=${settings.hours}&bucket_minutes=${settings.bucketMinutes}&per_model=${settings.perModel}`;
      const [summaryResponse, historyResponse, snapshotResponse] = await Promise.all([
        gatewayRequest<UsageSummary>(settings.baseUrl, '/usage/summary.json'),
        gatewayRequest<ContextHistory>(settings.baseUrl, query),
        gatewayRequest<DashboardSnapshot>(settings.baseUrl, '/dashboard/snapshot.json'),
      ]);

      const unauthorized = [summaryResponse, historyResponse, snapshotResponse].find(isUnauthorized);
      if (unauthorized) {
        setState('auth');
        setMessage(responseMessage(unauthorized));
        return;
      }
      const failed = [summaryResponse, historyResponse, snapshotResponse].find((response) => response.status >= 400);
      if (failed) {
        throw new Error(responseMessage(failed));
      }

      setSummary(summaryResponse.body);
      setHistory(historyResponse.body);
      setSnapshot(snapshotResponse.body);
      setLastRefresh(new Date());
      setState('ready');
    } catch (error) {
      setState('error');
      setMessage(error instanceof Error ? error.message : String(error));
    }
  }, [settings]);

  useEffect(() => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
  }, [settings]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, 30_000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  async function handleLogin(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setLoggingIn(true);
    setMessage(null);
    try {
      const response = await login(settings.baseUrl, otp, adminApiKey);
      if (response.status >= 400 || !response.body.ok) {
        setMessage(responseMessage(response));
        return;
      }
      setOtp('');
      setAdminApiKey('');
      await refresh();
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setLoggingIn(false);
    }
  }

  function applyBaseUrl(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setSettings((current) => ({ ...current, baseUrl: normalizeBaseUrl(draftBaseUrl) }));
  }

  const connected = state === 'ready';
  const loading = state === 'loading';
  const isDark = settings.theme === 'dark';
  const adminApiKeyRequired = session?.requires_api_key === true || session?.api_key_required === true;

  return (
    <main className="app-shell" data-theme={settings.theme}>
      <header className="topbar">
        <div>
          <div className="app-title">
            <Activity size={22} />
            <h1>IO Gateway Usage</h1>
          </div>
          <p className="muted">
            {connected ? 'Connected to local gateway' : state === 'auth' ? 'Admin login required' : 'Checking gateway'}
          </p>
        </div>
        <div className="topbar-actions">
          <div className={`status-pill ${connected ? 'ok' : state === 'error' ? 'bad' : 'warn'}`}>
            {connected ? <CheckCircle2 size={16} /> : <AlertCircle size={16} />}
            <span>{connected ? 'Live' : state === 'error' ? 'Offline' : 'Attention'}</span>
          </div>
          <button className="icon-button" type="button" onClick={refresh} disabled={loading} title="Refresh now">
            <RefreshCcw size={18} className={loading ? 'spin' : undefined} />
          </button>
          <button
            className="icon-button"
            type="button"
            onClick={() => setSettings((current) => ({ ...current, theme: current.theme === 'dark' ? 'light' : 'dark' }))}
            title={isDark ? 'Switch to light mode' : 'Switch to dark mode'}
          >
            {isDark ? <Sun size={18} /> : <Moon size={18} />}
          </button>
          <button
            className={`icon-button ${settings.compactAccounts ? 'active' : ''}`}
            type="button"
            onClick={() => setSettings((current) => ({ ...current, compactAccounts: !current.compactAccounts }))}
            title={settings.compactAccounts ? 'Show detailed accounts' : 'Minimize account view'}
          >
            <Minimize2 size={18} />
          </button>
        </div>
      </header>

      <section className="control-band">
        <form className="gateway-form" onSubmit={applyBaseUrl}>
          <label>
            <Server size={16} />
            <input value={draftBaseUrl} onChange={(event) => setDraftBaseUrl(event.target.value)} />
          </label>
          <button type="submit">Apply</button>
        </form>

        <div className="filters">
          <label>
            <Clock3 size={16} />
            <select
              value={settings.hours}
              onChange={(event) => setSettings((current) => ({ ...current, hours: Number(event.target.value) }))}
            >
              <option value={1}>1 hour</option>
              <option value={6}>6 hours</option>
              <option value={24}>24 hours</option>
              <option value={168}>7 days</option>
              <option value={720}>30 days</option>
            </select>
          </label>
          <label>
            <Settings2 size={16} />
            <select
              value={settings.bucketMinutes}
              onChange={(event) => setSettings((current) => ({ ...current, bucketMinutes: Number(event.target.value) }))}
            >
              <option value={1}>1 min</option>
              <option value={5}>5 min</option>
              <option value={15}>15 min</option>
              <option value={30}>30 min</option>
              <option value={60}>60 min</option>
            </select>
          </label>
          <label className="toggle">
            <input
              type="checkbox"
              checked={settings.perModel}
              onChange={(event) => setSettings((current) => ({ ...current, perModel: event.target.checked }))}
            />
            <span>Per model</span>
          </label>
        </div>
      </section>

      {message && <div className="notice">{message}</div>}

      {state === 'auth' && (
        <section className="login-panel">
          <div>
            <KeyRound size={20} />
            <h2>Admin Session</h2>
            <p className="muted">
              {session?.configured
                ? adminApiKeyRequired
                  ? 'Enter the current TOTP code and administrator API key for this gateway.'
                  : 'Enter the current TOTP code for this gateway.'
                : 'Admin auth is enabled, but the gateway reports no TOTP secret configured.'}
            </p>
          </div>
          <form onSubmit={handleLogin}>
            <input
              inputMode="numeric"
              autoComplete="one-time-code"
              placeholder="6-digit code"
              value={otp}
              onChange={(event) => setOtp(event.target.value)}
            />
            {adminApiKeyRequired && (
              <input
                type="password"
                autoComplete="current-password"
                placeholder="Administrator API key"
                value={adminApiKey}
                onChange={(event) => setAdminApiKey(event.target.value)}
              />
            )}
            <button type="submit" disabled={loggingIn || !otp.trim() || (adminApiKeyRequired && !adminApiKey.trim())}>
              {loggingIn ? 'Signing in' : 'Sign in'}
            </button>
          </form>
        </section>
      )}

      <UsageOverview totals={summary?.totals ?? null} accounts={accounts} lastRefresh={lastRefresh} />
      <UsageHistoryChart history={history} />
      <AccountUsageTable accounts={accounts} snapshot={snapshot} compact={settings.compactAccounts} />
    </main>
  );
}

function readSettings(): Settings {
  try {
    const parsed = JSON.parse(localStorage.getItem(STORAGE_KEY) || 'null') as Partial<Settings> | null;
    return {
      baseUrl: normalizeBaseUrl(parsed?.baseUrl || DEFAULT_GATEWAY_URL),
      hours: parsed?.hours || 24,
      bucketMinutes: parsed?.bucketMinutes || 15,
      perModel: parsed?.perModel ?? false,
      theme: parsed?.theme === 'light' ? 'light' : 'dark',
      compactAccounts: parsed?.compactAccounts ?? false,
    };
  } catch {
    return { baseUrl: DEFAULT_GATEWAY_URL, hours: 24, bucketMinutes: 15, perModel: false, theme: 'dark', compactAccounts: false };
  }
}

function normalizeBaseUrl(value: string): string {
  return value.trim().replace(/\/+$/, '') || DEFAULT_GATEWAY_URL;
}
