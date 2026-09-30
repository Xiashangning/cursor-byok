import type { JsonValue } from "cursor-byok:plugin";
import type { ModelEvent, ProviderInvokeInput } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import {
  assert,
  assertEquals,
  context,
  request,
  response,
  streamResponse,
} from "../test_helpers.ts";
import {
  isAuthenticationError,
  isQueuedError,
  isQuotaError,
  QoderChatError,
  streamQoderChat,
} from "./agent_stream.ts";
import { buildAgentBody, requestIdentity } from "./agent_request.ts";
import { fetchCatalog, parseCatalog } from "./catalog.ts";
import { qoderDeviceOAuth } from "./oauth.ts";
import { fetchModelTexts } from "./texts.ts";
import { qoderProvider } from "./provider.ts";
import {
  accountData,
  cosyIdentity,
  parseQuotaUsage,
  planTierLabel,
  presentAccount,
  quotaState,
  readTokenBundle,
  refreshAccount,
  RESOURCE_TYPE,
} from "./resources.ts";
import {
  aesCbcEncrypt,
  jsonSortedCompact,
  machineIdentity,
  md5Hex,
  qoderDecode,
  qoderEncode,
  rsaPkcs1v15Encrypt,
} from "./sign.ts";

let resourceSequence = 0;

function snapshot(privateData: JsonValue, id = `resource-${++resourceSequence}`): ResourceSnapshot {
  return {
    id,
    type: RESOURCE_TYPE,
    key: "qoder:user-1",
    privateData,
    state: { status: "ready" },
  };
}

function account(overrides: Record<string, JsonValue> = {}): JsonValue {
  return {
    security_oauth_token: "access-old",
    refresh_token: "refresh-old",
    user_id: "user-1",
    user_name: "Qoder User",
    machine_id: "11111111-1111-4111-8111-111111111111",
    access_expires_at_ms: null,
    refresh_token_expires_at_ms: null,
    profile: {
      id: "user-1",
      name: "Ada",
      email: "ada@example.com",
      organizationId: "org-1",
      organizationName: "Example Org",
      organizationTags: ["team"],
      userType: null,
      dataPolicyAgreed: false,
    },
    quota: null,
    user_type: "teams",
    ...overrides,
  };
}

function envelope(body: unknown, statusCodeValue = 200): string {
  return `data:${
    JSON.stringify({
      headers: { "Content-Type": ["application/json"] },
      body: typeof body === "string" ? body : JSON.stringify(body),
      statusCodeValue,
      statusCode: statusCodeValue === 200 ? "OK" : "ERROR",
    })
  }`;
}

function chunk(value: unknown): string {
  return envelope({
    choices: [{ index: 0, delta: value as Record<string, JsonValue> }],
    created: 1,
    id: "chatcmpl-1",
    model: "auto",
    object: "chat.completion.chunk",
  });
}

function invokeInput(
  resource: ResourceSnapshot | null = snapshot(account()),
): ProviderInvokeInput {
  return {
    model: {
      id: "qmodel_38max",
      displayName: "Qwen3.8-Max",
      capabilities: { images: true },
      privateData: { key: "qmodel_38max", is_vl: true, is_reasoning: true },
    },
    resource,
    request: request(32_000),
  };
}

function streamContext(lines: string[], status = 200) {
  return context({ stream: () => streamResponse(lines, status) });
}

async function collect(
  input: ProviderInvokeInput,
  lines: string[],
  status = 200,
): Promise<ModelEvent[]> {
  const events: ModelEvent[] = [];
  await streamQoderChat(
    { model: input.model, request: input.request, data: accountData(input.resource!) },
    { emit: (event) => void events.push(event) },
    streamContext(lines, status),
  );
  return events;
}

// ---------------------------------------------------------------------- OAuth

