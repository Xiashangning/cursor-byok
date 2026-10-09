import type { JsonValue } from "cursor-byok:plugin";
import type { ModelEvent } from "cursor-byok:provider";
import {
  assert,
  assertEquals,
  context,
  jwt,
  request,
  response,
  snapshot,
  streamResponse,
} from "../test_helpers.ts";
import { kimiDeviceOAuth } from "./oauth.ts";
import { FALLBACK_MODELS, kimiModels, parseKimiModels } from "./models.ts";
import { kimiProvider } from "./provider.ts";
import {
  accountData,
  accountIdentity,
  credentialDraft,
  parseCredentialFiles,
  parseKimiUsage,
  presentAccount,
  refreshAccessToken,
  refreshAccount,
  RESOURCE_TYPE,
  tokenExpiring,
} from "./resources.ts";

const kimiSnapshot = (privateData: JsonValue, id = "resource-1") =>
  snapshot(RESOURCE_TYPE, "kimi:user-1", privateData, id);

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
  const view = presentAccount(kimiSnapshot(draft.privateData));
  assert(
    !JSON.stringify(view).includes(token),
    "resource view exposed an access token",
  );
  assertEquals(view.displayName, "person@kimi.com");
});

Deno.test("credential import accepts Kimi credential JSON files", () => {
  const { credentials, warnings } = parseCredentialFiles([
    {
      name: "accounts.json",
      content: JSON.stringify({
        accounts: [
          {
            access_token: "token-1",
            refresh_token: "refresh-1",
            email: "a@kimi.com",
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
    displayName: "a@kimi.com",
  }]);
  assertEquals(warnings, ["broken.json: not valid JSON"]);
});

Deno.test("model discovery parses the Kimi Code model list shape", () => {
  const models = parseKimiModels({
    data: [
      {
        id: "kimi-for-coding",
        display_name: "K2.7 Coding",
        context_length: 262_144,
      },
      { id: "k2-thinking", supports_reasoning: true, supports_image_in: true },
      { id: "kimi-for-coding" },
    ],
  });
  assertEquals(models.map((model) => model.id), [
    "kimi-for-coding",
    "k2-thinking",
  ]);
  assertEquals(models[0].displayName, "K2.7 Coding");
  assertEquals(models[0].capabilities, { images: false });
  assertEquals(models[0].privateData, {
    thinking: false,
    contextWindowTokens: 262_144,
  });
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
    { resource: kimiSnapshot(draft.privateData) },
    context({
      fetch: () => response("{}", 401),
    }),
  );
  assertEquals(models, FALLBACK_MODELS);
});

Deno.test("refresh rotates expired tokens, tolerates usage failures, and reports rejected or healthy credentials", async () => {
  // 过期 JWT:刷新授予换新令牌,/models 探针先旧后新;额度查询失败不影响凭证结论。
  const expired = jwt({
    sub: "user-1",
    exp: Math.floor(Date.now() / 1000) - 60,
  });
  const expiringDraft = await credentialDraft({
    accessToken: expired,
    refreshToken: "refresh-old",
    displayName: null,
  });
  const requests: Array<{ url: string; authorization: string }> = [];
  const rotated = await refreshAccount(
    kimiSnapshot(expiringDraft.privateData, "resource-recover"),
    context({
      fetch: (url, init) => {
        const authorization = init?.headers?.authorization ?? "";
        requests.push({ url, authorization });
        if (url.endsWith("/models")) {
          return authorization === "Bearer token-new"
            ? response('{"data":[]}')
            : response("{}", 401);
        }
        if (url.endsWith("/usages")) {
          return response("{}", 500);
        }
        assertEquals(url, "https://auth.kimi.com/api/oauth/token");
        return response({
          access_token: "token-new",
          refresh_token: "refresh-new",
        });
      },
    }),
  );
  assertEquals(
    requests.filter((request) => request.url.endsWith("/models")).map((
      request,
    ) => request.authorization),
    [`Bearer ${expired}`, "Bearer token-new"],
  );
  const data = rotated.privateData as Record<string, unknown>;
  assertEquals(data.accessToken, "token-new");
  assertEquals(data.refreshToken, "refresh-new");
  assertEquals(rotated.state, undefined);

  // 凭证被拒且没有 refresh token:直接判定失效。
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const rejected = await refreshAccount(
    kimiSnapshot(draft.privateData),
    context({ fetch: () => response("{}", 401) }),
  );
  assertEquals(rejected.state, {
    status: "invalid",
    message: "Kimi authorization expired; sign in again",
  });

  // 健康凭证:额度返回空列表时保持 ready。
  const healthy = await refreshAccount(
    kimiSnapshot(draft.privateData),
    context({
      fetch: () => response('{"data":[]}'),
    }),
  );
  assertEquals(healthy.state, { status: "ready" });

  // 健康凭证:额度查询失败同样保持 ready,且不产生补丁。
  const usageFailure = await refreshAccount(
    kimiSnapshot(draft.privateData),
    context({
      fetch: (url) => url.endsWith("/models") ? response('{"data":[]}') : response("boom", 500),
    }),
  );
  assertEquals(usageFailure.state, { status: "ready" });
  assertEquals(usageFailure.privateData, undefined);
});

Deno.test("usage parsing reads quota windows from string numbers and tolerates missing or malformed limits", () => {
  const quota = parseKimiUsage({
    usage: {
      limit: "2048",
      used: "1024",
      remaining: "1024",
      resetTime: "2026-09-07T00:00:00Z",
    },
    limits: [
      {
        window: { duration: 300, timeUnit: "TIME_UNIT_MINUTE" },
        detail: {
          limit: "200",
          used: "150",
          remaining: "50",
          resetTime: 1_800_000_000,
        },
      },
      {
        window: { duration: 1440, timeUnit: "TIME_UNIT_MINUTE" },
        detail: { limit: "500", used: "0", remaining: "500" },
      },
    ],
  });
  assertEquals(quota.weekly, {
    remainingPercent: 50,
    resetAtMs: Date.parse("2026-09-07T00:00:00Z"),
  });
  assertEquals(quota.fiveHour, {
    remainingPercent: 25,
    resetAtMs: 1_800_000_000_000,
  });

  const weeklyOnly = parseKimiUsage({
    usage: { limit: "2048", remaining: "2048" },
  });
  assertEquals(weeklyOnly.weekly?.remainingPercent, 100);
  assertEquals(weeklyOnly.weekly?.resetAtMs, null);
  assertEquals(weeklyOnly.fiveHour, null);
  const malformed = parseKimiUsage({
    usage: { limit: "lots", remaining: "some" },
    limits: [{
      window: { duration: 300 },
      detail: { limit: "0", remaining: "0" },
    }],
  });
  assertEquals(malformed.weekly?.remainingPercent, null);
  assertEquals(malformed.fiveHour?.remainingPercent, null);
});

Deno.test("refresh stores the parsed quota, cools the exhausted account, and presents quota metrics", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const weeklyReset = Date.now() + 86_400_000;
  const fiveHourReset = Date.now() + 3_600_000;
  const patch = await refreshAccount(
    kimiSnapshot(draft.privateData),
    context({
      fetch: (url) => {
        if (url.endsWith("/models")) {
          return response('{"data":[]}');
        }
        if (url.endsWith("/me")) {
          return response({ data: { user_level_name: "Allegro" } });
        }
        assertEquals(url, "https://api.kimi.com/coding/v1/usages");
        return response({
          usage: {
            limit: "2048",
            used: "2048",
            remaining: "0",
            resetTime: weeklyReset,
          },
          limits: [{
            window: { duration: 300, timeUnit: "TIME_UNIT_MINUTE" },
            detail: {
              limit: "200",
              used: "150",
              remaining: "50",
              resetTime: fiveHourReset,
            },
          }],
        });
      },
    }),
  );
  assertEquals(patch.state, {
    status: "cooling",
    retryAtMs: weeklyReset,
    message: "Kimi quota is exhausted",
  });
  const saved = (patch.privateData as Record<string, unknown>).quota as Record<
    string,
    unknown
  >;
  assertEquals((saved.weekly as Record<string, unknown>).remainingPercent, 0);
  assertEquals(
    (saved.fiveHour as Record<string, unknown>).remainingPercent,
    25,
  );

  const view = presentAccount(kimiSnapshot(patch.privateData as JsonValue));
  assertEquals(view.tier, "ALLEGRO");
  assertEquals(view.metrics, [
    {
      id: "five-hour",
      label: { "en-US": "5-hour window", "zh-CN": "5 小时窗口" },
      unit: "percent",
      value: 25,
      resetAtMs: fiveHourReset,
    },
    {
      id: "weekly",
      label: { "en-US": "Weekly quota", "zh-CN": "周额度" },
      unit: "percent",
      value: 0,
      resetAtMs: weeklyReset,
    },
  ]);
  const bare = presentAccount(kimiSnapshot(draft.privateData));
  assertEquals(bare.metrics, undefined);
});

Deno.test("new usages shape maps duration and code to 5h, weekly and monthly windows", () => {
  const quota = parseKimiUsage({
    usages: [
      {
        code: "limit_5h",
        window: { duration: 300, timeUnit: "TIME_UNIT_MINUTE" },
        detail: { limit: "100", used: "40", remaining: "60" },
      },
      {
        code: "limit_7d",
        window: { duration: 7, timeUnit: "TIME_UNIT_DAY" },
        detail: { limit: "2048", used: "1024", remaining: "1024" },
      },
      {
        code: "limit_month_total",
        detail: { limit: "10000", used: "2000", remaining: "8000" },
      },
    ],
  });
  assertEquals(quota.fiveHour?.remainingPercent, 60);
  assertEquals(quota.weekly?.remainingPercent, 50);
  assertEquals(quota.monthly?.remainingPercent, 80);

  // 上游省略 window 时按 code 归类,条目不丢。
  const noWindow = parseKimiUsage({
    usages: [
      { code: "limit_5h", detail: { limit: "100", used: "40", remaining: "60" } },
      { code: "limit_7d", detail: { limit: "2048", used: "1024", remaining: "1024" } },
    ],
  });
  assertEquals(noWindow.fiveHour?.remainingPercent, 60);
  assertEquals(noWindow.weekly?.remainingPercent, 50);
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
      resource: kimiSnapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => streamResponse(['{"error":"rate limit exceeded"}'], 429),
    }),
  );
  assert(
    result.status === "resource-error",
    `expected resource-error, received ${result.status}`,
  );
  assert(
    result.patch.state?.status === "cooling",
    "429 should cool the resource",
  );
  assert(
    result.patch.state.retryAtMs !== undefined &&
      result.patch.state.retryAtMs > Date.now(),
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
        assertEquals(
          url,
          "https://auth.kimi.com/api/oauth/device_authorization",
        );
        assert(
          init?.body?.includes("client_id="),
          "device code request must carry the client id",
        );
        return response({
          device_code: "private-device-code",
          user_code: "ABCD-EFGH",
          verification_uri: "https://www.kimi.com/device",
          verification_uri_complete: "https://www.kimi.com/device?code=ABCD-EFGH",
          expires_in: 900,
          interval: 5,
        });
      }
      assertEquals(url, "https://auth.kimi.com/api/oauth/token");
      assert(init?.body?.includes("device_code=private-device-code"));
      if (requestNumber === 2) {
        return response({ error: "authorization_pending" }, 400);
      }
      return response({
        access_token: accessToken,
        refresh_token: "refresh-secret",
      });
    },
  });

  const begun = await kimiDeviceOAuth.begin(flowContext);
  assertEquals(begun.userCode, "ABCD-EFGH");
  assertEquals(
    begun.verificationUrlComplete,
    "https://www.kimi.com/device?code=ABCD-EFGH",
  );
  assertEquals(begun.pollIntervalMs, 5000);

  const pending = await kimiDeviceOAuth.poll(begun.session, flowContext);
  assertEquals(pending.status, "pending");

  const polled = await kimiDeviceOAuth.poll(begun.session, flowContext);
  assert(
    polled.status === "completed",
    `expected completed, received ${polled.status}`,
  );
  assertEquals(polled.resources[0].key, "kimi:user-oauth");
  assertEquals(requestNumber, 3);
});

