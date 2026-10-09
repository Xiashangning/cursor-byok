import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import type { CredentialCandidate } from "cursor-byok:credentials";
import {
  jwtAccountIdentity,
  parseCredentialFiles as parseCredentialFilesShared,
} from "cursor-byok:credentials";
import { clampPercent, jwtDisplayName, number, object, text, timestampMs } from "cursor-byok:json";
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

export const RESOURCE_TYPE = "grok-account";

const CREDITS_URL = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const ONE_HOUR_MS = 60 * 60 * 1000;

export type AccountQuota = {
  /** 由 monthlyLimit(美分)阈值推导的档位;无法推导时为 null,不显示徽章。 */
  tier: string | null;
  /** 计费周期;按量账号或周期未知时为 null。 */
  period: "weekly" | "monthly" | null;
  remainingPercent: number | null;
  resetAtMs: number | null;
  limitReached: boolean;
  updatedAtMs: number;
};

/** 单条 grok-account 资源的 privateData 形状。 */
export type AccountData = {
  accessToken: string;
  refreshToken: string | null;
  displayName: string;
  quota: AccountQuota | null;
};

export function accountIdentity(
  accessToken: string,
): Promise<{ key: string; displayName: string }> {
  return jwtAccountIdentity(accessToken, "grok");
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
  if (!accessToken) throw new Error("Grok account resource is missing its access token");
  return {
    accessToken,
    refreshToken: text(data?.refreshToken),
    displayName: text(data?.displayName) ?? "Grok account",
    quota: (data?.quota ?? null) as AccountQuota | null,
  };
}

/** 金额字段容错解析:{"val": N}、数值或数值字符串三种包装。 */
function moneyValue(value: unknown): number | null {
  const wrapped = object(value);
  return number(wrapped ? wrapped.val : value);
}

/** monthlyLimit(美分)→ 官方档位徽章:$150 SuperGrok,$1500 SuperGrok Heavy。 */
export function planFromMonthlyLimit(limitCents: number | null): string | null {
  if (limitCents === null) return null;
  const rounded = Math.round(limitCents);
  if (rounded === 150_000) return "SUPERGROK HEAVY";
  if (rounded === 15_000) return "SUPERGROK";
  return null;
}

/**
 * 解析 Grok CLI 计费接口(?format=credits)响应;字段均为 config 下的 camelCase,
 * 形状与 sub2api 的 BillingConfig 一致:creditUsagePercent 是已用占比,
 * monthlyLimit/used 是美分,prepaid/on-demand 是美元。
 */
export function parseGrokUsage(body: unknown, nowMs = Date.now()): AccountQuota {
  const config = object(object(body)?.config) ?? {};
  const currentPeriod = object(config.currentPeriod);
  const periodName = text(currentPeriod?.type)?.toLowerCase() ?? "";
  let used = number(config.creditUsagePercent);
  if (used === null) {
    // 按量账号:on-demand 用量/上限换算。
    const onDemandUsed = moneyValue(config.onDemandUsed);
    const onDemandCap = moneyValue(config.onDemandCap);
    if (onDemandUsed !== null && onDemandCap !== null && onDemandCap > 0) {
      used = (onDemandUsed / onDemandCap) * 100;
    }
  }
  if (used === null) {
    // 月度预付账号:used/monthlyLimit 均为美分。
    const limit = moneyValue(config.monthlyLimit);
    const monthlyUsed = moneyValue(config.used);
    if (limit !== null && limit > 0 && monthlyUsed !== null) {
      used = (Math.min(monthlyUsed, limit) / limit) * 100;
    }
  }
  // 存在计费周期但没有用量字段时视为未使用。
  if (used === null && currentPeriod !== null) used = 0;
  const remaining = used === null ? null : clampPercent(100 - used);
  return {
    tier: planFromMonthlyLimit(moneyValue(config.monthlyLimit)),
    period: periodName.includes("weekly")
      ? "weekly"
      : periodName.includes("monthly")
      ? "monthly"
      : null,
    // 窗口重置只取 currentPeriod.end;billingPeriodEnd 是月度边界,不能回退。
    resetAtMs: timestampMs(currentPeriod?.end),
    remainingPercent: remaining,
    limitReached: remaining !== null && remaining <= 0,
    updatedAtMs: nowMs,
  };
}

