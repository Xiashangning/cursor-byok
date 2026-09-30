/**
 * Live model catalog.
 *
 * The catalog behind every Qoder client is a signed gateway endpoint; the IDE
 * endpoint (api2-v2) has no list route at all. One signed GET per listing is
 * enough, so the plugin never ships a static model list.
 */
import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import { type CosyIdentity, cosySession } from "./sign.ts";

const HOSTS = ["https://api1.qoder.sh", "https://api2.qoder.sh", "https://api3.qoder.sh"];
const CATALOG_PATH = "/algo/api/v2/model/list?Encode=1";

export type CatalogModel = {
  key: string;
  displayName: string;
  /** Vision input supported (`is_vl`). */
  images: boolean;
  /** Reasoning model (`is_reasoning`). */
  reasoning: boolean;
  /** Full upstream entry, kept in the model snapshot's private data. */
  entry: JsonValue;
};

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

export function parseCatalog(body: string): CatalogModel[] {
  const payload = object(JSON.parse(body));
  const entries = payload?.chat;
  if (!Array.isArray(entries)) throw new Error("Qoder model catalog has no chat scene");
  const models: CatalogModel[] = [];
  for (const raw of entries) {
    const entry = object(raw);
    const key = text(entry?.key);
    // `enable: false` entries are listed by the gateway but not selectable.
    if (!key || entry?.enable === false) continue;
    models.push({
      key,
      displayName: text(entry?.display_name) ?? key,
      images: entry?.is_vl === true,
      reasoning: entry?.is_reasoning === true,
      entry: raw as JsonValue,
    });
  }
  return models;
}

/** Fetch the signed catalog, falling back across the gateway hosts. */
export async function fetchCatalog(
  identity: CosyIdentity,
  context: PluginContext,
): Promise<CatalogModel[]> {
  const session = await cosySession(identity);
  // A bare GET is accepted as long as the signature covers the same (empty)
  // body; the CLI sends the encoded "{}" only because it always encodes.
  const body = "";
  let failure: Error | null = null;
  for (const host of HOSTS) {
    const url = host + CATALOG_PATH;
    const signed = session.sign(body, url);
    try {
      const response = await context.network.fetch(url, {
        method: "GET",
        headers: { ...signed.headers, accept: "application/json" },
      });
      if (response.status < 200 || response.status >= 300) {
        failure = new Error(
          `Qoder model catalog failed (HTTP ${response.status}): ${response.body.slice(0, 200)}`,
        );
        continue;
      }
      return parseCatalog(response.body);
    } catch (error) {
      failure = error instanceof Error ? error : new Error(String(error));
    }
  }
  throw failure ?? new Error("Qoder model catalog is unavailable");
}
