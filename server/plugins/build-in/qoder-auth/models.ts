import type { JsonValue } from "cursor-byok:plugin";
import type { ModelDefinition, ModelSupport } from "cursor-byok:model";
import { fetchCatalog } from "./catalog.ts";
import { accountData, cosyIdentity } from "./resources.ts";
import { fetchModelTexts } from "./texts.ts";

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function number(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/**
 * One-line digest of the catalog entry for the host's model tooltip: the free
 * marker, the credit multiplier (skipped while the model is free) and the
 * official off-peak text. Everything the host cannot carry as a structured
 * field stays in privateData.
 */
export function describeCatalogModel(entry: JsonValue): string | undefined {
  const model = object(entry) ?? {};
  const promotion = object(model.promotion) ?? {};
  const promotionText = text(object(promotion.description)?.zh) ??
    text(object(promotion.description)?.en) ??
    text(object(promotion.badge)?.zh) ??
    text(object(promotion.badge)?.en);
  const parts: string[] = [];
  const free = model.is_free === true;
  if (free) parts.push("限免");
  const price = number(model.price_factor);
  if (price !== null && !free) parts.push(`${price}x`);
  if (promotionText) parts.push(promotionText);
  return parts.length > 0 ? parts.join(" · ") : undefined;
}

const EFFORT_ORDER = ["minimal", "low", "medium", "high", "xhigh", "max"];

/**
 * Effort levels and context windows the account may pick for this model,
 * read straight from the catalog entry. The host uses them as the model's
 * defaults; user overrides in the desktop still win.
 */
export function catalogOptions(entry: JsonValue): {
  effortOptions: string[];
  contextOptions: string[];
} {
  const model = object(entry) ?? {};
  const windows = new Map<string, number>();
  for (const [label, value] of Object.entries(object(model.context_config) ?? {})) {
    const name = label.trim().toLowerCase();
    if (!name) continue;
    const tokens = number(object(value)?.token_count);
    windows.set(name, tokens ?? windows.get(name) ?? Number.MAX_SAFE_INTEGER);
  }
  const contextOptions = [...windows.entries()]
    .sort((left, right) => left[1] - right[1])
    .map(([name]) => name);

  const efforts = Object.keys(object(object(model.thinking_config)?.enabled)?.efforts ?? {})
    .map((effort) => effort.trim().toLowerCase())
    .filter(Boolean);
  const effortOptions = [
    ...EFFORT_ORDER.filter((effort) => efforts.includes(effort)),
    ...efforts.filter((effort) => !EFFORT_ORDER.includes(effort)).sort(),
  ];
  return { effortOptions, contextOptions };
}

/**
 * The catalog is fetched from Qoder on every listing, so models appear and
 * disappear exactly as the official clients see them. Official copy from the
 * IDE text service names the model and leads the tooltip; the catalog digest
 * of free, price and promotion flags trails it as a second paragraph. The
 * upstream entry is kept as private data because the chat request reuses its
 * capabilities.
 */
export const qoderModels: ModelSupport = {
  async list({ resource }, context): Promise<ModelDefinition[]> {
    if (!resource) throw new Error("add a Qoder account before listing Qoder models");
    const data = accountData(resource);
    const [catalog, texts] = await Promise.all([
      fetchCatalog(cosyIdentity(data), context),
      fetchModelTexts(context),
    ]);
    return catalog.map((model) => {
      const official = texts.get(model.key);
      const parts = [official?.description, describeCatalogModel(model.entry)]
        .filter((part): part is string => Boolean(part));
      const { effortOptions, contextOptions } = catalogOptions(model.entry);
      return {
        id: model.key,
        displayName: official?.displayName ?? model.displayName,
        ...(parts.length > 0 ? { description: parts.join("\n\n") } : {}),
        ...(effortOptions.length > 0 ? { effortOptions } : {}),
        ...(contextOptions.length > 0 ? { contextOptions } : {}),
        capabilities: { images: model.images },
        privateData: model.entry,
      };
    });
  },
};
