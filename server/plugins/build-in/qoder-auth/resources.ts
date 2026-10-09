import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import {
  boolean,
  clampPercent,
  number,
  object,
  parseJsonObject,
  text,
  tokenFingerprint,
} from "cursor-byok:json";
import type {
  ResourceDraft,
  ResourceMetric,
  ResourcePatch,
  ResourceSnapshot,
  ResourceState,
  ResourceView,
} from "cursor-byok:resource";
import type { CosyIdentity } from "./sign.ts";

export const RESOURCE_TYPE = "qoder-account";

const REFRESH_URL = "https://openapi.qoder.sh/api/v1/deviceToken/refresh";
const USERINFO_URL = "https://openapi.qoder.sh/api/v1/userinfo";
const STATUS_URL = "https://openapi.qoder.sh/api/v3/user/status";
const QUOTA_USAGE_URL = "https://openapi.qoder.sh/api/v2/quota/usage";
const ACCESS_REFRESH_SKEW_MS = 60 * 60 * 1000;
const DEFAULT_COOLING_MS = 5 * 60 * 1000;
const EXPIRED_MESSAGE = "Qoder authorization expired; sign in again";

export type AccountProfile = {
  id: string | null;
  name: string | null;
  email: string | null;
  organizationId: string | null;
  organizationName: string | null;
  organizationTags: string[];
  userType: string | null;
  dataPolicyAgreed: boolean | null;
};

export type AccountQuota = {
  planLabel: string | null;
  userTag: string | null;
  remainingPercent: number | null;
  remainingCount: number | null;
  /** 已用额度数值;Teams/Enterprise 为个人+团队池合计。 */
  usedCount: number | null;
  /** 月度积分池总量,来自 userQuota.total;有总量时桌面端显示 剩余/总量。 */
  totalCount: number | null;
  /** 是否为 Teams/Enterprise 池化账号且团队池可用;决定指标拆分方式。 */
  pooled: boolean;
  exceeded: boolean;
  resetAtMs: number | null;
  updatedAtMs: number;
};

export type AccountData = {
  accessToken: string;
  refreshToken: string | null;
  userId: string | null;
  userName: string | null;
  machineId: string;
  accessExpiresAtMs: number | null;
  refreshExpiresAtMs: number | null;
  profile: AccountProfile | null;
  quota: AccountQuota | null;
  /** Account kind from `/api/v3/user/status` (e.g. "teams"); signing needs it. */
  userType: string | null;
};

export type CredentialCandidate = {
  accessToken: string;
  refreshToken: string | null;
  userId: string | null;
  userName: string | null;
  machineId: string;
  accessExpiresAtMs: number | null;
  refreshExpiresAtMs: number | null;
};

export type TokenBundle = Omit<CredentialCandidate, "machineId">;

const ACCOUNT_CACHE_SIZE = 256;
const latestAccounts = new Map<string, AccountData>();
const pendingRefreshes = new Map<string, Promise<AccountData | null>>();