export function quotaState(quota: AccountQuota | null, nowMs = Date.now()): ResourceState {
  if (!quota || !quota.limitReached) return { status: "ready" };
  if (quota.resetAtMs !== null && quota.resetAtMs <= nowMs) return { status: "ready" };
  return {
    status: "cooling",
    retryAtMs: quota.resetAtMs ?? nowMs + ONE_HOUR_MS,
    message: "Grok credits are exhausted",
  };
}

/** 额度耗尽时的资源补丁:标记积分耗尽并进入冷却,重置时间未知时回退 1 小时。 */
export function quotaExhaustedPatch(data: AccountData, nowMs = Date.now()): ResourcePatch {
  const quota: AccountQuota = {
    tier: data.quota?.tier ?? null,
    period: data.quota?.period ?? null,
    remainingPercent: 0,
    resetAtMs: data.quota?.resetAtMs != null && data.quota.resetAtMs > nowMs
      ? data.quota.resetAtMs
      : null,
    limitReached: true,
    updatedAtMs: nowMs,
  };
  return {
    privateData: { ...data, quota } as unknown as JsonValue,
    state: quotaState(quota, nowMs),
  };
}

export function accountHeaders(data: AccountData): Record<string, string> {
  return {
    accept: "application/json",
    authorization: `Bearer ${data.accessToken}`,
    // Grok CLI 计费接口要求这些头标识客户端来源;版本号与 https://x.ai/cli/stable 保持同步。
    "x-xai-token-auth": "xai-grok-cli",
    "x-grok-client-version": "1.0.46",
  };
}

export function presentAccount(resource: ResourceSnapshot): ResourceView {
  const data = accountData(resource);
  const metrics: ResourceMetric[] = [];
  const quota = data.quota;
  if (quota && quota.remainingPercent !== null) {
    const [id, en, zh] = quota.period === "weekly"
      ? ["weekly", "Weekly credits", "周额度"]
      : quota.period === "monthly"
      ? ["monthly", "Monthly credits", "月额度"]
      : ["credits", "Credits", "积分额度"];
    metrics.push({
      id,
      label: { "en-US": en, "zh-CN": zh },
      unit: "percent",
      value: quota.remainingPercent,
      ...(quota.resetAtMs !== null ? { resetAtMs: quota.resetAtMs } : {}),
    });
  }
  return {
    // 旧记录可能存的是账号 ID;展示时优先从 token 现算邮箱。
    displayName: jwtDisplayName(data.accessToken) ?? data.displayName,
    ...(quota?.tier ? { tier: quota.tier } : {}),
    ...(metrics.length > 0 ? { metrics } : {}),
  };
}

export async function refreshAccount(
  resource: ResourceSnapshot,
  context: PluginContext,
): Promise<ResourcePatch> {
  const data = accountData(resource);
  const response = await context.network.fetch(CREDITS_URL, {
    method: "GET",
    headers: accountHeaders(data),
  });
  if (response.status < 200 || response.status >= 300) {
    if (response.status === 401 || response.status === 403) {
      return {
        state: { status: "invalid", message: "Grok authorization expired; sign in again" },
      };
    }
    throw new Error(`Grok usage lookup failed (HTTP ${response.status}): ${response.body}`);
  }
  let body: unknown;
  try {
    body = JSON.parse(response.body);
  } catch {
    throw new Error("Grok usage lookup returned invalid JSON");
  }
  const quota = parseGrokUsage(body);
  return {
    privateData: { ...data, quota } as unknown as JsonValue,
    state: quotaState(quota),
  };
}

export function parseCredentialFiles(files: ResourceImportFile[]): {
  credentials: CredentialCandidate[];
  warnings: string[];
} {
  return parseCredentialFilesShared(files, "Grok", "XAI_API_KEY");
}

export const credentialImport: ResourceImportSupport = {
  displayName: {
    "en-US": "Import Grok credentials",
    "zh-CN": "导入 Grok 凭证",
  },
  description: {
    "en-US": "Import one or more Grok JSON credential files.",
    "zh-CN": "导入一个或多个 Grok JSON 凭证文件。",
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