Deno.test("OAuth begin builds a PKCE S256 login URL and poll resolves pending, token-less and aliased responses", async () => {
  const before = Date.now();
  const begun = await qoderDeviceOAuth.begin(context({}));
  const after = Date.now();
  const session = begun.session as Record<string, string>;
  const url = new URL(begun.verificationUrl);

  assert(session.verifier.length >= 43 && session.verifier.length <= 128);
  assert(/^[A-Za-z0-9._~-]+$/.test(session.verifier));
  assertEquals(url.origin + url.pathname, "https://qoder.com/device/selectAccounts");
  assertEquals(url.searchParams.get("challenge_method"), "S256");
  assertEquals(url.searchParams.get("nonce"), session.nonce);
  assertEquals(url.searchParams.get("machine_id"), session.machineId);
  assertEquals(url.searchParams.get("client_id"), "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb");
  assertEquals(begun.pollIntervalMs, 1_000);
  assert(begun.expiresAtMs >= before + 300_000 && begun.expiresAtMs <= after + 300_000);

  const pending = await qoderDeviceOAuth.poll(
    session,
    context({
      fetch: (pollUrl, init) => {
        const parsed = new URL(pollUrl);
        assertEquals(
          parsed.origin + parsed.pathname,
          "https://openapi.qoder.sh/api/v1/deviceToken/poll",
        );
        assertEquals(parsed.searchParams.get("verifier"), session.verifier);
        assertEquals(init?.headers?.Accept, "application/json");
        return response("", 404);
      },
    }),
  );
  assertEquals(pending, { status: "pending" });

  const failed = await qoderDeviceOAuth.poll(
    session,
    context({
      fetch: () => response({ user_id: "user-1" }),
    }),
  );
  assertEquals(failed, {
    status: "failed",
    message: "Qoder device authorization response is missing a token",
  });

  const now = Date.now();
  const refreshExpirySeconds = Math.floor(now / 1000) + 86_400;
  const result = await qoderDeviceOAuth.poll(
    session,
    context({
      fetch: () =>
        response({
          token: "security-token",
          refreshToken: "refresh-token",
          uid: "user-oauth",
          userName: "OAuth User",
          expiresIn: 3_600,
          refresh_token_expires_at: refreshExpirySeconds,
        }),
    }),
  );
  assert(result.status === "completed");
  const data = result.resources[0].privateData as Record<string, JsonValue>;
  assertEquals(result.resources[0].key, "qoder:user-oauth");
  assertEquals(data.security_oauth_token, "security-token");
  assertEquals(data.refresh_token, "refresh-token");
  assertEquals(data.user_id, "user-oauth");
  assertEquals(data.user_name, "OAuth User");
  assertEquals(data.machine_id, session.machineId);
  const accessExpiry = data.access_expires_at_ms as number;
  assert(accessExpiry >= now + 3_599_000 && accessExpiry <= Date.now() + 3_601_000);
  assertEquals(data.refresh_token_expires_at_ms, refreshExpirySeconds * 1000);
});

// ------------------------------------------------------------------ resources

