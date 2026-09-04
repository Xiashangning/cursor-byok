import type { ModelDefinition, ModelSupport } from "cursor-byok:model";
import { accountData } from "./resources.ts";

const MODELS_URL = "https://api.kimi.com/coding/v1/models";

/** 模型列表不可用时回退到已知的 Kimi Code 编程套餐模型。 */
export const FALLBACK_MODELS: ModelDefinition[] = [
  {
    id: "kimi-for-coding",
    displayName: "Kimi for Coding",
    capabilities: { images: false },
    privateData: { thinking: true },
  },
];

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function positiveInteger(value: unknown): number | null {
  const parsed = typeof value === "number"
    ? value
    : typeof value === "string"
    ? Number(value)
    : NaN;
  return Number.isFinite(parsed) && parsed > 0 ? Math.floor(parsed) : null;
}

/** 把模型 ID 变成可读名称,如 kimi-for-coding → Kimi for Coding 由上游 display_name 提供。 */
function displayName(id: string): string {
  return id
    .split("-")
    .map((part) => (/^\d/.test(part) ? part : part.charAt(0).toUpperCase() + part.slice(1)))
    .join(" ");
}

/** 解析 /coding/v1/models 的 data 数组,字段与官方 Kimi CLI 一致。 */
export function parseKimiModels(body: unknown): ModelDefinition[] {
  const root = object(body);
  const source = root?.data ?? body;
  if (!Array.isArray(source)) {
    throw new Error("Kimi model discovery response does not contain a model list");
  }
  const seen = new Set<string>();
  const models: ModelDefinition[] = [];
  for (const raw of source) {
    const model = object(raw);
    const id = model ? text(model.id) : null;
    if (!id || seen.has(id)) continue;
    seen.add(id);
    const contextWindowTokens = positiveInteger(model?.context_length);
    const thinking = model?.supports_reasoning === true || id.toLowerCase().includes("thinking");
    models.push({
      id,
      displayName: text(model?.display_name) ?? displayName(id),
      capabilities: {
        images: model?.supports_image_in === true,
      },
      // SDK 契约之外的模型元数据放 privateData,随快照原样回传(同 codex 的
      // reasoningEfforts 模式)。
      privateData: {
        thinking,
        ...(contextWindowTokens !== null ? { contextWindowTokens } : {}),
      },
    });
  }
  return models;
}

export const kimiModels: ModelSupport = {
  list: async ({ resource }, context): Promise<ModelDefinition[]> => {
    if (!resource) throw new Error("add a Kimi account before syncing models");
    const data = accountData(resource);
    const response = await context.network.fetch(MODELS_URL, {
      method: "GET",
      headers: {
        accept: "application/json",
        authorization: `Bearer ${data.accessToken}`,
      },
    });
    if (response.status < 200 || response.status >= 300) {
      return FALLBACK_MODELS;
    }
    let body: unknown;
    try {
      body = JSON.parse(response.body);
    } catch {
      throw new Error("Kimi model discovery returned invalid JSON");
    }
    const models = parseKimiModels(body);
    return models.length > 0 ? models : FALLBACK_MODELS;
  },
};
