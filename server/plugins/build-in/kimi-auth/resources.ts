import type { NetworkResponse, PluginContext } from "cursor-byok:plugin";
import type { CredentialCandidate } from "cursor-byok:credentials";
import {
  jwtAccountIdentity,
  parseCredentialFiles as parseCredentialFilesShared,
} from "cursor-byok:credentials";
import {
  clampPercent,
  decodeJwtPayload,
  number,
  object,
  text,
  timestampMs,
} from "cursor-byok:json";
import { refreshBundle } from "./token.ts";
import type {
  QuotaWindow,
  ResourceDraft,
  ResourceImportFile,
  ResourceImportResult,
  ResourceImportSupport,
  ResourcePatch,
  ResourceSnapshot,
  ResourceState,
  ResourceView,
} from "cursor-byok:resource";
import { quotaWindowMetrics } from "cursor-byok:resource";

export type { QuotaWindow };

export const RESOURCE_TYPE = "kimi-account";

const MODELS_URL = "https://api.kimi.com/coding/v1/models";
const USAGE_URL = "https://api.kimi.com/coding/v1/usages";
const ME_URL = "https://api.kimi.com/coding/v1/me";
const FIVE_HOURS_MS = 5 * 60 * 60 * 1000;

/** Kimi For Coding 订阅额度:周限额、300 分钟(5 小时)窗口与月度限额。 */
export type AccountQuota = {
  /** 桌面档位徽章文案,直接透传 /coding/v1/me 的 user_level_name。 */
  tier: string | null;
  weekly: QuotaWindow | null;
  fiveHour: QuotaWindow | null;
  /** 新套餐的月度限额;旧套餐或字段缺失时为 null。 */
  monthly: QuotaWindow | null;
  updatedAtMs: number;
};

/** 单条 kimi-account 资源的 privateData 形状。 */
export type AccountData = {
  accessToken: string;
  refreshToken: string | null;
  displayName: string;
  quota: AccountQuota | null;
};

export function accountIdentity(
  accessToken: string,
): Promise<{ key: string; displayName: string }> {
  return jwtAccountIdentity(accessToken, "kimi", "Kimi account");
}

export async function credentialDraft(
  credential: Pick<CredentialCandidate, "accessToken" | "refreshToken" | "displayName">,
): Promise<ResourceDraft> {
  const identity = await accountIdentity(credential.accessToken);
  const data: AccountData = {
    accessToken: credential.accessToken,
    refreshToken: credential.refreshToken,
    displayName: credential.displayName ?? identity.displayName,
    quota: null,
  };
  return { key: identity.key, privateData: data };
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

/** resetTime 容错解析:多个键拼写归一后按 epoch(秒/毫秒)或 ISO 解析。 */
function resetAtMs(window: Record<string, unknown>): number | null {
  return timestampMs(window.resetTime ?? window.reset_time ?? window.resetAt ?? window.reset_at);
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
    remainingPercent,
    resetAtMs: resetAtMs(window),
  };
}

/** 从带 duration 的窗口条目判定窗口类型;duration 单位按 timeUnit 归一为分钟。 */
function windowMinutes(entry: Record<string, unknown>): number | null {
  const window = object(entry.window) ?? entry;
  const duration = number(window.duration);
  if (duration === null) return null;
  const unit = (text(window.timeUnit ?? window.time_unit) ?? "MINUTE").toUpperCase();
  if (unit.includes("SECOND")) return duration / 60;
  if (unit.includes("HOUR")) return duration * 60;
  if (unit.includes("DAY")) return duration * 24 * 60;
  return duration;
}

