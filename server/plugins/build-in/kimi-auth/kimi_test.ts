import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest, ModelEvent } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { kimiDeviceOAuth } from "./oauth.ts";
import { FALLBACK_MODELS, kimiModels, parseKimiModels } from "./models.ts";
import { kimiProvider } from "./provider.ts";
import {
  accountIdentity,
  credentialDraft,
  parseCredentialFiles,
  parseKimiUsage,
  presentAccount,
  refreshAccount,
  RESOURCE_TYPE,
} from "./resources.ts";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

function jwt(payload: Record<string, unknown>): string {
  const encoded = btoa(JSON.stringify(payload)).replace(/=/g, "").replace(/\+/g, "-").replace(
    /\//g,
    "_",
  );
  return `header.${encoded}.signature`;
}

type RequestInit = { body?: string; headers?: Record<string, string> };
type FetchHandler = (url: string, init?: RequestInit) => NetworkResponse;
type StreamHandler = (url: string, init?: RequestInit) => NetworkEventStream;

function context(handlers: { fetch?: FetchHandler; stream?: StreamHandler }): PluginContext {
  return {
    network: {
      fetch: (url, init) => {
        if (!handlers.fetch) throw new Error("fetch was not expected");
        return Promise.resolve(handlers.fetch(url, init));
      },
      stream: (url, init) => {
        if (!handlers.stream) throw new Error("stream was not expected");
        return Promise.resolve(handlers.stream(url, init));
      },
    },
    signal: new AbortController().signal,
  };
}

function snapshot(privateData: JsonValue): ResourceSnapshot {
  return {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "kimi:user-1",
    privateData,
    state: { status: "ready" },
  };
}

async function* sse(lines: string[]): AsyncGenerator<string> {
  for (const line of lines) yield line;
}

function request(): LlmRequest {
  return {
    instructions: "You are a coding assistant.",
    messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
    tools: [],
    reasoning: { enabled: true, effort: "medium" },
    latency: "fast",
    maxOutputTokens: 32_000,
    cacheKey: "conversation-1",
  };
}

Deno.test("account identity uses the JWT subject and drafts keep tokens private-side", async () => {
  const token = jwt({ sub: "user-1", email: "person@kimi.com" });
  assertEquals(await accountIdentity(token), {
    key: "kimi:user-1",
    displayName: "person@kimi.com",
  });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  assertEquals(draft.key, "kimi:user-1");
  const view = presentAccount(snapshot(draft.privateData));
  assert(!JSON.stringify(view).includes(token), "resource view exposed an access token");
  assertEquals(view.displayName, "person@kimi.com");
});

Deno.test("credential import accepts Kimi credential JSON files", () => {
  const { credentials, warnings } = parseCredentialFiles([
    {
      name: "accounts.json",
      content: JSON.stringify({
        accounts: [
          { access_token: "token-1", refresh_token: "refresh-1", email: "a@kimi.com" },
          { access_token: "token-2", disabled: true },
        ],
      }),
    },
    { name: "broken.json", content: "{not json" },
  ]);
  assertEquals(credentials, [{
    accessToken: "token-1",
    refreshToken: "refresh-1",
    displayName: "a@kimi.com",
  }]);
  assertEquals(warnings, ["broken.json: not valid JSON"]);
});

Deno.test("model discovery parses the Kimi Code model list shape", () => {
  const models = parseKimiModels({
    data: [
      { id: "kimi-for-coding", display_name: "K2.7 Coding", context_length: 262_144 },
      { id: "k2-thinking", supports_reasoning: true, supports_image_in: true },
      { id: "kimi-for-coding" },
    ],
  });
  assertEquals(models.map((model) => model.id), ["kimi-for-coding", "k2-thinking"]);
  assertEquals(models[0].displayName, "K2.7 Coding");
  assertEquals(models[0].capabilities, { images: false });
  assertEquals(models[0].privateData, { thinking: false, contextWindowTokens: 262_144 });
  assertEquals(models[1].capabilities, { images: true });
  assertEquals(models[1].privateData, { thinking: true });
});

Deno.test("model discovery falls back to known models when the account cannot list", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const models = await kimiModels.list(
    { resource: snapshot(draft.privateData) },
    context({
      fetch: () => ({ status: 401, headers: {}, body: "{}" }),
    }),
  );
  assertEquals(models, FALLBACK_MODELS);
});