Deno.test("invoke streams normalized events from the Kimi Code Responses API", async () => {
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
      resource: kimiSnapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: (url, init) => {
        assertEquals(url, "https://api.kimi.com/coding/v1/responses");
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return streamResponse([
          'data: {"type":"response.output_text.delta","output_index":0,"delta":"Hel"}',
          'data: {"type":"response.output_text.delta","output_index":0,"delta":"lo"}',
          'data: {"type":"response.output_text.done","output_index":0,"text":"Hello"}',
          'data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":2}}}',
        ]);
      },
    }),
  );
  assertEquals(result, { status: "completed" });
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.model, "kimi-for-coding");
  assertEquals(body.stream, true);
  assert(
    !("reasoning" in body),
    "Kimi Code endpoint receives no reasoning field",
  );
  assert(
    !("service_tier" in body),
    "Kimi Code endpoint receives no service_tier",
  );
  assertEquals(body.input, [
    {
      type: "message",
      role: "user",
      content: [{ type: "input_text", text: "hi" }],
    },
  ]);
  assertEquals(body.include, ["reasoning.encrypted_content"]);
  assertEquals(body.max_output_tokens, 32_000);
  assertEquals(body.prompt_cache_key, "conversation-1");
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

Deno.test("invoke reconciles each output item's text against its own deltas", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const events: ModelEvent[] = [];
  // 中继只发终态(done/output_item.done,无 output_text.delta):第 2 个文本
  // item 的终态若与整条流的基线比对会因前缀不匹配被丢弃,文本整段丢失。
  const result = await kimiProvider.invoke(
    {
      model: { id: "kimi-for-coding", displayName: "Kimi for Coding" },
      resource: kimiSnapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: () =>
        streamResponse([
          'data: {"type":"response.output_text.done","output_index":0,"text":"checking the file"}',
          'data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call-1","name":"shell","arguments":""}}',
          'data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"echo hi"}',
          'data: {"type":"response.function_call_arguments.done","output_index":1,"arguments":"echo hi"}',
          'data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call-1","name":"shell","arguments":"echo hi"}}',
          'data: {"type":"response.output_item.done","output_index":2,"item":{"type":"message","content":[{"type":"output_text","text":"the file looks fine"}]}}',
          'data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":2}}}',
        ]),
    }),
  );
  assertEquals(result, { status: "completed" });
  // 第 1 个 item 的终态文本补发,工具调用完整,第 2 个 item 的终态文本也
  // 必须整段补发(回归点:旧实现把全流文本当基线,这里会静默丢失)。
  assertEquals(events, [
    { type: "text-start" },
    { type: "text-delta", text: "checking the file" },
    { type: "text-end" },
    { type: "tool-call-start", index: 1, callId: "call-1", name: "shell" },
    { type: "tool-call-arguments-delta", index: 1, delta: "echo hi" },
    { type: "tool-call-end", index: 1 },
    { type: "text-start" },
    { type: "text-delta", text: "the file looks fine" },
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
    { type: "done", reason: "tool-use" },
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
      resource: kimiSnapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => streamResponse(['{"error":"unauthorized"}'], 401),
    }),
  );
  assert(
    result.status === "resource-error",
    `expected resource-error, received ${result.status}`,
  );
  assert(
    result.patch.state?.status === "invalid",
    "auth failure should invalidate the resource",
  );
});