/** 新套餐 usages 形状:每条带 code 与 duration;code 含 month 或 duration≈30 天为月度。 */
function parseNewUsages(root: Record<string, unknown>): {
  fiveHour: QuotaWindow | null;
  weekly: QuotaWindow | null;
  monthly: QuotaWindow | null;
} | null {
  const usages = Array.isArray(root.usages) ? root.usages : null;
  if (!usages) return null;
  let fiveHour: QuotaWindow | null = null;
  let weekly: QuotaWindow | null = null;
  let monthly: QuotaWindow | null = null;
  for (const value of usages) {
    const entry = object(value);
    if (!entry) continue;
    const code = (text(entry.code) ?? "").toLowerCase();
    const minutes = windowMinutes(entry);
    const window = quotaWindow(entry.detail ?? entry);
    // code 与 duration 任一命中即可归类;上游省略 window 时按 code 兜底,不丢条目。
    if (code.includes("month") || (minutes !== null && minutes >= 30 * 24 * 60)) {
      monthly = monthly ?? window;
    } else if (
      code.includes("week") || code.includes("7d") ||
      (minutes !== null && minutes >= 7 * 24 * 60)
    ) {
      weekly = weekly ?? window;
    } else if (
      code.includes("hour") || code.includes("5h") ||
      (minutes !== null && minutes <= 5 * 60 + 1)
    ) {
      fiveHour = fiveHour ?? window;
    }
  }
  return { fiveHour, weekly, monthly };
}

export function parseKimiUsage(body: unknown): AccountQuota {
  const root = object(body) ?? {};
  // 新套餐:usages 数组带 code 与 duration;旧套餐:顶层 usage(周)+ limits(300 分钟)。
  const parsed = parseNewUsages(root);
  if (parsed) {
    return { tier: null, ...parsed, updatedAtMs: Date.now() };
  }
  const weekly = quotaWindow(root.usage);
  const limits = Array.isArray(root.limits) ? root.limits : [];
  const fiveHourEntry = limits.map(object).find((entry) => {
    const window = object(entry?.window);
    if (number(window?.duration) !== 300) return false;
    const unit = text(window?.timeUnit ?? window?.time_unit);
    return unit === null || unit.toUpperCase().includes("MINUTE");
  });
  return {
    tier: null,
    weekly,
    fiveHour: quotaWindow(fiveHourEntry?.detail ?? null),
    monthly: null,
    updatedAtMs: Date.now(),
  };
}

/** 额度耗尽的冷却截止:周额度优先,其次 5 小时窗口;拿不到重置时间回退 5 小时。 */
export function quotaCoolingUntil(quota: AccountQuota, nowMs = Date.now()): number | null {
  for (const window of [quota.weekly, quota.fiveHour, quota.monthly]) {
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
    tier: data.quota?.tier ?? null,
    weekly: data.quota?.weekly ?? null,
    fiveHour: { remainingPercent: 0, resetAtMs: nowMs + FIVE_HOURS_MS },
    monthly: data.quota?.monthly ?? null,
    updatedAtMs: nowMs,
  };
  return {
    privateData: { ...data, quota },
    state: quotaState(quota, nowMs),
  };
}

export function presentAccount(resource: ResourceSnapshot): ResourceView {
  const data = accountData(resource);
  const metrics = quotaWindowMetrics(data.quota ?? {});
  const tier = data.quota?.tier?.toUpperCase();
  return {
    displayName: data.displayName,
    ...(tier ? { tier } : {}),
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
  const headers = {
    accept: "application/json",
    authorization: `Bearer ${data.accessToken}`,
  };
  const [usage, me] = await Promise.all([
    fetchJson(context, USAGE_URL, headers),
    fetchJson(context, ME_URL, headers),
  ]);
  const meSource = object(me?.data) ?? me;
  const tier = text(meSource?.user_level_name ?? meSource?.userLevelName);
  let quota = usage ? parseKimiUsage(usage) : null;
  // usages 失败但拿到档位时,保留档位,窗口留空。
  if (tier) {
    quota = quota
      ? { ...quota, tier }
      : { tier, weekly: null, fiveHour: null, monthly: null, updatedAtMs: Date.now() };
  }
  if (!quota) {
    return rotated ? { privateData: data } : { state: { status: "ready" } };
  }
  return {
    privateData: { ...data, quota },
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

/** 额度与档位都是锦上添花:网络失败或非 JSON 响应一律当作查不到,不阻断刷新。 */
async function fetchJson(
  context: PluginContext,
  url: string,
  headers: Record<string, string>,
): Promise<Record<string, unknown> | null> {
  try {
    const response = await context.network.fetch(url, { method: "GET", headers });
    if (response.status < 200 || response.status >= 300) return null;
    return object(JSON.parse(response.body));
  } catch {
    return null;
  }
}

export function parseCredentialFiles(files: ResourceImportFile[]): {
  credentials: CredentialCandidate[];
  warnings: string[];
} {
  return parseCredentialFilesShared(files, "Kimi", "KIMI_API_KEY");
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