Deno.test("refresh marks expired credentials invalid and healthy ones ready", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const expired = await refreshAccount(
    snapshot(draft.privateData),
    context({ fetch: () => ({ status: 401, headers: {}, body: "{}" }) }),
  );
  assertEquals(expired.state, {
    status: "invalid",
    message: "Kimi authorization expired; sign in again",
  });
  const healthy = await refreshAccount(
    snapshot(draft.privateData),
    context({ fetch: () => ({ status: 200, headers: {}, body: '{"data":[]}' }) }),
  );
  assertEquals(healthy.state, { status: "ready" });
});

Deno.test("usage parsing reads the weekly quota and the 300-minute window from string numbers", () => {
  const quota = parseKimiUsage({
    usage: { limit: "2048", used: "1024", remaining: "1024", resetTime: "2026-09-07T00:00:00Z" },
    limits: [
      {
        window: { duration: 300, timeUnit: "TIME_UNIT_MINUTE" },
        detail: { limit: "200", used: "150", remaining: "50", resetTime: 1_800_000_000 },
      },
      {
        window: { duration: 1440, timeUnit: "TIME_UNIT_MINUTE" },
        detail: { limit: "500", used: "0", remaining: "500" },
      },
    ],
  });
  assertEquals(quota.weekly, {
    usedPercent: 50,
    remainingPercent: 50,
    resetAtMs: Date.parse("2026-09-07T00:00:00Z"),
  });
  assertEquals(quota.fiveHour, {
    usedPercent: 75,
    remainingPercent: 25,
    resetAtMs: 1_800_000_000_000,
  });
});

Deno.test("usage parsing tolerates missing limits and malformed numbers", () => {
  const weeklyOnly = parseKimiUsage({ usage: { limit: "2048", remaining: "2048" } });
  assertEquals(weeklyOnly.weekly?.remainingPercent, 100);
  assertEquals(weeklyOnly.weekly?.resetAtMs, null);
  assertEquals(weeklyOnly.fiveHour, null);
  const malformed = parseKimiUsage({
    usage: { limit: "lots", remaining: "some" },
    limits: [{ window: { duration: 300 }, detail: { limit: "0", remaining: "0" } }],
  });
  assertEquals(malformed.weekly?.remainingPercent, null);
  assertEquals(malformed.fiveHour?.remainingPercent, null);
});

Deno.test("presentAccount surfaces weekly and five-hour quota metrics", async () => {
  const token = jwt({ sub: "user-1", email: "person@kimi.com" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const resetAtMs = Date.now() + 60_000;
  const data = {
    ...(draft.privateData as Record<string, unknown>),
    quota: {
      weekly: { usedPercent: 50, remainingPercent: 50, resetAtMs },
      fiveHour: { usedPercent: 75, remainingPercent: 25, resetAtMs },
      updatedAtMs: Date.now(),
    },
  };
  const view = presentAccount(snapshot(data as JsonValue));
  assertEquals(view.metrics, [
    {
      id: "weekly",
      label: { "en-US": "Weekly quota", "zh-CN": "周额度" },
      unit: "percent",
      value: 50,
      resetAtMs,
    },
    {
      id: "five-hour",
      label: { "en-US": "5-hour window", "zh-CN": "5 小时窗口" },
      unit: "percent",
      value: 25,
      resetAtMs,
    },
  ]);
  const bare = presentAccount(snapshot(draft.privateData));
  assertEquals(bare.metrics, undefined);
});

Deno.test("refresh stores the parsed quota and cools the account when it is exhausted", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const weeklyReset = Date.now() + 86_400_000;
  const patch = await refreshAccount(
    snapshot(draft.privateData),
    context({
      fetch: (url) => {
        if (url.endsWith("/models")) {
          return { status: 200, headers: {}, body: '{"data":[]}' };
        }
        assertEquals(url, "https://api.kimi.com/coding/v1/usages");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            usage: { limit: "2048", used: "2048", remaining: "0", resetTime: weeklyReset },
            limits: [{
              window: { duration: 300, timeUnit: "TIME_UNIT_MINUTE" },
              detail: { limit: "200", used: "150", remaining: "50" },
            }],
          }),
        };
      },
    }),
  );
  assertEquals(patch.state, {
    status: "cooling",
    retryAtMs: weeklyReset,
    message: "Kimi quota is exhausted",
  });
  const saved = (patch.privateData as Record<string, unknown>).quota as Record<string, unknown>;
  assertEquals((saved.weekly as Record<string, unknown>).remainingPercent, 0);
  assertEquals((saved.fiveHour as Record<string, unknown>).remainingPercent, 25);
});

