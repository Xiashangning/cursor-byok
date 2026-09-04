import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import type {
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import { HttpError, streamOpenAiChat } from "cursor-byok:protocol/openai-chat";
import { kimiModels } from "./models.ts";
import {
  type AccountData,
  accountData,
  quotaExhaustedPatch,
  refreshAccessToken,
  RESOURCE_TYPE,
  tokenExpiring,
  tokenExpiryMs,
} from "./resources.ts";

const CHAT_URL = "https://api.kimi.com/coding/v1/chat/completions";
const EXPIRED_MESSAGE = "Kimi authorization expired; sign in again";

function invalidResult(message: string, stateMessage: string): ProviderResult {
  return {
    status: "resource-error",
    message,
    patch: { state: { status: "invalid", message: stateMessage } },
  };
}

/**
 * Worker 进程内常驻的令牌状态。refresh_token 是轮换式的:同一账号的两个
 * 并发调用各自刷新,后到的会拿着已被轮换的 refresh_token 被拒,把刚刷新
 * 的账号误标为失效。按资源 ID 合并刷新,并记住最新令牌,让持久化补丁
 * 落地前后到达的调用都拿到新令牌;重新登录产生的令牌 exp 更大,总是胜出。
 */
const latestAccounts = new Map<string, AccountData>();
const pendingRefreshes = new Map<string, Promise<AccountData | null>>();

/** 单飞刷新:同一账号的并发调用共享同一次刷新;null 表示刷新被拒。 */
async function sharedRefresh(
  id: string,
  data: AccountData,
  context: PluginContext,
): Promise<AccountData | null> {
  const pending = pendingRefreshes.get(id);
  if (pending) return pending;
  const promise = refreshAccessToken(data, context)
    .then((refreshed) => {
      if (refreshed) latestAccounts.set(id, refreshed);
      return refreshed;
    })
    .finally(() => pendingRefreshes.delete(id));
  pendingRefreshes.set(id, promise);
  return promise;
}

async function invoke(
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  if (!input.resource) {
    return { status: "request-error", message: "add a Kimi account before calling Kimi" };
  }
  let data: AccountData;
  let storedAccessToken: string;
  try {
    data = accountData(input.resource);
    storedAccessToken = data.accessToken;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return invalidResult(message, message);
  }
  const id = input.resource.id;
  const cached = latestAccounts.get(id);
  if (cached && tokenExpiryMs(cached) > tokenExpiryMs(data)) data = cached;

  // 令牌临期先刷新;刷新暂时不可用时继续用现有令牌,由调用结果兜底。
  if (data.refreshToken && tokenExpiring(data)) {
    try {
      const refreshed = await sharedRefresh(id, data, context);
      if (!refreshed) return invalidResult(EXPIRED_MESSAGE, EXPIRED_MESSAGE);
      data = refreshed;
    } catch {
      // 瞬时失败不阻断调用。
    }
  }
  for (let attempt = 0;; attempt++) {
    try {
      await streamOpenAiChat(
        {
          url: CHAT_URL,
          model: input.model.id,
          // Kimi Code 端点是否接受 reasoning_effort 与 service_tier 未验证;思考由模型自身决定。
          request: {
            ...input.request,
            reasoning: { enabled: false, effort: null },
            latency: "standard",
          },
          headers: { authorization: `Bearer ${data.accessToken}` },
        },
        output,
        context,
      );
      // 刷新过令牌时经补丁持久化,下一次调用与模型同步直接用新令牌。
      return data.accessToken === storedAccessToken
        ? { status: "completed" }
        : { status: "completed", patch: { privateData: data as unknown as JsonValue } };
    } catch (error) {
      // 401/403 先刷新一次再重试:令牌可能已过期或被同账号的其他客户端轮换。
      // HttpError 只在读取响应状态时抛出,此时尚未发出任何事件,重试不会重复输出。
      if (
        error instanceof HttpError &&
        (error.status === 401 || error.status === 403) &&
        attempt === 0 &&
        data.refreshToken
      ) {
        try {
          const refreshed = await sharedRefresh(id, data, context);
          if (!refreshed) return invalidResult(error.message, EXPIRED_MESSAGE);
          data = refreshed;
          continue;
        } catch (refreshError) {
          // 刷新瞬时失败:不判定账号失效,把原因如实上报。
          return {
            status: "request-error",
            message: refreshError instanceof Error ? refreshError.message : String(refreshError),
          };
        }
      }
      if (error instanceof HttpError) {
        if (error.status === 401 || error.status === 403) {
          return invalidResult(error.message, EXPIRED_MESSAGE);
        }
        if (error.status === 429) {
          return {
            status: "resource-error",
            message: error.message,
            patch: quotaExhaustedPatch(data),
          };
        }
        return { status: "request-error", message: error.message };
      }
      return {
        status: "request-error",
        message: error instanceof Error ? error.message : String(error),
      };
    }
  }
}

export const kimiProvider: ProviderSupport = {
  id: "kimi",
  displayName: "Kimi",
  description: {
    "en-US": "Kimi for Coding subscription access through the official Kimi Code endpoint.",
    "zh-CN": "通过官方 Kimi Code 接口使用 Kimi For Coding 订阅。",
  },
  providerType: "kimi",
  resourceType: RESOURCE_TYPE,
  models: kimiModels,
  invoke,
};
