import type { JsonValue, NetworkResponse, PluginContext } from "cursor-byok:plugin";
import { refreshBundle } from "./token.ts";
import type {
  ResourceDraft,
  ResourceImportFile,
  ResourceImportResult,
  ResourceImportSupport,
  ResourceMetric,
  ResourcePatch,
  ResourceSnapshot,
  ResourceState,
  ResourceView,
} from "cursor-byok:resource";

export const RESOURCE_TYPE = "kimi-account";

const MODELS_URL = "https://api.kimi.com/coding/v1/models";
const USAGE_URL = "https://api.kimi.com/coding/v1/usages";
const FIVE_HOURS_MS = 5 * 60 * 60 * 1000;

export type QuotaWindow = {
  usedPercent: number | null;
  remainingPercent: number | null;
  resetAtMs: number | null;
};

/** Kimi For Coding 订阅额度:顶层 usage 为周限额,limits 里 300 分钟窗口为 5 小时窗口。 */
export type AccountQuota = {
  weekly: QuotaWindow | null;
  fiveHour: QuotaWindow | null;
  updatedAtMs: number;
};

/** 单条 kimi-account 资源的 privateData 形状。 */
export type AccountData = {
  accessToken: string;
  refreshToken: string | null;
  displayName: string;
  quota: AccountQuota | null;
};

export type CredentialCandidate = {
  accessToken: string;
  refreshToken: string | null;
  displayName: string | null;
};

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function number(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && value.trim()) {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : null;
  }
  return null;
}

function decodeJwtPayload(token: string): Record<string, unknown> | null {
  const encoded = token.split(".")[1];
  if (!encoded) return null;
  try {
    const normalized = encoded.replace(/-/g, "+").replace(/_/g, "/");
    const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
    const bytes = Uint8Array.from(atob(padded), (character) => character.charCodeAt(0));
    return object(JSON.parse(new TextDecoder().decode(bytes)));
  } catch {
    return null;
  }
}

function claim(payload: Record<string, unknown> | null, key: string): string | null {
  return payload ? text(payload[key]) : null;
}

async function tokenFingerprint(token: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(token));
  return Array.from(
    new Uint8Array(digest).slice(0, 8),
    (byte) => byte.toString(16).padStart(2, "0"),
  ).join("");
}

export async function accountIdentity(
  accessToken: string,
): Promise<{ key: string; displayName: string }> {
  const payload = decodeJwtPayload(accessToken);
  const identity = claim(payload, "sub") ??
    claim(payload, "email") ??
    await tokenFingerprint(accessToken);
  const displayName = claim(payload, "email") ??
    claim(payload, "preferred_username") ??
    claim(payload, "name") ??
    "Kimi account";
  return { key: `kimi:${identity}`, displayName };
}

export async function credentialDraft(credential: CredentialCandidate): Promise<ResourceDraft> {
  const identity = await accountIdentity(credential.accessToken);
  const data: AccountData = {
    accessToken: credential.accessToken,
    refreshToken: credential.refreshToken,
    displayName: credential.displayName ?? identity.displayName,
    quota: null,
  };
  return { key: identity.key, privateData: data as unknown as JsonValue };
}

export function accountData(resource: ResourceSnapshot): AccountData {
  const data = object(resource.privateData);
  const accessToken = text(data?.accessToken);
  if (!accessToken) throw new Error("Kimi account resource is missing its access token");
  return {
    accessToken,
    refreshToken: text(data?.refreshToken),
    displayName: text(data?.displayName) ?? "Kimi account",
    quota: (data?.quota ?? null) as AccountQuota | null,
  };
}

/** 令牌剩余寿命低于该值时提前刷新,避免每次会话开头都先吃一次 401。 */
const EXPIRY_SKEW_MS = 60_000;

/** 访问令牌的过期时刻;JWT exp 声明缺失时返回 0(视为无已知过期)。 */
export function tokenExpiryMs(data: AccountData): number {
  return (number(decodeJwtPayload(data.accessToken)?.exp) ?? 0) * 1000;
}

/** 令牌是否临期;exp 缺失时视为未过期,由调用 401 后的兜底刷新处理。 */
export function tokenExpiring(data: AccountData, nowMs = Date.now()): boolean {
  const expiry = tokenExpiryMs(data);
  return expiry !== 0 && expiry <= nowMs + EXPIRY_SKEW_MS;
}

/** 用 refresh_token 换新令牌;返回 null 表示刷新被拒,需要重新登录。 */
export async function refreshAccessToken(
  data: AccountData,
  context: PluginContext,
): Promise<AccountData | null> {
  if (!data.refreshToken) return null;
  const bundle = await refreshBundle(data.refreshToken, context);
  if (!bundle) return null;
  return {
    ...data,
    accessToken: bundle.accessToken,
    refreshToken: bundle.refreshToken ?? data.refreshToken,
  };
}

