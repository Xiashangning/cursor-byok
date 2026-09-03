export const defaultCustomHeaders = {
  "User-Agent": "claude-cli/2.1.177 (external, cli)",
  "anthropic-beta": "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,effort-2025-11-24",
};

export const defaultCustomHeadersText = JSON.stringify(defaultCustomHeaders, null, 2);

export const defaultEffortOptions = ["low", "medium", "high", "xhigh", "max"];

export const defaultContextOptions = ["200k", "356k", "800k", "1m"];

/** 与 server 端 parse_token_count 一致：支持 200k / 1m / 裸数字。 */
export function parseTokenCount(value: string): number | null {
  const match = /^(\d+)([km])?$/i.exec(value.trim());
  if (!match) return null;
  const unit = match[2]?.toLowerCase();
  const multiplier = unit === "k" ? 1_000 : unit === "m" ? 1_000_000 : 1;
  return Number(match[1]) * multiplier;
}

/** 与 server 端 format_token_count 一致。 */
export function formatTokenCount(tokens: number): string {
  if (tokens >= 1_000_000 && tokens % 1_000_000 === 0) return `${tokens / 1_000_000}m`;
  if (tokens >= 1_000 && tokens % 1_000 === 0) return `${tokens / 1_000}k`;
  return String(tokens);
}