Deno.test("resource refresh normalizes token bundles, rotates tokens, caches userinfo, reads credit quota and maps exhausted or rejected states", async () => {
  // Device-token and refresh spellings normalize onto one bundle.
  const bundle = readTokenBundle({
    device_token: "dt-1",
    refresh_token: "drt-1",
    user_id: "user-1",
    expires_at: "2030-01-01T00:00:00Z",
    refresh_token_expires_in: 86_400,
  });
  assert(bundle !== null);
  assertEquals(bundle.accessToken, "dt-1");
  assertEquals(bundle.refreshToken, "drt-1");
  assertEquals(bundle.userId, "user-1");
  assertEquals(bundle.accessExpiresAtMs, Date.parse("2030-01-01T00:00:00Z"));

  assertEquals(readTokenBundle({ user_id: "user-1" }), null);

  let call = 0;
  const patch = await refreshAccount(
    snapshot(account()),
    context({
      fetch: (url, init) => {
        call += 1;
        assertEquals(init?.headers?.Authorization, call === 1 ? undefined : "Bearer access-new");
        if (call === 1) {
          assertEquals(url, "https://openapi.qoder.sh/api/v1/deviceToken/refresh");
          return response({
            device_token: "access-new",
            refresh_token: "refresh-new",
            expires_in: 600,
            refresh_token_expires_in: 3_600,
          });
        }
        if (call === 2) {
          assertEquals(url, "https://openapi.qoder.sh/api/v1/userinfo");
          return response({
            id: "user-1",
            name: "Ada",
            email: "ada@example.com",
            organization_id: "org-1",
            organization_name: "Example Org",
          });
        }
        if (call === 3) {
          assertEquals(url, "https://openapi.qoder.sh/api/v3/user/status");
          return response({
            userType: "teams",
            plan: "PLAN_TIER_TEAM",
            userTag: "Teams",
            isQuotaExceeded: false,
            quota: 0,
            nextResetAt: 1_792_753_794_979,
          });
        }
        assertEquals(url, "https://openapi.qoder.sh/api/v2/quota/usage");
        return response({
          usageType: "credits",
          totalUsagePercentage: 0.25,
          isQuotaExceeded: false,
          expiresAt: 1_792_753_794_979,
          userQuota: {
            total: 3_000,
            used: 7.5,
            remaining: 2_992.5,
            percentage: 0.25,
            unit: "credits",
          },
          orgResourcePackage: {
            used: 170,
            remaining: 236_987,
            unit: "credits",
            cap: -1,
            available: true,
          },
        });
      },
    }),
  );

  const data = accountData({ ...snapshot(account()), ...patch, privateData: patch.privateData! });
  assertEquals(data.accessToken, "access-new");
  assertEquals(data.refreshToken, "refresh-new");
  assertEquals(data.userType, "teams");
  assertEquals(data.quota?.planLabel, "PLAN_TIER_TEAM");
  assertEquals(data.quota?.remainingPercent, 99.75);
  assertEquals(data.quota?.remainingCount, 2_993);
  assertEquals(data.quota?.exceeded, false);
  assertEquals(patch.state, { status: "ready" });
  assertEquals(call, 4);

  // Exhausted credit quota parks the account until the reset.
  const quota = parseQuotaUsage({
    totalUsagePercentage: 100,
    isQuotaExceeded: true,
    expiresAt: Date.now() + 3_600_000,
    userQuota: { remaining: 0 },
  });
  assertEquals(quota.remainingPercent, 0);
  assertEquals(quota.remainingCount, 0);
  assertEquals(quota.exceeded, true);
  assert(quota.resetAtMs !== null && quota.resetAtMs > Date.now());
  assertEquals(quotaState(quota).status, "cooling");

  const rejected = await refreshAccount(
    snapshot(account()),
    context({
      fetch: () =>
        response(
          {
            errorCode: "DeviceRefreshTokenPrefixInvalid",
            errorMessage: "invalid refresh_token: must start with drt-",
          },
          400,
        ),
    }),
  );
  assertEquals(rejected, {
    state: { status: "invalid", message: "Qoder authorization expired; sign in again" },
  });
});

function quotaView(overrides: Record<string, JsonValue> = {}): JsonValue {
  return {
    planLabel: "PLAN_TIER_TEAM",
    userTag: null,
    remainingPercent: null,
    remainingCount: null,
    exceeded: false,
    resetAtMs: null,
    updatedAtMs: 1,
    ...overrides,
  };
}

Deno.test("presentAccount projects cached profile and credit quota", () => {
  const view = presentAccount(snapshot(account({
    quota: quotaView({ userTag: "Teams", remainingPercent: 99.75, remainingCount: 2_993 }),
  })));
  assertEquals(view.displayName, "Ada");
  assertEquals(view.description, "Teams");
  assertEquals(view.metrics?.[0].id, "quota-percent");
  assertEquals(view.metrics?.[0].value, 99.75);

  // Without a user tag the plan enum is humanized for the account card.
  assertEquals(planTierLabel("PLAN_TIER_TEAM"), "Team");
  assertEquals(planTierLabel("PLAN_TIER_PERSONAL_PRO"), "Personal Pro");
  assertEquals(planTierLabel(null), null);
  const fallback = presentAccount(snapshot(account({
    quota: quotaView({ planLabel: "PLAN_TIER_ENTERPRISE", remainingPercent: 80 }),
  })));
  assertEquals(fallback.description, "Enterprise");
});

// -------------------------------------------------------------------- signing