function timestampMs(value: unknown): number | null {
  const numeric = number(value);
  if (numeric !== null && numeric > 0) {
    return numeric > 10_000_000_000 ? numeric : numeric * 1000;
  }
  if (typeof value === "string") {
    const parsed = Date.parse(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return null;
}

function expiryMs(absolute: unknown, relativeSeconds: unknown, nowMs: number): number | null {
  const explicit = timestampMs(absolute);
  if (explicit !== null) return explicit;
  const seconds = number(relativeSeconds);
  return seconds !== null && seconds > 0 ? nowMs + seconds * 1000 : null;
}

/** Normalize both device-poll and refresh token response spellings. */
export function readTokenBundle(body: unknown, nowMs = Date.now()): TokenBundle | null {
  const root = object(body);
  if (!root) return null;
  const accessToken = text(root.device_token ?? root.token ?? root.access_token);
  if (!accessToken) return null;
  return {
    accessToken,
    refreshToken: text(root.refresh_token ?? root.refreshToken),
    userId: text(root.user_id ?? root.userId ?? root.uid),
    userName: text(root.user_name ?? root.userName ?? root.name),
    accessExpiresAtMs: expiryMs(
      root.expires_at ?? root.expiresAt,
      root.expires_in ?? root.expiresIn,
      nowMs,
    ),
    refreshExpiresAtMs: expiryMs(
      root.refresh_token_expires_at ?? root.refreshTokenExpiresAt,
      root.refresh_token_expires_in ?? root.refreshTokenExpiresIn,
      nowMs,
    ),
  };
}

function privateData(data: AccountData): JsonValue {
  return {
    security_oauth_token: data.accessToken,
    refresh_token: data.refreshToken,
    user_id: data.userId,
    user_name: data.userName,
    machine_id: data.machineId,
    access_expires_at_ms: data.accessExpiresAtMs,
    refresh_token_expires_at_ms: data.refreshExpiresAtMs,
    profile: data.profile as unknown as JsonValue,
    quota: data.quota as unknown as JsonValue,
    user_type: data.userType,
  };
}

export function accountPrivateData(data: AccountData): JsonValue {
  return privateData(data);
}

export async function credentialDraft(credential: CredentialCandidate): Promise<ResourceDraft> {
  const identity = credential.userId ?? await tokenFingerprint(credential.accessToken);
  const data: AccountData = {
    ...credential,
    profile: null,
    quota: null,
    userType: null,
  };
  return {
    key: `qoder:${identity}`,
    privateData: privateData(data),
  };
}

function storedProfile(value: unknown): AccountProfile | null {
  const profile = object(value);
  if (!profile) return null;
  return {
    id: text(profile.id),
    name: text(profile.name),
    email: text(profile.email),
    organizationId: text(profile.organizationId),
    organizationName: text(profile.organizationName),
    organizationTags: Array.isArray(profile.organizationTags)
      ? profile.organizationTags.flatMap((item) => text(item) ?? [])
      : [],
    userType: text(profile.userType),
    dataPolicyAgreed: boolean(profile.dataPolicyAgreed),
  };
}

function storedQuota(value: unknown): AccountQuota | null {
  const quota = object(value);
  if (!quota) return null;
  return {
    planLabel: text(quota.planLabel),
    userTag: text(quota.userTag),
    remainingPercent: number(quota.remainingPercent),
    remainingCount: number(quota.remainingCount),
    usedCount: number(quota.usedCount),
    totalCount: number(quota.totalCount),
    pooled: boolean(quota.pooled) ?? false,
    exceeded: boolean(quota.exceeded) ?? false,
    resetAtMs: number(quota.resetAtMs),
    updatedAtMs: number(quota.updatedAtMs) ?? 0,
  };
}

export function accountData(resource: ResourceSnapshot): AccountData {
  const data = object(resource.privateData);
  const accessToken = text(data?.security_oauth_token ?? data?.access_token);
  if (!accessToken) throw new Error("Qoder account resource is missing its access token");
  return {
    accessToken,
    refreshToken: text(data?.refresh_token),
    userId: text(data?.user_id),
    userName: text(data?.user_name),
    machineId: text(data?.machine_id) ?? "",
    accessExpiresAtMs: number(data?.access_expires_at_ms),
    refreshExpiresAtMs: number(data?.refresh_token_expires_at_ms),
    profile: storedProfile(data?.profile),
    quota: storedQuota(data?.quota),
    userType: text(data?.user_type),
  };
}

function rememberAccountData(resourceId: string, data: AccountData): AccountData {
  latestAccounts.delete(resourceId);
  latestAccounts.set(resourceId, data);
  if (latestAccounts.size > ACCOUNT_CACHE_SIZE) {
    const oldest = latestAccounts.keys().next().value;
    if (oldest !== undefined) latestAccounts.delete(oldest);
  }
  return data;
}

export function latestAccountData(resourceId: string, data: AccountData): AccountData {
  const latest = latestAccounts.get(resourceId);
  if (!latest || latest.machineId !== data.machineId) {
    return rememberAccountData(resourceId, data);
  }
  return latest;
}

/** Refresh before the official client enters its one-hour expiry window. */
export function accessTokenExpiring(data: AccountData, nowMs = Date.now()): boolean {
  return data.accessExpiresAtMs !== null && data.accessExpiresAtMs > 0 &&
    data.accessExpiresAtMs <= nowMs + ACCESS_REFRESH_SKEW_MS;
}

function refreshTokenExpired(data: AccountData, nowMs = Date.now()): boolean {
  return data.refreshExpiresAtMs !== null && data.refreshExpiresAtMs > 0 &&
    data.refreshExpiresAtMs <= nowMs;
}

function invalidTokenBody(body: Record<string, unknown>, rawBody = ""): boolean {
  const error = object(body.error);
  // Underscores and dashes are normalized so `invalid_token`, `invalid-token`
  // and `invalid refresh_token` all classify as a definitive rejection.
  const value = [
    rawBody,
    body.code,
    body.error,
    body.message,
    body.error_description,
    error?.code,
    error?.type,
    error?.message,
  ].flatMap((item) => text(item) ?? []).join(" ").toLowerCase().replace(/[_-]/g, " ");
  return value.includes("invalid token") || value.includes("invalid grant") ||
    value.includes("expired token") || value.includes("refresh token expired") ||
    value.includes("invalid refresh token") || value.includes("refresh token invalid") ||
    value.includes("login expired");
}

/** Refresh the access token once. Null is a definitive credential rejection. */
export async function refreshAccessToken(
  data: AccountData,
  context: PluginContext,
): Promise<AccountData | null> {
  if (!data.refreshToken || refreshTokenExpired(data)) return null;
  const request: Record<string, string> = { refresh_token: data.refreshToken };
  if (data.machineId) request.machine_id = data.machineId;
  const response = await context.network.fetch(REFRESH_URL, {
    method: "POST",
    headers: { Accept: "application/json", "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
  const body = parseJsonObject(response.body);
  if (
    response.status === 401 || response.status === 403 || invalidTokenBody(body, response.body)
  ) return null;
  if (response.status < 200 || response.status >= 300) {
    throw new Error(`Qoder token refresh failed (HTTP ${response.status}): ${response.body}`);
  }
  const bundle = readTokenBundle(body);
  if (!bundle) throw new Error("Qoder token refresh response is missing a token");
  return {
    ...data,
    accessToken: bundle.accessToken,
    refreshToken: bundle.refreshToken ?? data.refreshToken,
    userId: bundle.userId ?? data.userId,
    userName: bundle.userName ?? data.userName,
    accessExpiresAtMs: bundle.accessExpiresAtMs,
    refreshExpiresAtMs: bundle.refreshExpiresAtMs ?? data.refreshExpiresAtMs,
  };
}

/** Coalesce concurrent rotation attempts that still hold the same refresh token. */
export function sharedRefreshAccessToken(
  resourceId: string,
  data: AccountData,
  context: PluginContext,
): Promise<AccountData | null> {
  const current = latestAccountData(resourceId, data);
  if (!current.refreshToken || refreshTokenExpired(current)) return Promise.resolve(null);
  const key = `${resourceId}\u0000${current.refreshToken}`;
  const pending = pendingRefreshes.get(key);
  if (pending) return pending;
  const promise = refreshAccessToken(current, context)
    .then((next) => next === null ? null : rememberAccountData(resourceId, next))
    .finally(() => pendingRefreshes.delete(key));
  pendingRefreshes.set(key, promise);
  return promise;
}

export function parseUserInfo(body: unknown): AccountProfile {
  const root = object(body) ?? {};
  const source = object(root.data) ?? root;
  return {
    id: text(source.id ?? source.user_id ?? source.uid),
    name: text(source.name ?? source.username),
    email: text(source.email),
    organizationId: text(source.organization_id),
    organizationName: text(source.organization_name),
    organizationTags: Array.isArray(source.organization_tags)
      ? source.organization_tags.flatMap((item) => text(item) ?? [])
      : [],
    userType: text(source.user_type),
    dataPolicyAgreed: boolean(source.data_policy_agreed),
  };
}

export function parseQoderStatus(body: unknown, nowMs = Date.now()): AccountQuota {
  const rootObject = object(body) ?? {};
  const root = object(rootObject.data) ?? rootObject;
  const quotaValue = root.quota;
  const quota = object(quotaValue);
  const total = number(quota?.total);
  const remaining = number(quota?.remaining);
  const used = number(quota?.used);
  const exceeded = boolean(root.isQuotaExceeded) ?? false;
  const userType = text(root.userType ?? root.user_type)?.toLowerCase() ?? "";
  const pooled = userType === "teams" || userType === "enterprise";
  let remainingPercent = number(quota?.remainingPercent ?? quota?.remaining_percent);
  if (remainingPercent === null && total !== null && total > 0) {
    if (remaining !== null) remainingPercent = (remaining / total) * 100;
    else if (used !== null) remainingPercent = (1 - used / total) * 100;
  }
  let remainingCount = remaining ?? (quota === null ? number(quotaValue) : null);
  if (exceeded) {
    remainingPercent = remainingPercent === null ? null : 0;
    remainingCount = 0;
  } else if (pooled) {
    remainingPercent = null;
    remainingCount = null;
  }
  return {
    planLabel: text(root.plan ?? root.userType),
    userTag: text(root.userTag),
    remainingPercent: remainingPercent === null ? null : clampPercent(remainingPercent),
    remainingCount,
    usedCount: used === null ? null : Math.max(0, Math.round(used)),
    totalCount: total !== null && total > 0 ? total : null,
    pooled,
    exceeded,
    resetAtMs: timestampMs(root.nextResetAt ?? quota?.nextResetAt ?? quota?.resetAt),
    updatedAtMs: nowMs,
  };
}

function parseUserType(body: unknown): string | null {
  const root = object(body) ?? {};
  return text(root.userType ?? root.user_type);
}

/**
 * Account credits from `/api/v2/quota/usage`. This endpoint carries real
 * numbers for personal, teams and enterprise accounts, unlike the single
 * `quota` field of `/api/v3/user/status`, which is 0 for pooled plans.
 */
export function parseQuotaUsage(body: unknown, nowMs = Date.now()): AccountQuota {
  const root = object(body) ?? {};
  const userQuota = object(root.userQuota);
  const totalPercentage = number(root.totalUsagePercentage) ?? number(userQuota?.percentage);
  const exceeded = boolean(root.isQuotaExceeded) ?? false;
  const remainingPercent = totalPercentage === null ? null : clampPercent(100 - totalPercentage);

  const userRemaining = number(userQuota?.remaining);
  const userUsed = number(userQuota?.used);
  const total = number(userQuota?.total);

  // Teams/Enterprise 池化账号:团队池可用时,已用/剩余为个人+团队池合计。
  const org = object(root.orgResourcePackage);
  const orgAvailable = boolean(org?.available) ?? false;
  const orgUsed = number(org?.used);
  const orgRemaining = number(org?.remaining);
  // cap != -1 表示有上限,团队池剩余用 cap;cap = -1 表示无上限,用 remaining。
  const orgCap = number(org?.cap);
  const orgEffectiveRemaining = orgCap !== null && orgCap !== -1 ? orgCap : orgRemaining;
  const pooled = orgAvailable && org !== null;

  const usedCount = pooled ? (userUsed ?? 0) + (orgUsed ?? 0) : userUsed;
  // 额度耗尽时已用保持实测合计,仅剩余归零。
  const remainingCount = exceeded
    ? 0
    : pooled
    ? (userRemaining ?? 0) + (orgEffectiveRemaining ?? 0)
    : userRemaining;
  return {
    planLabel: null,
    userTag: null,
    remainingPercent: exceeded ? 0 : remainingPercent,
    remainingCount,
    usedCount,
    totalCount: total !== null && total > 0 ? total : null,
    pooled,
    exceeded,
    resetAtMs: timestampMs(root.expiresAt ?? root.nextResetAt),
    updatedAtMs: nowMs,
  };
}

export function quotaState(quota: AccountQuota | null, nowMs = Date.now()): ResourceState {
  if (!quota?.exceeded) return { status: "ready" };
  const retryAtMs = quota.resetAtMs !== null && quota.resetAtMs > nowMs
    ? quota.resetAtMs
    : nowMs + DEFAULT_COOLING_MS;
  return { status: "cooling", retryAtMs, message: "Qoder quota is exhausted" };
}

export function quotaExhaustedPatch(
  data: AccountData,
  retryAtMs?: number,
  nowMs = Date.now(),
): ResourcePatch {
  const quota: AccountQuota = {
    planLabel: data.quota?.planLabel ?? null,
    userTag: data.quota?.userTag ?? null,
    remainingPercent: 0,
    remainingCount: 0,
    usedCount: data.quota?.usedCount ?? null,
    totalCount: data.quota?.totalCount ?? null,
    pooled: data.quota?.pooled ?? false,
    exceeded: true,
    resetAtMs: retryAtMs ?? data.quota?.resetAtMs ?? nowMs + DEFAULT_COOLING_MS,
    updatedAtMs: nowMs,
  };
  return { privateData: privateData({ ...data, quota }), state: quotaState(quota, nowMs) };
}

export function coolingPatch(
  data: AccountData,
  retryAtMs: number,
  message: string,
): ResourcePatch {
  return {
    privateData: privateData(data),
    state: { status: "cooling", retryAtMs, message },
  };
}

function accountDisplayName(data: AccountData): string {
  return data.profile?.name ?? data.userName ?? data.profile?.email ?? data.userId ??
    "Qoder account";
}

/** `PLAN_TIER_TEAM` → "Team"; unknown plans stay readable instead of showing the enum. */
export function planTierLabel(plan: string | null | undefined): string | null {
  const value = plan?.trim();
  if (!value) return null;
  return value
    .replace(/^PLAN_TIER_/i, "")
    .replace(/[_-]+/g, " ")
    .toLowerCase()
    .replace(/\b[a-z]/g, (character) => character.toUpperCase());
}

/** 桌面档位徽章文案:userTag(如 Teams)优先,否则用 plan 枚举,统一大写。 */
export function accountTier(
  userTag: string | null | undefined,
  plan: string | null | undefined,
): string | null {
  const tag = userTag?.trim();
  if (tag) return tag.toUpperCase();
  const label = planTierLabel(plan);
  return label ? label.toUpperCase() : null;
}

export function presentAccount(resource: ResourceSnapshot): ResourceView {
  const data = accountData(resource);
  const metrics: ResourceMetric[] = [];
  const quota = data.quota;
  if (quota) {
    if (quota.pooled) {
      // Teams/Enterprise 池化账号:已用、剩余两项分开,均为纯数值。
      if (quota.usedCount !== null) {
        metrics.push({
          id: "quota-used",
          label: { "en-US": "Quota used", "zh-CN": "已用额度" },
          unit: "count",
          value: quota.usedCount,
        });
      }
      if (quota.remainingCount !== null) {
        metrics.push({
          id: "quota-remaining",
          label: { "en-US": "Quota remaining", "zh-CN": "剩余额度" },
          unit: "count",
          value: quota.remainingCount,
          ...(quota.resetAtMs !== null ? { resetAtMs: quota.resetAtMs } : {}),
        });
      }
    } else if (quota.remainingCount !== null) {
      // 个人账号:剩余/总量两个 count。
      metrics.push({
        id: "quota-count",
        label: { "en-US": "Quota remaining", "zh-CN": "剩余额度" },
        unit: "count",
        value: quota.remainingCount,
        ...(quota.totalCount !== null ? { total: quota.totalCount } : {}),
        ...(quota.resetAtMs !== null ? { resetAtMs: quota.resetAtMs } : {}),
      });
    } else if (quota.remainingPercent !== null) {
      metrics.push({
        id: "quota-percent",
        label: { "en-US": "Quota remaining", "zh-CN": "剩余额度" },
        unit: "percent",
        value: quota.remainingPercent,
        ...(quota.resetAtMs !== null ? { resetAtMs: quota.resetAtMs } : {}),
      });
    }
  }
  const tier = accountTier(quota?.userTag, quota?.planLabel);
  return {
    displayName: accountDisplayName(data),
    ...(tier ? { tier } : {}),
    ...(metrics.length > 0 ? { metrics } : {}),
  };
}

function invalidPatch(data?: AccountData): ResourcePatch {
  return {
    ...(data ? { privateData: privateData(data) } : {}),
    state: { status: "invalid", message: EXPIRED_MESSAGE },
  };
}

/** Refresh credentials, then best-effort cache user info and quota for presentation. */
export async function refreshAccount(
  resource: ResourceSnapshot,
  context: PluginContext,
): Promise<ResourcePatch> {
  const current = latestAccountData(resource.id, accountData(resource));
  const refreshed = await sharedRefreshAccessToken(resource.id, current, context);
  if (!refreshed) return invalidPatch();

  let data = refreshed;
  try {
    const userInfo = await context.network.fetch(USERINFO_URL, {
      method: "GET",
      headers: { Accept: "application/json", Authorization: `Bearer ${data.accessToken}` },
    });
    if (userInfo.status === 401 || userInfo.status === 403) return invalidPatch(data);
    if (userInfo.status >= 200 && userInfo.status < 300) {
      const profile = parseUserInfo(JSON.parse(userInfo.body));
      data = {
        ...data,
        userId: profile.id ?? data.userId,
        userName: profile.name ?? data.userName,
        profile,
      };
    }
  } catch {
    // Profile data is optional; never discard an already rotated credential.
  }

  let quota: AccountQuota | null = null;
  try {
    const status = await context.network.fetch(STATUS_URL, {
      method: "GET",
      headers: { Accept: "application/json", Authorization: `Bearer ${data.accessToken}` },
    });
    if (status.status === 401 || status.status === 403) return invalidPatch(data);
    if (status.status >= 200 && status.status < 300) {
      const body = JSON.parse(status.body);
      quota = parseQoderStatus(body);
      data = { ...data, userType: parseUserType(body) ?? data.userType, quota };
    }
  } catch {
    // Quota is optional presentation data; token and identity refresh still succeed.
  }

  try {
    const usage = await context.network.fetch(QUOTA_USAGE_URL, {
      method: "GET",
      headers: { Accept: "application/json", Authorization: `Bearer ${data.accessToken}` },
    });
    if (usage.status >= 200 && usage.status < 300) {
      const credits = parseQuotaUsage(JSON.parse(usage.body));
      quota = {
        ...credits,
        planLabel: quota?.planLabel ?? null,
        userTag: quota?.userTag ?? null,
        usedCount: credits.usedCount ?? quota?.usedCount ?? null,
        totalCount: credits.totalCount ?? quota?.totalCount ?? null,
        pooled: credits.pooled || (quota?.pooled ?? false),
        resetAtMs: credits.resetAtMs ?? quota?.resetAtMs ?? null,
      };
      data = { ...data, quota };
    }
  } catch {
    // Credit usage refines the status quota; the status value already stands.
  }
  data = rememberAccountData(resource.id, data);
  return { privateData: privateData(data), state: quotaState(data.quota) };
}

/** Identity fields the COSY signer seals into every signed request. */
export function cosyIdentity(data: AccountData): CosyIdentity {
  return {
    uid: data.userId ?? "",
    name: data.userName ?? "",
    organizationId: data.profile?.organizationId ?? "",
    organizationName: data.profile?.organizationName ?? "",
    userType: data.userType ?? data.profile?.userType ?? "",
    accessToken: data.accessToken,
    refreshToken: data.refreshToken ?? "",
  };
}

/** Qoder exposes no token-revocation endpoint for this public device client. */
export function removeAccount(
  resource: ResourceSnapshot,
  _context: PluginContext,
): Promise<void> {
  latestAccounts.delete(resource.id);
  return Promise.resolve();
}
