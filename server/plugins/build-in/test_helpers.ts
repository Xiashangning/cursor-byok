import type {
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "../../src/plugin/sdk/plugin.ts";
import type { LlmRequest } from "../../src/plugin/sdk/provider.ts";

export function assert(
  condition: unknown,
  message = "assertion failed",
): asserts condition {
  if (!condition) throw new Error(message);
}

export function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

export function jwt(payload: Record<string, unknown>): string {
  const encoded = btoa(JSON.stringify(payload)).replace(/=/g, "").replace(
    /\+/g,
    "-",
  ).replace(
    /\//g,
    "_",
  );
  return `header.${encoded}.signature`;
}

type RequestInit = { method?: string; body?: string; headers?: Record<string, string> };
type FetchHandler = (
  url: string,
  init?: RequestInit,
) => NetworkResponse | Promise<NetworkResponse>;
type StreamHandler = (url: string, init?: RequestInit) => NetworkEventStream;

export function context(
  handlers: { fetch?: FetchHandler; stream?: StreamHandler },
): PluginContext {
  return {
    network: {
      fetch: (url, init) => {
        if (!handlers.fetch) throw new Error("fetch was not expected");
        return Promise.resolve(handlers.fetch(url, init));
      },
      stream: (url, init) => {
        if (!handlers.stream) throw new Error("stream was not expected");
        return Promise.resolve(handlers.stream(url, init));
      },
    },
    signal: new AbortController().signal,
  };
}

export async function* sse(lines: string[]): AsyncGenerator<string> {
  for (const line of lines) yield line;
}

/** 构造非流式网络响应;字符串 body 原样返回,其余走 JSON 序列化。 */
export function response(body: unknown, status = 200): NetworkResponse {
  return {
    status,
    headers: {},
    body: typeof body === "string" ? body : JSON.stringify(body),
  };
}

/** 构造流式网络响应;lines 直接作为 SSE 行产出。 */
export function streamResponse(lines: string[], status = 200): NetworkEventStream {
  return { status, headers: {}, lines: sse(lines) };
}

export function request(maxOutputTokens = 32_000): LlmRequest {
  return {
    instructions: "You are a coding assistant.",
    messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
    tools: [],
    reasoning: { enabled: true, effort: "medium" },
    latency: "fast",
    maxOutputTokens,
    cacheKey: "conversation-1",
  };
}