const vectors = {
  encode_empty_object: "$kwm",
  encode_json_hello: "&QPSWBEw$oK*BMn*QG(mYKiBMn%G",
  aes_key: "0123456789abcdef",
  aes_plain: "Hello, Qoder! 1234567890abcdef",
  aes_cipher_hex: "a909ce1a6f1e8384d5605e37c0a5b13c174787da2db09d07ddefd4b9e91c9520",
  rsa_plain: "0123456789abcdef",
  rsa_ps_hex:
    "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d",
  rsa_cipher_hex:
    "0a3df0703dc7572dd84ad3fd6ac2be99a534c5a789fb48be00916fcba8be437f55827b897b41a7a8953431aca2f97a4558263333b45308121f075f1cf6b2830fe3e42659f9547a1ffe4eca279ca6040b0237e01f0c5014903147ecc7333c37ea11fb67839ddbeca6d231870d8b59a485334d110abc7b962d3b01cddaf3ef8901",
  machine_id: "a453681e676d43b7f40915ed1fae05f3",
  machine_type: "c0276ea2a9b725cf6b",
  machine_token: "qMpLpigA0-itbtmk6lNdxSPxWBr2omQod6gWbAY5A-5",
  identity_plain:
    '{"aid":"019dbeaf-75e1-759b-af1a-989c0ec9d54f","name":"shangning xia","organization_id":"019dba08-3b9e-78a6-ba4c-b31dceb4a579","organization_name":"穹彻智能","refresh_token":"drt-probe","security_oauth_token":"dt-probe","uid":"019dbeaf-75e1-759b-af1a-989c0ec9d54f","user_type":"teams","yx_uid":""}',
};

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