function clampPercent(value: number): number {
  return Math.max(0, Math.min(100, value));
}

/** resetTime 容错解析:ISO 字符串或 epoch(秒/毫秒)。 */
function resetAtMs(window: Record<string, unknown>): number | null {
  const value = window.resetTime ?? window.reset_time ?? window.resetAt ?? window.reset_at;
  const numeric = number(value);
  if (numeric !== null) return numeric > 10_000_000_000 ? numeric : numeric * 1000;
  if (typeof value === "string") {
    const parsed = Date.parse(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return null;
}

/** Kimi usage 窗口的 limit/used/remaining 都是字符串数值,换算成剩余百分比。 */
function quotaWindow(value: unknown): QuotaWindow | null {
  const window = object(value);
  if (!window) return null;
  const limit = number(window.limit);
  const used = number(window.used);
  const remaining = number(window.remaining);
  let remainingPercent: number | null = null;
  if (limit !== null && limit > 0 && remaining !== null) {
    remainingPercent = clampPercent(Math.round((remaining / limit) * 100));
  } else if (limit !== null && limit > 0 && used !== null) {
    remainingPercent = clampPercent(Math.round((1 - used / limit) * 100));
  }
  return {
    usedPercent: remainingPercent === null ? null : 100 - remainingPercent,
    remainingPercent,
    resetAtMs: resetAtMs(window),
  };
}

export function parseKimiUsage(body: unknown): AccountQuota {
  const root = object(body) ?? {};
  const weekly = quotaWindow(root.usage);
  const limits = Array.isArray(root.limits) ? root.limits : [];
  const fiveHourEntry = limits.map(object).find((entry) => {
    const window = object(entry?.window);
    if (number(window?.duration) !== 300) return false;
    const unit = text(window?.timeUnit ?? window?.time_unit);
    return unit === null || unit.toUpperCase().includes("MINUTE");
  });
  return {
    weekly,
    fiveHour: quotaWindow(fiveHourEntry?.detail ?? null),
    updatedAtMs: Date.now(),
  };
}

/** 额度耗尽的冷却截止:周额度优先,其次 5 小时窗口;拿不到重置时间回退 5 小时。 */
export function quotaCoolingUntil(quota: AccountQuota, nowMs = Date.now()): number | null {
  for (const window of [quota.weekly, quota.fiveHour]) {
    if (!window || window.remainingPercent !== 0) continue;
    if (window.resetAtMs !== null && window.resetAtMs <= nowMs) continue;
    return window.resetAtMs ?? nowMs + FIVE_HOURS_MS;
  }
  return null;
}

export function quotaState(quota: AccountQuota | null, nowMs = Date.now()): ResourceState {
  if (!quota) return { status: "ready" };
  const coolingUntil = quotaCoolingUntil(quota, nowMs);
  return coolingUntil === null
    ? { status: "ready" }
    : { status: "cooling", retryAtMs: coolingUntil, message: "Kimi quota is exhausted" };
}

/** 429 时的资源补丁:标记 5 小时窗口耗尽,按回退时间进入冷却。 */
export function quotaExhaustedPatch(data: AccountData, nowMs = Date.now()): ResourcePatch {
  const quota: AccountQuota = {
    weekly: data.quota?.weekly ?? null,
    fiveHour: { usedPercent: 100, remainingPercent: 0, resetAtMs: nowMs + FIVE_HOURS_MS },
    updatedAtMs: nowMs,
  };
  return {
    privateData: { ...data, quota } as unknown as JsonValue,
    state: quotaState(quota, nowMs),
  };
}

export function presentAccount(resource: ResourceSnapshot): ResourceView {
  const data = accountData(resource);
  const metrics: ResourceMetric[] = [];
  const weekly = data.quota?.weekly;
  if (weekly && weekly.remainingPercent !== null) {
    metrics.push({
      id: "weekly",
      label: { "en-US": "Weekly quota", "zh-CN": "周额度" },
      unit: "percent",
      value: weekly.remainingPercent,
      ...(weekly.resetAtMs !== null ? { resetAtMs: weekly.resetAtMs } : {}),
    });
  }
  const fiveHour = data.quota?.fiveHour;
  if (fiveHour && fiveHour.remainingPercent !== null) {
    metrics.push({
      id: "five-hour",
      label: { "en-US": "5-hour window", "zh-CN": "5 小时窗口" },
      unit: "percent",
      value: fiveHour.remainingPercent,
      ...(fiveHour.resetAtMs !== null ? { resetAtMs: fiveHour.resetAtMs } : {}),
    });
  }
  return {
    displayName: data.displayName,
    ...(metrics.length > 0 ? { metrics } : {}),
  };
}

/** 先验证凭证(被拒时先尝试刷新令牌),再查订阅额度;额度查询失败不影响凭证结论。 */
export async function refreshAccount(
  resource: ResourceSnapshot,
  context: PluginContext,
): Promise<ResourcePatch> {
  let data = accountData(resource);
  let rotated = false;
  let response = await checkCredentials(data, context);
  if (isRejected(response.status) && data.refreshToken) {
    const refreshed = await refreshAccessToken(data, context);
    if (!refreshed) {
      return { state: { status: "invalid", message: EXPIRED_MESSAGE } };
    }
    data = refreshed;
    rotated = true;
    response = await checkCredentials(data, context);
  }
  if (isRejected(response.status)) {
    return { state: { status: "invalid", message: EXPIRED_MESSAGE } };
  }
  if (response.status < 200 || response.status >= 300) {
    throw new Error(`Kimi credential check failed (HTTP ${response.status}): ${response.body}`);
  }
  let quota: AccountQuota | null = null;
  try {
    const usage = await context.network.fetch(USAGE_URL, {
      method: "GET",
      headers: {
        accept: "application/json",
        authorization: `Bearer ${data.accessToken}`,
      },
    });
    if (usage.status >= 200 && usage.status < 300) {
      quota = parseKimiUsage(JSON.parse(usage.body));
    }
  } catch {
    // 额度是锦上添花:查询失败时保持 ready。
  }
  if (!quota) {
    return rotated ? { privateData: data as unknown as JsonValue } : { state: { status: "ready" } };
  }
  return {
    privateData: { ...data, quota } as unknown as JsonValue,
    state: quotaState(quota),
  };
}

const EXPIRED_MESSAGE = "Kimi authorization expired; sign in again";

function isRejected(status: number): boolean {
  return status === 401 || status === 403;
}

function checkCredentials(
  data: AccountData,
  context: PluginContext,
): Promise<NetworkResponse> {
  return context.network.fetch(MODELS_URL, {
    method: "GET",
    headers: {
      accept: "application/json",
      authorization: `Bearer ${data.accessToken}`,
    },
  });
}

function firstText(source: Record<string, unknown>, keys: string[]): string | null {
  for (const key of keys) {
    const value = text(source[key]);
    if (value) return value;
  }
  return null;
}

function collectCredentials(value: unknown, output: CredentialCandidate[]): void {
  if (Array.isArray(value)) {
    for (const item of value) collectCredentials(item, output);
    return;
  }
  const item = object(value);
  if (!item || item.disabled === true) return;
  for (const key of ["accounts", "credentials", "items"]) {
    if (Array.isArray(item[key])) {
      collectCredentials(item[key], output);
      return;
    }
  }
  const tokens = object(item.tokens) ?? item;
  const accessToken = firstText(tokens, ["access_token", "accessToken", "token", "key"]) ??
    firstText(item, ["access_token", "accessToken", "token", "key", "KIMI_API_KEY"]);
  if (!accessToken) return;
  const refreshToken = firstText(tokens, ["refresh_token", "refreshToken"]) ??
    firstText(item, ["refresh_token", "refreshToken"]);
  const displayName = firstText(item, ["email", "display_name", "displayName", "name"]) ??
    firstText(tokens, ["email", "display_name", "displayName", "name"]);
  output.push({ accessToken, refreshToken, displayName });
}

export function parseCredentialFiles(files: ResourceImportFile[]): {
  credentials: CredentialCandidate[];
  warnings: string[];
} {
  const credentials: CredentialCandidate[] = [];
  const warnings: string[] = [];
  for (const file of files) {
    let content: unknown;
    try {
      content = JSON.parse(file.content);
    } catch {
      warnings.push(`${file.name}: not valid JSON`);
      continue;
    }
    const found: CredentialCandidate[] = [];
    collectCredentials(content, found);
    if (found.length === 0) {
      warnings.push(`${file.name}: no Kimi access token found`);
      continue;
    }
    credentials.push(...found);
  }
  return { credentials, warnings };
}

export const credentialImport: ResourceImportSupport = {
  displayName: {
    "en-US": "Import Kimi credentials",
    "zh-CN": "导入 Kimi 凭证",
  },
  description: {
    "en-US": "Import one or more Kimi JSON credential files.",
    "zh-CN": "导入一个或多个 Kimi JSON 凭证文件。",
  },
  accept: [".json"],
  multiple: true,
  parse: async (files: ResourceImportFile[]): Promise<ResourceImportResult> => {
    const { credentials, warnings } = parseCredentialFiles(files);
    if (credentials.length === 0) {
      throw new Error(warnings.join("; ") || "credential JSON does not contain an access token");
    }
    return {
      resources: await Promise.all(credentials.map(credentialDraft)),
      ...(warnings.length > 0 ? { warnings } : {}),
    };
  },
};
