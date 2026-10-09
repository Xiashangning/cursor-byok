import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import { object, parseJsonObject, text } from "cursor-byok:json";
import type { OAuth2AddMethod, OAuth2Begin, OAuth2Poll } from "cursor-byok:resource";
import { credentialDraft, readTokenBundle } from "./resources.ts";

const CLIENT_ID = "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb";
const LOGIN_URL = "https://qoder.com/device/selectAccounts";
const POLL_URL = "https://openapi.qoder.sh/api/v1/deviceToken/poll";
const FLOW_LIFETIME_MS = 5 * 60 * 1000;

export type QoderOAuthSession = {
  nonce: string;
  verifier: string;
  machineId: string;
};

function base64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

async function pkce(): Promise<{ verifier: string; challenge: string }> {
  // 64 random bytes produce an 86-character RFC 7636 verifier.
  const verifier = base64Url(crypto.getRandomValues(new Uint8Array(64)));
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
  return { verifier, challenge: base64Url(new Uint8Array(digest)) };
}

function parseSession(value: JsonValue): QoderOAuthSession {
  const session = object(value);
  const nonce = text(session?.nonce);
  const verifier = text(session?.verifier);
  const machineId = text(session?.machineId);
  if (!nonce || !verifier || !machineId) throw new Error("Qoder OAuth session is invalid");
  return { nonce, verifier, machineId };
}

async function begin(_context: PluginContext): Promise<OAuth2Begin> {
  const { verifier, challenge } = await pkce();
  const nonce = crypto.randomUUID();
  const machineId = crypto.randomUUID();
  const url = new URL(LOGIN_URL);
  url.searchParams.set("challenge", challenge);
  url.searchParams.set("challenge_method", "S256");
  url.searchParams.set("nonce", nonce);
  url.searchParams.set("machine_id", machineId);
  url.searchParams.set("client_id", CLIENT_ID);
  const session: QoderOAuthSession = { nonce, verifier, machineId };
  return {
    session: session as unknown as JsonValue,
    userCode: "",
    verificationUrl: url.toString(),
    verificationUrlComplete: url.toString(),
    expiresAtMs: Date.now() + FLOW_LIFETIME_MS,
    pollIntervalMs: 1_000,
  };
}

async function poll(sessionValue: JsonValue, context: PluginContext): Promise<OAuth2Poll> {
  const session = parseSession(sessionValue);
  const url = new URL(POLL_URL);
  url.searchParams.set("nonce", session.nonce);
  url.searchParams.set("verifier", session.verifier);
  url.searchParams.set("challenge_method", "S256");
  const response = await context.network.fetch(url.toString(), {
    method: "GET",
    headers: { Accept: "application/json" },
  });
  if (response.status === 404 || response.status === 202) return { status: "pending" };
  if (response.status < 200 || response.status >= 300) {
    const body = parseJsonObject(response.body);
    const message = text(body.message ?? body.error_description ?? body.error);
    return {
      status: "failed",
      message: message ?? `Qoder device authorization failed (HTTP ${response.status})`,
    };
  }
  const body = parseJsonObject(response.body);
  const bundle = readTokenBundle(body);
  if (!bundle) {
    return { status: "failed", message: "Qoder device authorization response is missing a token" };
  }
  return {
    status: "completed",
    resources: [await credentialDraft({ ...bundle, machineId: session.machineId })],
  };
}

export const qoderDeviceOAuth: OAuth2AddMethod = {
  type: "oauth2.0",
  id: "qoder-device",
  displayName: {
    "en-US": "Sign in with Qoder",
    "zh-CN": "使用 Qoder 登录",
  },
  description: {
    "en-US": "Authorize this device in your browser, then add the Qoder account.",
    "zh-CN": "在浏览器中授权此设备，然后添加对应的 Qoder 账号。",
  },
  begin,
  poll,
};