Deno.test("refreshAccessToken rotates or rejects refresh grants and tokenExpiring tracks the JWT exp claim", async () => {
  const draft = await credentialDraft({
    accessToken: jwt({ sub: "user-1" }),
    refreshToken: "refresh-old",
    displayName: null,
  });
  let requestedBody = "";
  const refreshed = await refreshAccessToken(
    accountData(kimiSnapshot(draft.privateData)),
    context({
      fetch: (url, init) => {
        assertEquals(url, "https://auth.kimi.com/api/oauth/token");
        requestedBody = init?.body ?? "";
        return response({
          access_token: "token-new",
          refresh_token: "refresh-new",
        });
      },
    }),
  );
  assert(
    requestedBody.includes("grant_type=refresh_token"),
    "must use the refresh_token grant",
  );
  assert(
    requestedBody.includes("client_id=17e5f671-d194-4dfb-9706-5516cb48c098"),
    "must send the official Kimi Code client id",
  );
  assert(
    requestedBody.includes("refresh_token=refresh-old"),
    "must send the stored refresh token",
  );
  assertEquals(refreshed?.accessToken, "token-new");
  assertEquals(refreshed?.refreshToken, "refresh-new");

  const rejected = await refreshAccessToken(
    accountData(kimiSnapshot(draft.privateData)),
    context({
      fetch: () => response("{}", 401),
    }),
  );
  assertEquals(rejected, null);

  const now = 1_800_000_000_000;
  const data = async (payload: Record<string, unknown>) =>
    accountData(kimiSnapshot(
      (await credentialDraft({
        accessToken: jwt(payload),
        refreshToken: null,
        displayName: null,
      }))
        .privateData,
    ));
  assert(
    !tokenExpiring(await data({ sub: "user-1" }), now),
    "missing exp never expires",
  );
  assert(
    tokenExpiring(await data({ sub: "user-1", exp: now / 1000 - 10 }), now),
  );
  assert(
    tokenExpiring(
      await data({ sub: "user-1", exp: (now + 30_000) / 1000 }),
      now,
    ),
    "within the 60s skew counts as expiring",
  );
  assert(
    !tokenExpiring(
      await data({ sub: "user-1", exp: (now + 120_000) / 1000 }),
      now,
    ),
  );
});

