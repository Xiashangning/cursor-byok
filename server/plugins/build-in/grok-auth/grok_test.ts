import type { JsonValue } from "cursor-byok:plugin";
import type { ModelEvent } from "cursor-byok:provider";
import { assert, assertEquals, context, jwt, request, snapshot, sse } from "../test_helpers.ts";
import { grokDeviceOAuth } from "./oauth.ts";
import { FALLBACK_MODELS, grokModels, parseGrokModels } from "./models.ts";
import { grokProvider, isQuotaError } from "./provider.ts";
import {
  accountIdentity,
  credentialDraft,
  parseCredentialFiles,
  parseGrokUsage,
  planFromMonthlyLimit,
  presentAccount,
  quotaState,
  RESOURCE_TYPE,
} from "./resources.ts";

const grokSnapshot = (privateData: JsonValue) =>
  snapshot(RESOURCE_TYPE, "grok:user-1", privateData, "resource-1");

Deno.test("account identity uses the JWT subject and drafts keep tokens private-side", async () => {
  const token = jwt({ sub: "user-1", email: "person@x.ai" });
  assertEquals(await accountIdentity(token), {
    key: "grok:user-1",
    displayName: "person@x.ai",
  });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  assertEquals(draft.key, "grok:user-1");
  const view = presentAccount(grokSnapshot(draft.privateData));
  assert(
    !JSON.stringify(view).includes(token),
    "resource view exposed an access token",
  );
  assertEquals(view.displayName, "person@x.ai");
});

Deno.test("credential import accepts Grok credential JSON files", () => {
  const { credentials, warnings } = parseCredentialFiles([
    {
      name: "accounts.json",
      content: JSON.stringify({
        accounts: [
          {
            access_token: "token-1",
            refresh_token: "refresh-1",
            email: "a@x.ai",
          },
          { access_token: "token-2", disabled: true },
        ],
      }),
    },
    { name: "broken.json", content: "{not json" },
  ]);
  assertEquals(credentials, [{
    accessToken: "token-1",
    refreshToken: "refresh-1",
    accountId: null,
    idToken: null,
    displayName: "a@x.ai",
  }]);
  assertEquals(warnings, ["broken.json: not valid JSON"]);
});

Deno.test("credit usage percent is inverted to remaining and drives cooling", () => {
  const quota = parseGrokUsage({
    config: {
      creditUsagePercent: 34,
      currentPeriod: { type: "WEEKLY", end: "2026-09-01T00:00:00Z" },
    },
  }, 1_700_000_000_000);
  assertEquals(quota.period, "weekly");
  assertEquals(quota.remainingPercent, 66);
  assertEquals(quota.resetAtMs, Date.parse("2026-09-01T00:00:00Z"));
  assertEquals(quotaState(quota, 1_700_000_000_000), { status: "ready" });

  const exhausted = parseGrokUsage({
    config: { creditUsagePercent: 100, currentPeriod: { type: "WEEKLY", end: 1_900_000_000 } },
  }, 1_700_000_000_000);
  assertEquals(quotaState(exhausted, 1_700_000_000_000), {
    status: "cooling",
    retryAtMs: 1_900_000_000_000,
    message: "Grok credits are exhausted",
  });
});

Deno.test("missing usage with a billing period counts as unused", () => {
  const quota = parseGrokUsage({
    config: { currentPeriod: { type: "WEEKLY", end: 1_900_000_000 } },
  });
  assertEquals(quota.remainingPercent, 100);
  assertEquals(quota.limitReached, false);
});

Deno.test("currentPeriod type decides the metric period", () => {
  // 周计费账号:period 为 weekly,剩余 = 100 - creditUsagePercent。
  const weekly = parseGrokUsage({
    config: { creditUsagePercent: 20, currentPeriod: { type: "WEEKLY" } },
  }, 1_700_000_000_000);
  assertEquals(weekly.period, "weekly");
  assertEquals(weekly.remainingPercent, 80);

  // 月度预付账号:used/monthlyLimit 为美分,档位由 monthlyLimit 阈值推导。
  const monthly = parseGrokUsage({
    config: { currentPeriod: { type: "MONTHLY" }, monthlyLimit: 15000, used: 3750 },
  }, 1_700_000_000_000);
  assertEquals(monthly.period, "monthly");
  assertEquals(monthly.remainingPercent, 75);
  assertEquals(monthly.tier, "SUPERGROK");

  // 按量账号:on-demand 用量/上限换算,周期未知。
  const onDemand = parseGrokUsage({
    config: { onDemandUsed: { val: 12 }, onDemandCap: 40 },
  }, 1_700_000_000_000);
  assertEquals(onDemand.period, null);
  assertEquals(onDemand.remainingPercent, 70);
  assertEquals(onDemand.tier, null);
});

Deno.test("planFromMonthlyLimit maps the two official tiers", () => {
  assertEquals(planFromMonthlyLimit(15_000), "SUPERGROK");
  assertEquals(planFromMonthlyLimit(150_000), "SUPERGROK HEAVY");
  assertEquals(planFromMonthlyLimit(30_000), null);
  assertEquals(planFromMonthlyLimit(null), null);
});

