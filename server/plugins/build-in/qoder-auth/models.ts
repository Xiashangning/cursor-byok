import type { JsonValue, LocalizedText } from "cursor-byok:plugin";
import { object, text } from "cursor-byok:json";
import type { ModelDefinition, ModelSupport } from "cursor-byok:model";
import { fetchCatalog } from "./catalog.ts";
import { accountData, cosyIdentity } from "./resources.ts";
import { fetchModelTexts } from "./texts.ts";

/**
 * Local `number` is stricter than the shared one on purpose: catalog prices
 * must be real JSON numbers, numeric strings do not count.
 */
function number(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/**
 * Free / credit-multiplier / promotion digest as a localized note. The host
 * refreshes these every 60s alongside quota and shows them as their own
 * underlined tooltip row, so they are never baked into the editable remark.
 */
function noteParts(entry: JsonValue | undefined): LocalizedText | null {
  const model = object(entry) ?? {};
  const free = model.is_free === true;
  const price = number(model.price_factor);
  const promotion = object(model.promotion);
  const promotionText = (locale: "en-US" | "zh-CN") => {
    const key = locale === "zh-CN" ? "zh" : "en";
    return text(object(promotion?.description)?.[key]) ?? text(object(promotion?.badge)?.[key]);
  };
  const parts = (locale: "en-US" | "zh-CN"): string[] => {
    const parts: string[] = [];
    if (free) parts.push(locale === "zh-CN" ? "限免" : "Free");
    if (price !== null && !free) {
      parts.push(locale === "zh-CN" ? `${price}x 额度消耗` : `${price}x credit cost`);
    }
    const promo = promotionText(locale) ?? promotionText(locale === "zh-CN" ? "en-US" : "zh-CN");
    if (promo) parts.push(promo);
    return parts;
  };
  const zh = parts("zh-CN");
  if (zh.length === 0) return null;
  return { "en-US": parts("en-US").join(" · "), "zh-CN": zh.join(" · ") };
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
 * IDE text service names the model and leads the tooltip; the free / credit
 * multiplier digest moves to the dynamic `notes` hook so it refreshes with
 * quota instead of freezing into the editable remark. The upstream entry is
 * kept as private data because the chat request reuses its capabilities.
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
      const { effortOptions, contextOptions } = catalogOptions(model.entry);
      return {
        id: model.key,
        displayName: official?.displayName ?? model.displayName,
        ...(official?.description ? { description: official.description } : {}),
        ...(effortOptions.length > 0 ? { effortOptions } : {}),
        ...(contextOptions.length > 0 ? { contextOptions } : {}),
        capabilities: { images: model.images },
        privateData: model.entry,
      };
    });
  },

  /** 限免 / 额度消耗倍率 / 促销文案:从宿主已同步的模型快照 privateData 派生,不再拉上游目录。 */
  notes({ models }) {
    const derived = models.flatMap((model) => {
      const text = noteParts(model.privateData);
      return text ? [{ modelId: model.id, text }] : [];
    });
    return Promise.resolve(derived);
  },
};
