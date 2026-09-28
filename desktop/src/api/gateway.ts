import { invoke } from '@tauri-apps/api/core';

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export type GatewayResponse<T = JsonValue> = {
  status: number;
  body: T;
};

export type AdminSession = {
  enabled: boolean;
  configured: boolean;
  authenticated: boolean;
  requires_api_key?: boolean;
  api_key_required?: boolean;
};

export type UsageTotals = {
  requests?: number;
  errors?: number;
  prompt_total?: number;
  prompt_error_total?: number;
  input_tokens?: number;
  output_tokens?: number;
  total_tokens?: number;
  cache_tokens?: number;
  reasoning_tokens?: number;
  first_recorded_at?: string | null;
  last_recorded_at?: string | null;
};

export type AccountUsage = {
  key: string;
  provider: string;
  requests: number;
  errors: number;
  inputTokens: number;
  outputTokens: number;
  totalTokens: number;
  cacheTokens: number;
  reasoningTokens: number;
  lastSuccessAt?: string | null;
  lastErrorAt?: string | null;
  lastErrorMessage?: string | null;
};

export type UsageSummary = {
  totals: UsageTotals;
  providers: Record<string, Array<{ key: string; usage: Record<string, unknown> }>>;
};

export type ContextBucket = {
  input_tokens?: number;
  output_tokens?: number;
  total_tokens?: number;
  cache_tokens?: number;
  reasoning_tokens?: number;
  request_count?: number;
};

export type ContextHistory = {
  hours: number;
  bucket_minutes: number;
  labels?: string[];
  buckets?: ContextBucket[];
  models?: Record<string, Array<{
    input?: number;
    output?: number;
    cache?: number;
    reasoning?: number;
  }>>;
};

export type DashboardSnapshot = {
  dashboard?: Record<string, unknown>;
  [provider: string]: unknown;
};

type RequestOptions = {
  method?: string;
  body?: JsonValue;
};

export async function gatewayRequest<T = JsonValue>(
  baseUrl: string,
  path: string,
  options: RequestOptions = {},
): Promise<GatewayResponse<T>> {
  return invoke<GatewayResponse<T>>('gateway_request', {
    request: {
      baseUrl,
      path,
      method: options.method ?? 'GET',
      body: options.body,
    },
  });
}

export async function login(
  baseUrl: string,
  otp: string,
  apiKey?: string,
): Promise<GatewayResponse<{ ok?: boolean; message?: string }>> {
  return invoke('gateway_login', { request: { baseUrl, otp, apiKey: apiKey || undefined } });
}

export function isUnauthorized(response: GatewayResponse<unknown>): boolean {
  return response.status === 401 || response.status === 403;
}

export function responseMessage(response: GatewayResponse<unknown>): string {
  const body = response.body;
  if (body && typeof body === 'object') {
    const record = body as Record<string, unknown>;
    if (typeof record.message === 'string') return record.message;
    if (record.error && typeof record.error === 'object') {
      const error = record.error as Record<string, unknown>;
      if (typeof error.message === 'string') return error.message;
    }
  }
  return `Gateway returned HTTP ${response.status}`;
}

export function flattenUsage(summary: UsageSummary | null): AccountUsage[] {
  if (!summary?.providers) return [];
  return Object.entries(summary.providers).flatMap(([provider, rows]) =>
    rows.map((row) => {
      const usage = row.usage ?? {};
      return {
        key: row.key,
        provider,
        requests: numberField(usage, 'requests'),
        errors: numberField(usage, 'errors'),
        inputTokens: numberField(usage, 'input_tokens'),
        outputTokens: numberField(usage, 'output_tokens'),
        totalTokens: numberField(usage, 'total_tokens'),
        cacheTokens: numberField(usage, 'cache_tokens'),
        reasoningTokens: numberField(usage, 'reasoning_tokens'),
        lastSuccessAt: stringField(usage, 'last_success_at'),
        lastErrorAt: stringField(usage, 'last_error_at'),
        lastErrorMessage: stringField(usage, 'last_error_message'),
      };
    }),
  );
}

function numberField(value: Record<string, unknown>, key: string): number {
  const raw = value[key];
  return typeof raw === 'number' && Number.isFinite(raw) ? raw : 0;
}

function stringField(value: Record<string, unknown>, key: string): string | null {
  const raw = value[key];
  return typeof raw === 'string' && raw.trim() ? raw : null;
}