Deno.test("model discovery parses both language-models and standard list shapes", () => {
  const richModels = parseGrokModels({
    models: [
      {
        id: "grok-4",
        input_modalities: ["text", "image"],
        context_window: 256_000,
      },
      { id: "grok-3-mini", input_modalities: ["text"] },
      { id: "grok-4" },
    ],
  });
  assertEquals(richModels.map((model) => model.id), ["grok-4", "grok-3-mini"]);
  assertEquals(richModels[0].displayName, "Grok 4");
  assertEquals(richModels[0].capabilities, { images: true });
  assertEquals(richModels[1].capabilities, { images: false });

  const plainModels = parseGrokModels({ data: [{ id: "grok-4-fast" }] });
  assertEquals(plainModels.map((model) => model.id), ["grok-4-fast"]);
  assertEquals(plainModels[0].displayName, "Grok 4 Fast");
});

Deno.test("model discovery falls back to known models when the account cannot list", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const models = await grokModels.list(
    { resource: grokSnapshot(draft.privateData) },
    context({
      fetch: () => ({
        status: 403,
        headers: {},
        body: JSON.stringify({ code: "personal-team-blocked:spending-limit" }),
      }),
    }),
  );
  assertEquals(models, FALLBACK_MODELS);
});

Deno.test("device OAuth begins with a host-held session and completes with a resource draft", async () => {
  const accessToken = jwt({ sub: "user-oauth", email: "oauth@x.ai" });
  let requestNumber = 0;
  const flowContext = context({
    fetch: (url, init) => {
      requestNumber += 1;
      if (requestNumber === 1) {
        assertEquals(url, "https://auth.x.ai/oauth2/device/code");
        assert(
          init?.body?.includes("scope="),
          "device code request must carry the scope",
        );
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            device_code: "private-device-code",
            user_code: "ABCD-EFGH",
            verification_uri: "https://accounts.x.ai/activate",
            verification_uri_complete: "https://accounts.x.ai/activate?code=ABCD-EFGH",
            expires_in: 900,
            interval: 5,
          }),
        };
      }
      assertEquals(url, "https://auth.x.ai/oauth2/token");
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
        body: JSON.stringify({
          access_token: accessToken,
          refresh_token: "refresh-secret",
        }),
      };
    },
  });

  const begun = await grokDeviceOAuth.begin(flowContext);
  assertEquals(begun.userCode, "ABCD-EFGH");
  assertEquals(begun.pollIntervalMs, 5000);

  const pending = await grokDeviceOAuth.poll(begun.session, flowContext);
  assertEquals(pending.status, "pending");

  const polled = await grokDeviceOAuth.poll(begun.session, flowContext);
  assert(
    polled.status === "completed",
    `expected completed, received ${polled.status}`,
  );
  assertEquals(polled.resources[0].key, "grok:user-oauth");
  assertEquals(requestNumber, 3);
});

Deno.test("invoke streams normalized events from the xAI Chat Completions API", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: grokSnapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: (url, init) => {
        assertEquals(url, "https://api.x.ai/v1/chat/completions");
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"choices":[{"delta":{"content":"Hel"}}]}',
            'data: {"choices":[{"delta":{"content":"lo"}}]}',
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}}',
            "data: [DONE]",
          ]),
        };
      },
    }),
  );
  assertEquals(result, { status: "completed" });
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.model, "grok-4");
  assertEquals(body.stream, true);
  assertEquals(body.prompt_cache_key, "conversation-1");
  assert(
    !("reasoning_effort" in body),
    "xAI endpoint rejects reasoning_effort",
  );
  assert(!("service_tier" in body), "xAI endpoint rejects service_tier");
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
        cacheReadTokens: 4,
        cacheWriteTokens: null,
        reasoningTokens: null,
      },
    },
    { type: "done", reason: "stop" },
  ]);
});

Deno.test("invoke streams incremental tool calls and reasoning replay state", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const events: ModelEvent[] = [];
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: grokSnapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: () => ({
        status: 200,
        headers: {},
        lines: sse([
          'data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}',
          'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\\"path\\":"}}]}}]}',
          'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\\"a.ts\\"}"}}]}}]}',
          'data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}',
          "data: [DONE]",
        ]),
      }),
    }),
  );
  assertEquals(result, { status: "completed" });
  assertEquals(events, [
    { type: "thinking-start" },
    { type: "thinking-delta", text: "thinking" },
    { type: "tool-call-start", index: 0, callId: "call-1", name: "read_file" },
    { type: "tool-call-arguments-delta", index: 0, delta: '{"path":' },
    { type: "tool-call-arguments-delta", index: 0, delta: '"a.ts"}' },
    { type: "thinking-end" },
    { type: "tool-call-end", index: 0 },
    {
      type: "replay-state",
      providerKind: "openai_chat",
      value: { reasoning_content: "thinking" },
    },
    { type: "done", reason: "tool-use" },
  ]);
});

Deno.test("invoke maps quota failures to a cooling resource error", async () => {
  assert(!isQuotaError("400 invalid request"));
  assert(isQuotaError("429 credits exhausted"));
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: grokSnapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 429,
        headers: {},
        lines: sse(['{"error":"credits exhausted"}']),
      }),
    }),
  );
  assert(
    result.status === "resource-error",
    `expected resource-error, received ${result.status}`,
  );
  assert(
    result.patch.state?.status === "cooling",
    "quota failure should cool the resource",
  );
});
