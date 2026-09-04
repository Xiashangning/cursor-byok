import type { PluginContext } from "cursor-byok:plugin";

// 与官方 Kimi CLI 相同的公共 OAuth 客户端;设备授权与 refresh_token 共用同一端点。
export const OAUTH_CLIENT_ID = "17e5f671-d194-4dfb-9706-5516cb48c098";
export const OAUTH_TOKEN_URL = "https://auth.kimi.com/api/oauth/token";

/** 刷新成功返回的令牌束;上游轮换 refresh_token 时携带新值。 */
export type TokenBundle = {
  accessToken: string;
  refreshToken: string | null;
};

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

/**
 * refresh_token 换新令牌。返回 null 表示刷新令牌被上游拒绝,需要重新登录;
 * 网络与服务端瞬时失败抛错,调用方不得据此判定账号失效。
 * 官方客户端对瞬时失败做退避重试;这里单次尝试,是否重试由调用方决定。
 */
export async function refreshBundle(
  refreshToken: string,
  context: PluginContext,
): Promise<TokenBundle | null> {
  const response = await context.network.fetch(OAUTH_TOKEN_URL, {
    method: "POST",
    headers: {
      accept: "application/json",
      "content-type": "application/x-www-form-urlencoded",
    },
    body: new URLSearchParams({
      client_id: OAUTH_CLIENT_ID,
      grant_type: "refresh_token",
      refresh_token: refreshToken,
    }).toString(),
  });
  if (response.status === 401 || response.status === 403) return null;
  if (response.status < 200 || response.status >= 300) {
    throw new Error(`Kimi token refresh failed (HTTP ${response.status}): ${response.body}`);
  }
  const body = object(JSON.parse(response.body));
  const accessToken = text(body?.access_token);
  if (!accessToken) throw new Error("Kimi token refresh response is missing access_token");
  return { accessToken, refreshToken: text(body?.refresh_token) };
}
