import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import type { CredentialCandidate } from "cursor-byok:credentials";
import { parseCredentialFiles as parseCredentialFilesShared } from "cursor-byok:credentials";
import {
  clampPercent,
  decodeJwtPayload,
  jwtClaim,
  number,
  object,
  text,
  timestampMs,
  tokenFingerprint,
} from "cursor-byok:json";
import type {
  QuotaWindow,
  ResourceAction,
  ResourceActionCard,
  ResourceActionResult,
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

export const RESOURCE_TYPE = "chatgpt-account";

const USAGE_URL = "https://chatgpt.com/backend-api/wham/usage";
const RESET_CREDITS_URL = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const RESET_CREDITS_CONSUME_URL = `${RESET_CREDITS_URL}/consume`;
const FIVE_HOURS_MS = 5 * 60 * 60 * 1000;

export const LIST_RESET_CARDS_ACTION_ID = "list-reset-cards";
export const CONSUME_RESET_CARD_ACTION_ID = "consume-reset-card";

export type { CredentialCandidate, QuotaWindow };

export type AccountQuota = {
  /** 官方 plan_type 枚举原始值(如 plus/business);presentAccount 据此现算档位徽章。 */
  planType: string | null;
  weekly: QuotaWindow | null;
  fiveHour: QuotaWindow | null;
  /** 超过一周的长窗口归为月度(如部分工作区订阅);接口不提供时为 null。 */
  monthly: QuotaWindow | null;
  resetCreditsAvailable: number | null;
  limitReached: boolean;
  updatedAtMs: number;
};

/** 单条 chatgpt-account 资源的 privateData 形状。 */
export type AccountData = {
  accessToken: string;
  refreshToken: string | null;
  accountId: string | null;
  displayName: string;
  quota: AccountQuota | null;
};

export function chatGptAccountId(accessToken: string): string | null {
  const payload = decodeJwtPayload(accessToken);
  const auth = object(payload?.["https://api.openai.com/auth"]);
  return text(auth?.chatgpt_account_id) ?? jwtClaim(payload, "chatgpt_account_id");
}

/** ChatGPT access token 的邮箱通常在 OpenAI 的 profile 声明里,而不是顶层 email。 */
function profileEmail(payload: Record<string, unknown> | null): string | null {
  const profile = object(payload?.["https://api.openai.com/profile"]);
  return text(profile?.email);
}

export async function accountIdentity(
  accessToken: string,
  accountId?: string | null,
): Promise<{ key: string; displayName: string }> {
  const payload = decodeJwtPayload(accessToken);
  const identity = accountId ?? chatGptAccountId(accessToken) ??
    jwtClaim(payload, "sub") ??
    jwtClaim(payload, "email") ??
    await tokenFingerprint(accessToken);
  const displayName = jwtClaim(payload, "email") ??
    profileEmail(payload) ??
    jwtClaim(payload, "preferred_username") ??
    jwtClaim(payload, "name") ??
    identity;
  return { key: `codex:${identity}`, displayName };
}

export async function credentialDraft(credential: CredentialCandidate): Promise<ResourceDraft> {
  const identity = await accountIdentity(credential.accessToken, credential.accountId);
  const data: AccountData = {
    accessToken: credential.accessToken,
    refreshToken: credential.refreshToken,
    accountId: credential.accountId ?? chatGptAccountId(credential.accessToken),
    displayName: credential.displayName ?? identity.displayName,
    quota: null,
  };
  return { key: identity.key, privateData: data as unknown as JsonValue };
}

export function accountData(resource: ResourceSnapshot): AccountData {
  const data = object(resource.privateData);
  const accessToken = text(data?.accessToken);
  if (!accessToken) throw new Error("ChatGPT account resource is missing its access token");
  return {
    accessToken,
    refreshToken: text(data?.refreshToken),
    accountId: text(data?.accountId) ?? chatGptAccountId(accessToken),
    displayName: text(data?.displayName) ?? "ChatGPT account",
    quota: (data?.quota ?? null) as AccountQuota | null,
  };
}

export function accountHeaders(data: AccountData): Record<string, string> {
  const headers: Record<string, string> = {
    accept: "application/json",
    originator: "codex_cli_rs",
    authorization: `Bearer ${data.accessToken}`,
  };
  const accountId = data.accountId ?? chatGptAccountId(data.accessToken);
  if (accountId) headers["ChatGPT-Account-Id"] = accountId;
  return headers;
}

function resetAtMs(window: Record<string, unknown>, nowMs: number): number | null {
  const explicit = timestampMs(window.reset_at);
  if (explicit !== null) return explicit;
  const afterSeconds = number(window.reset_after_seconds);
  return afterSeconds === null ? null : nowMs + afterSeconds * 1000;
}

function quotaWindow(value: unknown, nowMs: number): QuotaWindow | null {
  const window = object(value);
  if (!window) return null;
  const used = number(window.used_percent);
  const remaining = used === null ? number(window.remaining_percent) : clampPercent(100 - used);
  return {
    remainingPercent: remaining === null ? null : clampPercent(remaining),
    resetAtMs: resetAtMs(window, nowMs),
  };
}

const FIVE_HOURS_S = FIVE_HOURS_MS / 1000;
const WEEK_S = 7 * 24 * 60 * 60;

/** 按官方 limit_window_seconds 判定窗口类型;长度未知时回退到 primary/secondary 位置。 */
function windowKind(
  window: Record<string, unknown>,
  fallback: "five-hour" | "weekly",
): "five-hour" | "weekly" | "monthly" {
  const seconds = number(window.limit_window_seconds);
  if (seconds !== null) {
    if (seconds <= FIVE_HOURS_S) return "five-hour";
    if (seconds <= WEEK_S) return "weekly";
    return "monthly";
  }
  return fallback;
}

export function parseCodexUsage(body: unknown, nowMs = Date.now()): AccountQuota {
  const root = object(body) ?? {};
  // 上游字段均为 snake_case(与 codex CLI / sub2api 的结构体一致)。
  const rateLimit = object(root.rate_limit) ?? {};
  const primaryValue = rateLimit.primary_window;
  const secondaryValue = rateLimit.secondary_window;
  const primaryObject = object(primaryValue);
  const secondaryObject = object(secondaryValue);
  let fiveHour: QuotaWindow | null = null;
  let weekly: QuotaWindow | null = null;
  let monthly: QuotaWindow | null = null;
  if (secondaryObject !== null) {
    // 双窗口:按窗口长度归类,而不是按位置猜。
    const primaryKind = windowKind(primaryObject ?? {}, "five-hour");
    const secondaryKind = windowKind(secondaryObject, "weekly");
    const primaryWindow = quotaWindow(primaryValue, nowMs);
    const secondaryWindow = quotaWindow(secondaryValue, nowMs);
    if (primaryKind === "five-hour") fiveHour = primaryWindow;
    else if (primaryKind === "weekly") weekly = primaryWindow;
    else monthly = primaryWindow;
    if (secondaryKind === "five-hour") fiveHour = fiveHour ?? secondaryWindow;
    else if (secondaryKind === "weekly") weekly = weekly ?? secondaryWindow;
    else monthly = monthly ?? secondaryWindow;
  } else if (primaryObject !== null) {
    const kind = windowKind(primaryObject, "weekly");
    const window = quotaWindow(primaryValue, nowMs);
    if (kind === "five-hour") fiveHour = window;
    else if (kind === "weekly") weekly = window;
    else monthly = window;
  }
  const explicitLimit = rateLimit.limit_reached;
  const resetCredits = object(root.rate_limit_reset_credits);
  const resetCreditsAvailable = number(resetCredits?.available_count);
  return {
    planType: text(root.plan_type),
    weekly,
    fiveHour,
    monthly,
    resetCreditsAvailable: resetCreditsAvailable === null
      ? null
      : Math.max(0, Math.floor(resetCreditsAvailable)),
    limitReached: typeof explicitLimit === "boolean" ? explicitLimit : [weekly, fiveHour].some(
      (window) => window?.remainingPercent !== null && window?.remainingPercent === 0,
    ),
    updatedAtMs: nowMs,
  };
}

function windowCoolingUntil(window: QuotaWindow | null, nowMs: number): number | null {
  if (!window || window.remainingPercent === null || window.remainingPercent > 0) return null;
  if (window.resetAtMs !== null && window.resetAtMs <= nowMs) return null;
  return window.resetAtMs ?? nowMs + FIVE_HOURS_MS;
}

export function quotaCoolingUntil(quota: AccountQuota, nowMs = Date.now()): number | null {
  const resets = [
    windowCoolingUntil(quota.weekly, nowMs),
    windowCoolingUntil(quota.fiveHour, nowMs),
    windowCoolingUntil(quota.monthly, nowMs),
  ].filter((value): value is number => value !== null);
  if (resets.length > 0) return Math.max(...resets);
  return quota.limitReached ? nowMs + FIVE_HOURS_MS : null;
}

export function quotaState(quota: AccountQuota | null, nowMs = Date.now()): ResourceState {
  if (!quota) return { status: "ready" };
  const coolingUntil = quotaCoolingUntil(quota, nowMs);
  return coolingUntil === null
    ? { status: "ready" }
    : { status: "cooling", retryAtMs: coolingUntil, message: "ChatGPT quota is exhausted" };
}

/** 从上游错误文本中提取重置时间;拿不到时回退 5 小时。 */
function resetFromError(error: string, nowMs: number): number {
  const resetAt = error.match(/["']?reset_at["']?\s*[:=]\s*["']?(\d+(?:\.\d+)?)/i)?.[1];
  if (resetAt) {
    const value = Number(resetAt);
    if (Number.isFinite(value)) return value > 10_000_000_000 ? value : value * 1000;
  }
  const resetAfter = error.match(/["']?reset_after_seconds["']?\s*[:=]\s*["']?(\d+(?:\.\d+)?)/i)
    ?.[1];
  if (resetAfter) {
    const value = Number(resetAfter);
    if (Number.isFinite(value)) return nowMs + value * 1000;
  }
  return nowMs + FIVE_HOURS_MS;
}

/** 额度耗尽时的资源补丁:标记 5 小时窗口耗尽并按重置时间进入冷却。 */
export function quotaExhaustedPatch(
  data: AccountData,
  error: string,
  nowMs = Date.now(),
): ResourcePatch {
  const quota: AccountQuota = {
    planType: data.quota?.planType ?? null,
    weekly: data.quota?.weekly ?? null,
    fiveHour: {
      remainingPercent: 0,
      resetAtMs: resetFromError(error, nowMs),
    },
    monthly: data.quota?.monthly ?? null,
    resetCreditsAvailable: data.quota?.resetCreditsAvailable ?? null,
    limitReached: true,
    updatedAtMs: nowMs,
  };
  return {
    privateData: { ...data, quota } as unknown as JsonValue,
    state: quotaState(quota, nowMs),
  };
}

async function fetchResetCredits(
  data: AccountData,
  context: PluginContext,
): Promise<{ cards: ResourceActionCard[]; availableCount: number }> {
  const accountId = data.accountId ?? chatGptAccountId(data.accessToken);
  if (!accountId) throw new Error("ChatGPT account is missing its account ID");
  const response = await context.network.fetch(RESET_CREDITS_URL, {
    method: "GET",
    headers: accountHeaders(data),
  });
  if (response.status < 200 || response.status >= 300) {
    throw new Error(
      `Codex reset card lookup failed (HTTP ${response.status}): ${response.body}`,
    );
  }
  let body: unknown;
  try {
    body = JSON.parse(response.body);
  } catch {
    throw new Error("Codex reset card lookup returned invalid JSON");
  }
  const root = object(body) ?? {};
  const rawCredits = Array.isArray(root.credits) ? root.credits : [];
  const cards = rawCredits.flatMap((value, index): ResourceActionCard[] => {
    const credit = object(value);
    const id = text(credit?.id);
    if (!id) return [];
    const resetType = text(credit?.reset_type ?? credit?.resetType);
    const grantedAt = timestampMs(credit?.granted_at ?? credit?.grantedAt);
    const expiresAt = timestampMs(credit?.expires_at ?? credit?.expiresAt);
    return [{
      id,
      title: text(credit?.title) ?? resetType ?? `Codex reset card ${index + 1}`,
      ...(text(credit?.status) ? { status: text(credit?.status)! } : {}),
      ...(grantedAt !== null ? { grantedAtMs: grantedAt } : {}),
      ...(expiresAt !== null ? { expiresAtMs: expiresAt } : {}),
      fields: resetType
        ? [{
          id: "reset-type",
          label: { "en-US": "Reset type", "zh-CN": "重置类型" },
          value: resetType,
        }]
        : [],
    }];
  });
  const availableCount = number(root.available_count ?? root.availableCount);
  return {
    cards,
    availableCount: availableCount === null
      ? cards.filter((card) => card.status === "available").length
      : Math.max(0, Math.floor(availableCount)),
  };
}

function actionDescription(availableCount: number): ResourceActionResult["description"] {
  return {
    "en-US": `${availableCount} reset card${availableCount === 1 ? "" : "s"} available`,
    "zh-CN": `可用重置卡 ${availableCount} 张`,
  };
}

async function listResetCards(
  resource: ResourceSnapshot,
  _input: JsonValue,
  context: PluginContext,
): Promise<ResourceActionResult> {
  const result = await fetchResetCredits(accountData(resource), context);
  return {
    title: { "en-US": "Codex reset cards", "zh-CN": "Codex 重置卡" },
    description: actionDescription(result.availableCount),
    cards: result.cards,
  };
}

async function consumeResetCard(
  resource: ResourceSnapshot,
  input: JsonValue,
  context: PluginContext,
): Promise<ResourceActionResult> {
  const inputObject = object(input);
  const creditId = text(inputObject?.creditId ?? inputObject?.cardId);
  if (!creditId) throw new Error("A reset card ID is required");

  const data = accountData(resource);
  const available = await fetchResetCredits(data, context);
  const card = available.cards.find((item) => item.id === creditId && item.status === "available");
  if (!card) throw new Error("The selected reset card is not available");

  const response = await context.network.fetch(RESET_CREDITS_CONSUME_URL, {
    method: "POST",
    headers: { ...accountHeaders(data), "content-type": "application/json" },
    body: JSON.stringify({ credit_id: creditId, redeem_request_id: crypto.randomUUID() }),
  });
  if (response.status < 200 || response.status >= 300) {
    throw new Error(
      `Codex reset card consumption failed (HTTP ${response.status}): ${response.body}`,
    );
  }

  const patch = await refreshAccount(resource, context);
  const refreshedResource: ResourceSnapshot = {
    ...resource,
    ...(patch.privateData ? { privateData: patch.privateData } : {}),
  };
  const refreshed = await fetchResetCredits(accountData(refreshedResource), context);
  return {
    title: { "en-US": "Codex reset card used", "zh-CN": "Codex 重置卡已使用" },
    description: actionDescription(refreshed.availableCount),
    cards: refreshed.cards,
    patch,
  };
}

export const listResetCardsAction: ResourceAction = {
  id: LIST_RESET_CARDS_ACTION_ID,
  displayName: { "en-US": "View reset cards", "zh-CN": "查看重置卡" },
  description: {
    "en-US": "List available Codex reset cards.",
    "zh-CN": "查看当前账号的 Codex 重置卡。",
  },
  target: "resource",
  run: listResetCards,
};

export const consumeResetCardAction: ResourceAction = {
  id: CONSUME_RESET_CARD_ACTION_ID,
  displayName: { "en-US": "Use reset card", "zh-CN": "使用重置卡" },
  description: {
    "en-US": "Redeem one available Codex reset card.",
    "zh-CN": "消耗一张可用的 Codex 重置卡。",
  },
  target: "card",
  destructive: true,
  run: consumeResetCard,
};

/** 官方 plan_type 枚举 → 桌面档位徽章文案(大写)。仅列出非简单大写的重命名,其余由 fallback 大写。 */
export function planTier(value: unknown): string | null {
  const plan = text(value)?.toLowerCase();
  if (!plan) return null;
  const renames: Record<string, string> = {
    team: "BUSINESS", // team 已更名 business,旧值仍按 BUSINESS 显示
    promax: "PRO MAX",
    prolite: "PRO",
  };
  return renames[plan] ?? plan.toUpperCase();
}

export function presentAccount(resource: ResourceSnapshot): ResourceView {
  const data = accountData(resource);
  const metrics = quotaWindowMetrics(data.quota ?? {});
  const resetCreditsAvailable = data.quota?.resetCreditsAvailable;
  if (resetCreditsAvailable !== null && resetCreditsAvailable !== undefined) {
    metrics.push({
      id: "reset-credits",
      label: { "en-US": "Reset cards", "zh-CN": "重置卡" },
      unit: "count",
      value: resetCreditsAvailable,
    });
  }
  const tier = planTier(data.quota?.planType);
  return {
    // 旧记录可能存的是账号 ID;展示时优先从 token 现算邮箱。
    displayName: jwtDisplayName(data.accessToken) ?? data.displayName,
    ...(tier ? { tier } : {}),
    ...(metrics.length > 0 ? { metrics } : {}),
  };
}

export async function refreshAccount(
  resource: ResourceSnapshot,
  context: PluginContext,
): Promise<ResourcePatch> {
  const data = accountData(resource);
  const response = await context.network.fetch(USAGE_URL, {
    method: "GET",
    headers: accountHeaders(data),
  });
  if (response.status < 200 || response.status >= 300) {
    if (response.status === 401) {
      return {
        state: { status: "invalid", message: "ChatGPT authorization expired; sign in again" },
      };
    }
    throw new Error(`Codex usage lookup failed (HTTP ${response.status}): ${response.body}`);
  }
  let body: unknown;
  try {
    body = JSON.parse(response.body);
  } catch {
    throw new Error("Codex usage lookup returned invalid JSON");
  }
  const quota = parseCodexUsage(body);
  return {
    privateData: { ...data, quota } as unknown as JsonValue,
    state: quotaState(quota),
  };
}

function jwtDisplayName(token: string | null): string | null {
  if (!token) return null;
  const payload = decodeJwtPayload(token);
  return jwtClaim(payload, "email") ?? profileEmail(payload) ??
    jwtClaim(payload, "preferred_username") ?? jwtClaim(payload, "name");
}

export function parseCredentialFiles(files: ResourceImportFile[]): {
  credentials: CredentialCandidate[];
  warnings: string[];
} {
  const result = parseCredentialFilesShared(files, "ChatGPT", "OPENAI_API_KEY");
  // ChatGPT 的展示名常在 id_token 的 OpenAI profile 声明里,共享收集器读不到。
  for (const credential of result.credentials) {
    credential.displayName = credential.displayName ?? jwtDisplayName(credential.idToken ?? null);
  }
  return result;
}

export const credentialImport: ResourceImportSupport = {
  displayName: {
    "en-US": "Import Codex credentials",
    "zh-CN": "导入 Codex 凭证",
  },
  description: {
    "en-US": "Import one or more Codex JSON credential files.",
    "zh-CN": "导入一个或多个 Codex JSON 凭证文件。",
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