Deno.test("signing primitives match the reference vectors", async () => {
  assertEquals(qoderEncode("{}"), vectors.encode_empty_object);
  assertEquals(qoderEncode('{"a":1,"b":"你好"}'), vectors.encode_json_hello);
  assertEquals(
    new TextDecoder().decode(qoderDecode(vectors.encode_json_hello)),
    '{"a":1,"b":"你好"}',
  );

  assertEquals(md5Hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
  assertEquals(md5Hex(""), "d41d8cd98f00b204e9800998ecf8427e");
  const key = new TextEncoder().encode(vectors.aes_key);
  const sealed = await aesCbcEncrypt(new TextEncoder().encode(vectors.aes_plain), key);
  assertEquals(hex(sealed), vectors.aes_cipher_hex);
  const padding = new Uint8Array(
    vectors.rsa_ps_hex.match(/.{2}/g)!.map((pair) => parseInt(pair, 16)),
  );
  const cipher = rsaPkcs1v15Encrypt(new TextEncoder().encode(vectors.rsa_plain), padding);
  assertEquals(hex(cipher), vectors.rsa_cipher_hex);

  const identity = await machineIdentity("019dbeaf-75e1-759b-af1a-989c0ec9d54f");
  assertEquals(identity.machineId, vectors.machine_id);
  assertEquals(identity.machineType, vectors.machine_type);
  assertEquals(identity.machineToken, vectors.machine_token);

  assertEquals(
    jsonSortedCompact({
      name: "shangning xia",
      aid: "019dbeaf-75e1-759b-af1a-989c0ec9d54f",
      uid: "019dbeaf-75e1-759b-af1a-989c0ec9d54f",
      yx_uid: "",
      organization_id: "019dba08-3b9e-78a6-ba4c-b31dceb4a579",
      organization_name: "穹彻智能",
      user_type: "teams",
      security_oauth_token: "dt-probe",
      refresh_token: "drt-probe",
    }),
    vectors.identity_plain,
  );
  assertEquals(jsonSortedCompact({ b: "2", a: "1" }), '{"a":"1","b":"2"}');
});

// --------------------------------------------------------------------- models

function versionBody(version: string, ossUrl: string): string {
  return JSON.stringify({
    success: true,
    data: { version, ossUrl, publishTime: "2026-09-20T02:51:01.212Z" },
  });
}

const textsBody = JSON.stringify({
  zh: {
    "modelSelector.item.auto": "自动",
    "modelSelector.item.auto.description": "智能选择最适合的模型",
    "modelSelector.item.kmodel": "Kimi-K2.8-Preview",
    "modelSelector.item.kmodel.functionSwitch.highspeed.description": "速度提升 6x",
    "modelSelector.item.qmodel_38max": "Qwen3.8-Max",
    "modelSelector.item.qmodel_38max.description": "千问最新一代大基座模型。",
    "modelSelector.item.qmodel_38max.markdownDescription": "千问最新一代大基座模型（markdown）。",
  },
  en: {
    "modelSelector.item.auto": "Auto",
    "modelSelector.item.en-only": "Only English",
    "modelSelector.item.en-only.description": "English fallback blurb",
  },
});

const catalogBody = JSON.stringify({
  chat: [
    {
      key: "auto",
      display_name: "Auto",
      enable: true,
      is_vl: true,
      is_reasoning: false,
      is_default: true,
    },
    {
      key: "qmodel_38max",
      display_name: "Qwen3.8-Max",
      enable: true,
      is_vl: true,
      is_reasoning: true,
      price_factor: 0.5,
      context_config: {
        "1M": { token_count: 1_000_000 },
        "200K": { token_count: 200_000, is_default: true },
        "400K": { token_count: 400_000 },
      },
      thinking_config: {
        enabled: { efforts: { xhigh: { is_default: true }, low: {}, medium: {} } },
      },
    },
    { key: "retired", display_name: "Retired", enable: false, is_vl: false, is_reasoning: false },
  ],
});

Deno.test("catalog fetch signs and falls back across gateway hosts, and models.list projects the enabled chat models", async () => {
  const urls: string[] = [];
  let authorization = "";
  const identity = cosyIdentity(accountData(snapshot(account())));
  const models = await fetchCatalog(
    identity,
    context({
      fetch: (url, init) => {
        urls.push(url);
        authorization = init?.headers?.authorization ?? "";
        if (url.startsWith("https://api1.")) return response("boom", 500);
        assertEquals(init?.method, "GET");
        assertEquals(init?.headers?.accept, "application/json");
        assertEquals(init?.headers?.["cosy-clienttype"], "5");
        assertEquals(typeof init?.headers?.["cosy-machineid"], "string");
        return response(catalogBody);
      },
    }),
  );
  assertEquals(urls, [
    "https://api1.qoder.sh/algo/api/v2/model/list?Encode=1",
    "https://api2.qoder.sh/algo/api/v2/model/list?Encode=1",
  ]);
  assert(authorization.startsWith("Bearer COSY."));
  assertEquals(models.length, 2);

  const parsed = parseCatalog(catalogBody);
  assertEquals(parsed.map((model) => model.key), ["auto", "qmodel_38max"]);
  assertEquals(parsed[1].images, true);

  const definitions = await qoderModelsList(account());
  assertEquals(definitions.map((model) => model.id), ["auto", "qmodel_38max"]);
  assertEquals(definitions[0].displayName, "自动");
  assertEquals(definitions[0].description, "智能选择最适合的模型");
  assertEquals(definitions[1].displayName, "Qwen3.8-Max");
  assertEquals(definitions[1].description, "千问最新一代大基座模型（markdown）。\n\n0.5x");
  assertEquals(definitions[1].capabilities?.images, true);
  assertEquals(definitions[1].effortOptions, ["low", "medium", "xhigh"]);
  assertEquals(definitions[1].contextOptions, ["200k", "400k", "1m"]);
  assertEquals((definitions[1].privateData as Record<string, JsonValue>).key, "qmodel_38max");

  let failed = false;
  try {
    await qoderModelsList(null);
  } catch {
    failed = true;
  }
  assert(failed);
});

Deno.test("text fetch downloads the OSS copy once per published version and stays empty on failures", async () => {
  const ossUrl = "https://qoder-ide.oss-accelerate.aliyuncs.com/ide-text/qoder-ide/cache.json";
  const urls: string[] = [];
  const fetch = (url: string) => {
    urls.push(url);
    return url.startsWith("https://center.qoder.sh/")
      ? response(versionBody("text-version-cache", ossUrl))
      : response(textsBody);
  };
  const first = await fetchModelTexts(context({ fetch }));
  assertEquals(first.get("auto")?.displayName, "自动");
  const second = await fetchModelTexts(context({ fetch }));
  assertEquals(second.get("qmodel_38max")?.description, "千问最新一代大基座模型（markdown）。");
  assertEquals(urls, [
    "https://center.qoder.sh/ide-text/latest?namespace=qoder-ide",
    ossUrl,
    "https://center.qoder.sh/ide-text/latest?namespace=qoder-ide",
  ]);

  const goneUrl = "https://qoder-ide.oss-accelerate.aliyuncs.com/ide-text/qoder-ide/gone.json";
  const missing = await fetchModelTexts(context({
    fetch: (url) =>
      url.startsWith("https://center.qoder.sh/")
        ? response(versionBody("text-version-missing", goneUrl))
        : response("boom", 500),
  }));
  assertEquals(missing.size, 0);

  const down = await fetchModelTexts(context({
    fetch: () => response("down", 503),
  }));
  assertEquals(down.size, 0);
});

async function qoderModelsList(privateData: JsonValue | null) {
  const ossUrl = "https://qoder-ide.oss-accelerate.aliyuncs.com/ide-text/qoder-ide/list.json";
  return await qoderProvider.models!.list(
    { resource: privateData === null ? null : snapshot(privateData) },
    context({
      fetch: (url) => {
        if (url.startsWith("https://center.qoder.sh/")) {
          return response(versionBody("text-version-list", ossUrl));
        }
        if (url.startsWith("https://qoder-ide.oss")) {
          return response(textsBody);
        }
        return response(catalogBody);
      },
    }),
  );
}

// ------------------------------------------------------------------- requests

Deno.test("agent body projects instructions, images, limits, tools and tool results", () => {
  const input = invokeInput();
  input.request.instructions = "You are a coding assistant.";
  input.request.messages = [
    {
      role: "user",
      content: [
        { type: "image", mediaType: "image/png", dataBase64: "AAAA" },
        { type: "text", text: "describe it" },
      ],
    },
  ];
  input.request.reasoning = { enabled: true, effort: "high" };
  const body = buildAgentBody(
    input.model,
    input.request,
    accountData(input.resource!),
    requestIdentity(input.request),
  );

  const messages = body.messages as Record<string, JsonValue>[];
  assertEquals(messages[0].role, "system");
  assertEquals(messages[0].content, "You are a coding assistant.");
  assertEquals(messages[1].role, "user");
  assertEquals(messages[1].content, "");
  const contents = messages[1].contents as Record<string, JsonValue>[];
  assertEquals(contents[0].type, "image_url");
  assertEquals(contents[1].text, "describe it");
  assertEquals((body.image_urls as string[])[0].startsWith("data:image/png;base64,"), true);
  assertEquals(
    ((body.chat_context as Record<string, JsonValue>).imageUrls as string[]).length,
    1,
  );
  assertEquals(
    ((body.chat_context as Record<string, JsonValue>).text as Record<string, JsonValue>).text,
    "describe it",
  );
  assertEquals((body.model_config as Record<string, JsonValue>).key, "qmodel_38max");
  assertEquals((body.model_config as Record<string, JsonValue>).is_reasoning, true);
  assertEquals((body.parameters as Record<string, JsonValue>).max_tokens, 32_000);
  assertEquals((body.parameters as Record<string, JsonValue>).reasoning_effort, "high");
  assertEquals(body.aliyun_user_type, "teams");
  assertEquals(body.agent_id, "agent_common");
  assertEquals(body.session_type, "qodercli");
  assertEquals(body.tools, []);
  assertEquals((body.business as Record<string, JsonValue>).name, "describe it");

  const toolInput = invokeInput();
  toolInput.request.tools = [{
    name: "echo",
    description: "Echo",
    parameters: { type: "object", properties: {}, required: [] },
  }];
  toolInput.request.messages = [
    { role: "user", content: [{ type: "text", text: "use echo" }] },
    {
      role: "assistant",
      text: "",
      thinking: "",
      replayState: null,
      toolCalls: [{ index: 0, callId: "call-1", name: "echo", arguments: { text: "ping" } }],
    },
    { role: "tool", callId: "call-1", name: "echo", content: "ping", isError: false, parts: [] },
  ];
  toolInput.request.reasoning = { enabled: false, effort: null };
  const toolBody = buildAgentBody(
    toolInput.model,
    toolInput.request,
    { ...accountData(toolInput.resource!), userType: null },
    requestIdentity(toolInput.request),
  );

  const tools = toolBody.tools as Record<string, JsonValue>[];
  assertEquals((tools[0].function as Record<string, JsonValue>).name, "echo");
  const toolMessages = toolBody.messages as Record<string, JsonValue>[];
  const offset = toolInput.request.instructions ? 1 : 0;
  const assistant = toolMessages[offset + 1];
  assertEquals((assistant.tool_calls as Record<string, JsonValue>[])[0].id, "call-1");
  assertEquals(toolMessages[offset + 2].role, "tool");
  assertEquals(toolMessages[offset + 2].tool_call_id, "call-1");
  assertEquals(toolMessages[offset + 2].name, "echo");
  assertEquals(toolMessages[offset + 2].content, "ping");
  assertEquals(toolBody.aliyun_user_type, "personal_professional_trial");
  assertEquals("reasoning_effort" in (toolBody.parameters as Record<string, JsonValue>), false);
});

Deno.test("fast mode enables the highspeed switch and request identity stays stable per conversation", () => {
  const input = invokeInput();
  input.model.privateData = {
    key: "qmodel_38max",
    function_switches: [
      { key: "HighSpeed", type: "bool", options: { true: { label: "Fast" }, false: {} } },
      { key: "vision_hint", type: "enum", options: { auto: {} } },
    ],
  };
  input.request.latency = "fast";
  const fast = buildAgentBody(
    input.model,
    input.request,
    accountData(input.resource!),
    requestIdentity(input.request),
  );
  // The catalog's own spelling is kept; only `highspeed` counts.
  assertEquals((fast.business as Record<string, JsonValue>).feature_switches, { HighSpeed: true });

  input.model.privateData = {
    key: "qmodel_38max",
    function_switches: [{ key: "vision_hint", type: "bool", options: { true: {}, false: {} } }],
  };
  const unrelated = buildAgentBody(
    input.model,
    input.request,
    accountData(input.resource!),
    requestIdentity(input.request),
  );
  assertEquals("feature_switches" in (unrelated.business as Record<string, JsonValue>), false);

  input.request.latency = "standard";
  const standard = buildAgentBody(
    input.model,
    input.request,
    accountData(input.resource!),
    requestIdentity(input.request),
  );
  assertEquals("feature_switches" in (standard.business as Record<string, JsonValue>), false);

  // `request_set_id` stays stable for the conversation while `request_id` is per turn.
  assertEquals(fast.session_id, "conversation-1");
  assertEquals(fast.request_set_id, standard.request_set_id);
  assert(fast.request_id !== standard.request_id);
});

// --------------------------------------------------------------------- stream

Deno.test("agent stream maps text, reasoning, tool calls, usage and finish and repairs fragmented frames", async () => {
  const input = invokeInput();
  const events = await collect(input, [
    chunk({ content: "", reasoning_content: "", role: "assistant" }),
    chunk({ content: "", reasoning_content: "We need" }),
    chunk({ content: "pong" }),
    chunk({
      tool_calls: [{
        index: 0,
        id: "call-1",
        type: "function",
        function: { name: "echo", arguments: '{"text":' },
      }],
    }),
    chunk({
      tool_calls: [{ index: 0, function: { arguments: '"ping"}' } }],
    }),
    envelope({
      choices: [],
      usage: {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        completion_tokens_details: { reasoning_tokens: 3 },
        prompt_tokens_details: { cached_tokens: 4 },
      },
    }),
    envelope("[DONE]"),
  ]);

  assertEquals(
    events.filter((event) => event.type === "thinking-delta").map((event) => event.type),
    ["thinking-delta"],
  );
  const text = events.filter((event) => event.type === "text-delta").map((event) =>
    event.type === "text-delta" ? event.text : ""
  ).join("");
  assertEquals(text, "pong");
  const start = events.find((event) => event.type === "tool-call-start");
  assert(start?.type === "tool-call-start" && start.callId === "call-1");
  const toolArgs = events
    .filter((event) => event.type === "tool-call-arguments-delta")
    .map((event) => event.type === "tool-call-arguments-delta" ? event.delta : "")
    .join("");
  assertEquals(toolArgs, '{"text":"ping"}');
  const usage = events.find((event) => event.type === "usage");
  assert(usage?.type === "usage");
  assertEquals(usage.usage.inputTokens, 10);
  assertEquals(usage.usage.cacheReadTokens, 4);
  assertEquals(usage.usage.reasoningTokens, 3);
  const done = events.find((event) => event.type === "done");
  assert(done?.type === "done" && done.reason === "tool-use");

  const fragmented = await collect(invokeInput(), [
    'data:{"headers":{},"body":"{\\"choices\\":[{\\"delta\\":{\\"content\\":\\"po',
    'ng\\"},\\"index\\":0}]}","statusCodeValue":200}',
    "",
    envelope("[DONE]"),
  ]);
  const fragmentedText = fragmented.filter((event) => event.type === "text-delta").map((event) =>
    event.type === "text-delta" ? event.text : ""
  ).join("");
  assertEquals(fragmentedText, "pong");
});

Deno.test("agent stream classifies in-body and envelope failures", async () => {
  const input = invokeInput();
  const cases: Array<[string[], (error: QoderChatError) => boolean]> = [
    [[envelope({ code: "105", message: "Login expired" })], isAuthenticationError],
    [[envelope({ code: "105", message: "Login expired" }, 403)], isAuthenticationError],
    [[envelope({ code: "EXCEED_QUOTA", message: "[EXCEED_QUOTA]" })], isQuotaError],
    [[envelope({ code: "10605", message: "model is queued" })], isQueuedError],
  ];
  for (const [lines, matches] of cases) {
    let caught: QoderChatError | null = null;
    try {
      await collect(input, lines);
    } catch (error) {
      assert(error instanceof QoderChatError, `expected QoderChatError for ${lines[0]}`);
      caught = error;
    }
    assert(caught !== null && matches(caught), `unclassified: ${caught?.message}`);
  }

  let transport = "";
  try {
    await collect(input, [], 500);
  } catch (error) {
    transport = error instanceof Error ? error.message : String(error);
  }
  assert(transport.includes("500"));
});

// ------------------------------------------------------------------- provider

type StubResponse = { status: number; headers: Record<string, string>; body: string };

function invokeProvider(
  resource: ResourceSnapshot | null,
  handlers: {
    fetch?: (url: string) => StubResponse;
    stream?: (url: string, call: number) => string[];
  } = {},
) {
  const events: ModelEvent[] = [];
  const fetches: string[] = [];
  const streams: string[] = [];
  const result = qoderProvider.invoke(
    invokeInput(resource),
    { emit: (event) => void events.push(event) },
    context({
      fetch: (url) => {
        fetches.push(url);
        return handlers.fetch?.(url) ?? response({});
      },
      stream: (url) => {
        streams.push(url);
        return streamResponse(handlers.stream?.(url, streams.length) ?? []);
      },
    }),
  );
  return { result, events, fetches, streams };
}

Deno.test("provider completes a turn and refreshes once to retry a rejected token", async () => {
  const completed = invokeProvider(snapshot(account()), {
    stream: () => [chunk({ content: "pong" }), envelope("[DONE]")],
  });
  const outcome = await completed.result;
  assertEquals(outcome, { status: "completed" });
  assert(completed.events.some((event) => event.type === "text-delta"));

  const retried = invokeProvider(snapshot(account()), {
    fetch: () => response({ device_token: "access-new", refresh_token: "refresh-new" }),
    stream: (_url, call) =>
      call === 1
        ? [envelope({ code: "105", message: "Login expired" })]
        : [chunk({ content: "pong" }), envelope("[DONE]")],
  });
  const retriedOutcome = await retried.result;
  assertEquals(retriedOutcome.status, "completed");
  assertEquals(retried.fetches, ["https://openapi.qoder.sh/api/v1/deviceToken/refresh"]);
  assertEquals(retried.streams.length, 2);
  assert(retried.streams.every((url) => url.startsWith("https://api1.qoder.sh/algo/")));
  assert("patch" in retriedOutcome && retriedOutcome.patch?.privateData !== undefined);
});

Deno.test("provider maps quota exhaustion to cooling and a missing account to a request error", async () => {
  const { result } = invokeProvider(snapshot(account()), {
    stream: () => [envelope({ code: "EXCEED_QUOTA", message: "[EXCEED_QUOTA]" })],
  });
  const outcome = await result;
  assertEquals(outcome.status, "resource-error");
  const state = "patch" in outcome ? outcome.patch?.state : undefined;
  assert(state?.status === "cooling");

  const missing = invokeProvider(null);
  assertEquals(await missing.result, {
    status: "request-error",
    message: "add a Qoder account before calling Qoder",
  });
  assertEquals(missing.fetches, []);
  assertEquals(missing.streams, []);
});
