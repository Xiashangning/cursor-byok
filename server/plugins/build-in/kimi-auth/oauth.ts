import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import type { OAuth2AddMethod, OAuth2Begin, OAuth2Poll } from "cursor-byok:resource";
import { credentialDraft } from "./resources.ts";

// 与官方 Kimi CLI 相同的 Kimi Code 设备授权客户端。
const CLIENT_ID = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEVICE_CODE_URL = "https://auth.kimi.com/api/oauth/device_authorization";
const TOKEN_URL = "https://auth.kimi.com/api/oauth/token";

type Session = {
  deviceCode: string;
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

function parseBody(body: string): Record<string, unknown> {
  try {
    return object(JSON.parse(body)) ?? {};
  } catch {
    return {};
  }
}

function parseSession(value: JsonValue): Session {
  const session = object(value);
  const deviceCode = text(session?.deviceCode);
  if (!deviceCode) throw new Error("Kimi OAuth session is invalid");
  return { deviceCode };
}

async function begin(context: PluginContext): Promise<OAuth2Begin> {
  const response = await context.network.fetch(DEVICE_CODE_URL, {
    method: "POST",
    headers: {
      accept: "application/json",
      "content-type": "application/x-www-form-urlencoded",
    },
    body: new URLSearchParams({ client_id: CLIENT_ID }).toString(),
  });
  const body = parseBody(response.body);
  if (response.status < 200 || response.status >= 300) {
    throw new Error(
      `Failed to request Kimi device code (HTTP ${response.status}): ${response.body}`,
    );
  }
  const deviceCode = text(body.device_code);
  const userCode = text(body.user_code);
  const verificationUrl = text(body.verification_uri) ?? text(body.verification_uri_complete);
  if (!deviceCode || !userCode || !verificationUrl) {
    throw new Error("Kimi device authorization response is incomplete");
  }
  const session: Session = { deviceCode };
  return {
    session: session as unknown as JsonValue,
    userCode,
    verificationUrl,
    ...(text(body.verification_uri_complete)
      ? { verificationUrlComplete: text(body.verification_uri_complete)! }
      : {}),
    expiresAtMs: Date.now() + Math.max(1, number(body.expires_in) ?? 900) * 1000,
    pollIntervalMs: Math.max(1, number(body.interval) ?? 5) * 1000,
  };
}

async function poll(sessionValue: JsonValue, context: PluginContext): Promise<OAuth2Poll> {
  const session = parseSession(sessionValue);
  const response = await context.network.fetch(TOKEN_URL, {
    method: "POST",
    headers: {
      accept: "application/json",
      "content-type": "application/x-www-form-urlencoded",
    },
    body: new URLSearchParams({
      grant_type: "urn:ietf:params:oauth:grant-type:device_code",
      client_id: CLIENT_ID,
      device_code: session.deviceCode,
    }).toString(),
  });
  const body = parseBody(response.body);
  if (response.status >= 200 && response.status < 300) {
    const accessToken = text(body.access_token);
    if (!accessToken) {
      return { status: "failed", message: "Kimi token response is missing access_token" };
    }
    return {
      status: "completed",
      resources: [
        await credentialDraft({
          accessToken,
          refreshToken: text(body.refresh_token),
          displayName: null,
        }),
      ],
    };
  }
  const code = text(body.error) ?? "";
  const message = text(body.error_description);
  switch (code) {
    case "authorization_pending":
      return { status: "pending" };
    case "slow_down":
      return { status: "slow-down" };
    case "expired_token":
      return { status: "failed", message: message ?? "Device authorization code expired" };
    case "access_denied":
      return { status: "denied", ...(message ? { message } : {}) };
    default:
      return {
        status: "failed",
        message: message ??
          (code
            ? `OAuth error: ${code}`
            : `Kimi device authorization failed (HTTP ${response.status})`),
      };
  }
}

export const kimiDeviceOAuth: OAuth2AddMethod = {
  type: "oauth2.0",
  id: "kimi-device",
  displayName: {
    "en-US": "Sign in with Kimi",
    "zh-CN": "使用 Kimi 登录",
  },
  description: {
    "en-US": "Authorize this device with your Kimi account, then add the resulting Kimi account.",
    "zh-CN": "在 Kimi 完成设备授权后,自动添加对应的 Kimi 账号。",
  },
  begin,
  poll,
};
