import type {
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { PluginContext } from "cursor-byok:plugin";
import { HttpError, streamOpenAiChat } from "cursor-byok:protocol/openai-chat";
import { kimiModels } from "./models.ts";
import { type AccountData, accountData, quotaExhaustedPatch, RESOURCE_TYPE } from "./resources.ts";

const CHAT_URL = "https://api.kimi.com/coding/v1/chat/completions";

function invalidResult(message: string, stateMessage: string): ProviderResult {
  return {
    status: "resource-error",
    message,
    patch: { state: { status: "invalid", message: stateMessage } },
  };
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
  try {
    data = accountData(input.resource);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return invalidResult(message, message);
  }
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
    return { status: "completed" };
  } catch (error) {
    if (error instanceof HttpError) {
      if (error.status === 401 || error.status === 403) {
        return invalidResult(error.message, "Kimi authorization expired; sign in again");
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