Deno.test("refresh stays ready when the usage lookup fails", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const patch = await refreshAccount(
    snapshot(draft.privateData),
    context({
      fetch: (url) =>
        url.endsWith("/models")
          ? { status: 200, headers: {}, body: '{"data":[]}' }
          : { status: 500, headers: {}, body: "boom" },
    }),
  );
  assertEquals(patch.state, { status: "ready" });
  assertEquals(patch.privateData, undefined);
});

Deno.test("invoke maps a 429 response to a cooling resource error", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const result = await kimiProvider.invoke(
    {
      model: { id: "kimi-for-coding", displayName: "Kimi for Coding" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 429,
        headers: {},
        lines: sse(['{"error":"rate limit exceeded"}']),
      }),
    }),
  );
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assert(result.patch.state?.status === "cooling", "429 should cool the resource");
  assert(
    result.patch.state.retryAtMs !== undefined && result.patch.state.retryAtMs > Date.now(),
    "cooling should carry a future retry time",
  );
});

Deno.test("device OAuth begins with a host-held session and completes with a resource draft", async () => {
  const accessToken = jwt({ sub: "user-oauth", email: "oauth@kimi.com" });
  let requestNumber = 0;
  const flowContext = context({
    fetch: (url, init) => {
      requestNumber += 1;
      if (requestNumber === 1) {
        assertEquals(url, "https://auth.kimi.com/api/oauth/device_authorization");
        assert(init?.body?.includes("client_id="), "device code request must carry the client id");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            device_code: "private-device-code",
            user_code: "ABCD-EFGH",
            verification_uri: "https://www.kimi.com/device",
            verification_uri_complete: "https://www.kimi.com/device?code=ABCD-EFGH",
            expires_in: 900,
            interval: 5,
          }),
        };
      }
      assertEquals(url, "https://auth.kimi.com/api/oauth/token");
      assert(init?.body?.includes("device_code=private-device-code"));
      if (requestNumber === 2) {
        return {
          status: 400,
          headers: {},
          body: JSON.stringify({ error: "authorization_pending" }),
        };
      }
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({ access_token: accessToken, refresh_token: "refresh-secret" }),
      };
    },
  });

  const begun = await kimiDeviceOAuth.begin(flowContext);
  assertEquals(begun.userCode, "ABCD-EFGH");
  assertEquals(begun.verificationUrlComplete, "https://www.kimi.com/device?code=ABCD-EFGH");
  assertEquals(begun.pollIntervalMs, 5000);

  const pending = await kimiDeviceOAuth.poll(begun.session, flowContext);
  assertEquals(pending.status, "pending");

  const polled = await kimiDeviceOAuth.poll(begun.session, flowContext);
  assert(polled.status === "completed", `expected completed, received ${polled.status}`);
  assertEquals(polled.resources[0].key, "kimi:user-oauth");
  assertEquals(requestNumber, 3);
});

Deno.test("invoke streams normalized events from the Kimi Code Chat Completions API", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];
  const result = await kimiProvider.invoke(
    {
      model: { id: "kimi-for-coding", displayName: "Kimi for Coding" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: (url, init) => {
        assertEquals(url, "https://api.kimi.com/coding/v1/chat/completions");
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"choices":[{"delta":{"content":"Hel"}}]}',
            'data: {"choices":[{"delta":{"content":"lo"}}]}',
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}',
            "data: [DONE]",
          ]),
        };
      },
    }),
  );
  assertEquals(result, { status: "completed" });
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.model, "kimi-for-coding");
  assertEquals(body.stream, true);
  assert(!("reasoning_effort" in body), "Kimi Code endpoint receives no reasoning_effort");
  assert(!("service_tier" in body), "Kimi Code endpoint receives no service_tier");
  assertEquals(requestHeaders["authorization"], `Bearer ${token}`);
  assertEquals(events, [
    { type: "text-start" },
    { type: "text-delta", text: "Hel" },
    { type: "text-delta", text: "lo" },
    { type: "text-end" },
    {
      type: "usage",
      usage: {
        inputTokens: 10,
        outputTokens: 2,
        totalTokens: null,
        cacheReadTokens: null,
        cacheWriteTokens: null,
        reasoningTokens: null,
      },
    },
    { type: "done", reason: "stop" },
  ]);
});

Deno.test("invoke maps authorization failures to an invalid resource error", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const result = await kimiProvider.invoke(
    {
      model: { id: "kimi-for-coding", displayName: "Kimi for Coding" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 401,
        headers: {},
        lines: sse(['{"error":"unauthorized"}']),
      }),
    }),
  );
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assert(result.patch.state?.status === "invalid", "auth failure should invalidate the resource");
});
