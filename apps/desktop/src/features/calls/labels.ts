import type { LlmCall } from "../../shared/api";

/** Shared Call-kind and route labels for the calls table and detail view. */
export function callKindLabel(kind: LlmCall["call_kind"]): string {
  if (kind === "cursor_official") return t("Cursor 官方");
  if (kind === "cursor_transport") return t("Cursor 追踪");
  return "LLM";
}

export function routeLabel(route: LlmCall["route"]): string {
  return route === "cursor_official" ? t("Cursor 官方") : "BYOK";
}