Deno.test("invoke refreshes expiring credentials, retries once after a 401, and invalidates a rejected refresh grant", async () => {
  const okLines = [
    'data: {"type":"response.output_text.delta","output_index":0,"delta":"ok"}',
    'data: {"type":"response.output_text.done","output_index":0,"text":"ok"}',
    'data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}',
  ];
  const okStream = () => streamResponse(okLines);

  // 临期令牌:调用前先刷新,新令牌随补丁持久化。
  const expired = jwt({
    sub: "user-1",
    exp: Math.floor(Date.now() / 1000) - 60,
  });
  const expiringDraft = await credentialDraft({
    accessToken: expired,
    refreshToken: "refresh-old",
    displayName: null,
  });
  let proactiveStreamCalls = 0;
  const proactive = await kimiProvider.invoke(
    {
      model: { id: "kimi-for-coding", displayName: "Kimi for Coding" },
      resource: kimiSnapshot(expiringDraft.privateData, "resource-expiring"),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: (url) => {
        assertEquals(url, "https://auth.kimi.com/api/oauth/token");
        return response({
          access_token: "token-new",
          refresh_token: "refresh-new",
        });
      },
      stream: (_url, init) => {
        proactiveStreamCalls++;
        assertEquals(init?.headers?.authorization, "Bearer token-new");
        return okStream();
      },
    }),
  );
  assertEquals(proactiveStreamCalls, 1);
  assert(
    proactive.status === "completed",
    `expected completed, received ${proactive.status}`,
  );
  const proactiveData = proactive.patch?.privateData as Record<string, unknown>;
  assertEquals(proactiveData.accessToken, "token-new");
  assertEquals(proactiveData.refreshToken, "refresh-new");

  // 401 后刷新一次再重试:首次用旧令牌,重试与补丁都用新令牌。
  const token = jwt({ sub: "user-1" });
  const retryDraft = await credentialDraft({
    accessToken: token,
    refreshToken: "refresh-old",
    displayName: null,
  });
  let retryStreamCalls = 0;
  const retried = await kimiProvider.invoke(
    {
      model: { id: "kimi-for-coding", displayName: "Kimi for Coding" },
      resource: kimiSnapshot(retryDraft.privateData, "resource-retry"),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: () =>
        response({
          access_token: "token-new",
          refresh_token: "refresh-new",
        }),
      stream: (_url, init) => {
        retryStreamCalls++;
        if (retryStreamCalls === 1) {
          assertEquals(init?.headers?.authorization, `Bearer ${token}`);
          return streamResponse(['{"error":"expired"}'], 401);
        }
        assertEquals(init?.headers?.authorization, "Bearer token-new");
        return okStream();
      },
    }),
  );
  assertEquals(retryStreamCalls, 2);
  assert(
    retried.status === "completed",
    `expected completed, received ${retried.status}`,
  );
  const retriedData = retried.patch?.privateData as Record<string, unknown>;
  assertEquals(retriedData.accessToken, "token-new");

  // 刷新授予被拒:账号判定为失效。
  const rejectedDraft = await credentialDraft({
    accessToken: jwt({ sub: "user-1" }),
    refreshToken: "refresh-old",
    displayName: null,
  });
  const rejected = await kimiProvider.invoke(
    {
      model: { id: "kimi-for-coding", displayName: "Kimi for Coding" },
      resource: kimiSnapshot(rejectedDraft.privateData, "resource-rejected"),
      request: request(),
    },
    { emit: () => {} },
    context({
      fetch: () => response("{}", 401),
      stream: () => streamResponse(['{"error":"expired"}'], 401),
    }),
  );
  assert(
    rejected.status === "resource-error",
    `expected resource-error, received ${rejected.status}`,
  );
  assertEquals(rejected.patch.state, {
    status: "invalid",
    message: "Kimi authorization expired; sign in again",
  });
});
