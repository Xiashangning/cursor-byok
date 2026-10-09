/**
 * Official model copy.
 *
 * The IDE text service publishes localized blurbs under its own namespace: a
 * flat map keyed `modelSelector.item.<model key>`, where the bare key holds the
 * localized model name and `.description` / `.markdownDescription` the blurb.
 * The OSS JSON behind a published version never changes, so a listing only
 * downloads it when the version moves.
 */
import type { PluginContext } from "cursor-byok:plugin";
import { object, text } from "cursor-byok:json";

const VERSION_URL = "https://center.qoder.sh/ide-text/latest?namespace=qoder-ide";
const ITEM_PREFIX = "modelSelector.item.";
const MARKDOWN_SUFFIX = ".markdownDescription";
const DESCRIPTION_SUFFIX = ".description";

/** Official copy for one catalog key. */
export type ModelText = {
  /** Localized official name; the catalog display name is used when absent. */
  displayName?: string;
  /** Official introduction, markdown, same content as `.description`. */
  description?: string;
};

type CachedTexts = { version: string; texts: Map<string, ModelText> };

let cache: CachedTexts | null = null;

/** Blurb for one model key: the markdown spelling first, then the short one. */
function blurb(locale: Record<string, unknown>, key: string): string | null {
  return text(locale[ITEM_PREFIX + key + MARKDOWN_SUFFIX]) ??
    text(locale[ITEM_PREFIX + key + DESCRIPTION_SUFFIX]);
}

/** Extract per-model copy from the OSS locale JSON, preferring Chinese. */
export function parseModelTexts(body: string): Map<string, ModelText> {
  const payload = object(JSON.parse(body)) ?? {};
  const zh = object(payload.zh) ?? {};
  const en = object(payload.en) ?? {};
  const keys = new Set<string>();
  for (const locale of [zh, en]) {
    for (const key of Object.keys(locale)) {
      if (!key.startsWith(ITEM_PREFIX)) continue;
      const rest = key.slice(ITEM_PREFIX.length);
      const dot = rest.indexOf(".");
      // Slot keys like `.functionSwitch.highspeed.description` are not model copy.
      if (dot < 0) keys.add(rest);
      else if (rest.slice(dot) === MARKDOWN_SUFFIX || rest.slice(dot) === DESCRIPTION_SUFFIX) {
        keys.add(rest.slice(0, dot));
      }
    }
  }
  const texts = new Map<string, ModelText>();
  for (const key of keys) {
    const displayName = text(zh[ITEM_PREFIX + key]) ?? text(en[ITEM_PREFIX + key]);
    const description = blurb(zh, key) ?? blurb(en, key);
    if (!displayName && !description) continue;
    texts.set(key, {
      ...(displayName ? { displayName } : {}),
      ...(description ? { description } : {}),
    });
  }
  return texts;
}

/**
 * Copy for the catalog keys. Unreachable text service or a missing OSS object
 * yields nothing: blurbs only decorate a listing, so they never fail one.
 */
export async function fetchModelTexts(context: PluginContext): Promise<Map<string, ModelText>> {
  const empty = new Map<string, ModelText>();
  try {
    const response = await context.network.fetch(VERSION_URL, {
      headers: { accept: "application/json" },
    });
    if (response.status < 200 || response.status >= 300) return empty;
    const pointer = object(object(JSON.parse(response.body))?.data) ?? {};
    const version = text(pointer.version);
    const ossUrl = text(pointer.ossUrl);
    if (!version || !ossUrl) return empty;
    if (cache?.version === version) return cache.texts;
    const oss = await context.network.fetch(ossUrl, { headers: { accept: "application/json" } });
    if (oss.status < 200 || oss.status >= 300) return empty;
    const texts = parseModelTexts(oss.body);
    cache = { version, texts };
    return texts;
  } catch {
    return empty;
  }
}
