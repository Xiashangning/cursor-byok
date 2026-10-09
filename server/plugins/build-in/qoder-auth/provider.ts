import type { PluginContext } from "cursor-byok:plugin";
import type {
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import {
  isAuthenticationError,
  isQueuedError,
  isQuotaError,
  QoderChatError,
  streamQoderChat,
} from "./agent_stream.ts";
import { qoderModels } from "./models.ts";
import {
  accessTokenExpiring,
  type AccountData,
  accountData,
  accountPrivateData,
  coolingPatch,
  latestAccountData,
  quotaExhaustedPatch,
  RESOURCE_TYPE,
  sharedRefreshAccessToken,
} from "./resources.ts";

const DEFAULT_RETRY_MS = 1_000;

function invalidResult(message: string, data?: AccountData): ProviderResult {
  return {
    status: "resource-error",
    message,
    patch: {
      ...(data ? { privateData: accountPrivateData(data) } : {}),
      state: { status: "invalid", message: "Qoder authorization expired; sign in again" },
    },
  };
}

/**
 * One Qoder turn: pre-flight token rotation, then the signed gateway call.
 * A rejected access token is refreshed and retried once, and every outcome is
 * mapped onto the resource patches the host persists.
 */
async function invoke(
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  if (!input.resource) {
    return { status: "request-error", message: "add a Qoder account before calling Qoder" };
  }
  let data: AccountData;
  let storedAccessToken: string;
  try {
    data = accountData(input.resource);
    storedAccessToken = data.accessToken;
    data = latestAccountData(input.resource.id, data);
  } catch (error) {
    return invalidResult(error instanceof Error ? error.message : String(error));
  }

  let refreshed = data.accessToken !== storedAccessToken;
  if (accessTokenExpiring(data)) {
    try {
      const next = await sharedRefreshAccessToken(input.resource.id, data, context);
      if (!next) return invalidResult("Qoder refresh token expired", data);
      data = next;
      refreshed = true;
    } catch (error) {
      return {
        status: "request-error",
        message: error instanceof Error ? error.message : String(error),
        ...(refreshed ? { patch: { privateData: accountPrivateData(data) } } : {}),
      };
    }
  }

  let emitted = false;
  const trackedOutput: ProviderOutput = {
    emit(event) {
      emitted = true;
      output.emit(event);
    },
  };

  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      await streamQoderChat(
        { model: input.model, request: input.request, data },
        trackedOutput,
        context,
      );
      data = latestAccountData(input.resource.id, data);
      refreshed ||= data.accessToken !== storedAccessToken;
      return refreshed
        ? { status: "completed", patch: { privateData: accountPrivateData(data) } }
        : { status: "completed" };
    } catch (error) {
      data = latestAccountData(input.resource.id, data);
      refreshed ||= data.accessToken !== storedAccessToken;
      if (error instanceof QoderChatError) {
        if (isAuthenticationError(error)) {
          // The gateway rejects a stale token mid-stream too, so retry once
          // with a rotated credential as long as nothing was emitted yet.
          if (error.refreshableAuth && attempt === 0 && !emitted && data.refreshToken) {
            try {
              const next = await sharedRefreshAccessToken(input.resource.id, data, context);
              if (!next) return invalidResult(error.message, data);
              data = next;
              refreshed = true;
              continue;
            } catch (refreshError) {
              return {
                status: "request-error",
                message: refreshError instanceof Error
                  ? refreshError.message
                  : String(refreshError),
                ...(refreshed ? { patch: { privateData: accountPrivateData(data) } } : {}),
              };
            }
          }
          return invalidResult(error.message, data);
        }
        if (isQuotaError(error)) {
          const retryAtMs = error.retryAfterMs === null
            ? undefined
            : Date.now() + error.retryAfterMs;
          return {
            status: "resource-error",
            message: error.message,
            patch: quotaExhaustedPatch(data, retryAtMs),
          };
        }
        if (isQueuedError(error)) {
          const retryAtMs = Date.now() + (error.retryAfterMs ?? DEFAULT_RETRY_MS);
          return {
            status: "request-error",
            message: error.message,
            patch: coolingPatch(data, retryAtMs, "Qoder model is queued; retry later"),
          };
        }
        return {
          status: "request-error",
          message: error.message,
          ...(refreshed ? { patch: { privateData: accountPrivateData(data) } } : {}),
        };
      }
      return {
        status: "request-error",
        message: error instanceof Error ? error.message : String(error),
        ...(refreshed ? { patch: { privateData: accountPrivateData(data) } } : {}),
      };
    }
  }
  return { status: "request-error", message: "Qoder request retry limit reached" };
}

export const qoderProvider: ProviderSupport = {
  id: "qoder",
  displayName: "Ali Qoder",
  description: {
    "en-US": "Qoder subscription access through the official agent gateway.",
    "zh-CN": "通过官方 agent 网关使用 Qoder 订阅。",
  },
  providerType: "qoder",
  resourceType: RESOURCE_TYPE,
  models: qoderModels,
  invoke,
};
