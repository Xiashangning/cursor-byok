/**
 * Streams one Qoder agent turn over the signed CLI gateway and maps its SSE
 * frames onto the host's normalized model events.
 *
 * Each frame is `{"body": "<json>", "statusCodeValue": 200, "statusCode": "OK"}`
 * around an OpenAI-shaped chunk; failures arrive the same way (HTTP 200 with a
 * non-200 `statusCodeValue`, or an in-body `code` such as 105 "Login expired"),
 * so both layers are classified before the stream is trusted.
 */
import type { PluginContext } from "cursor-byok:plugin";
import type { LlmRequest, ModelEvent, ModelUsage, ProviderOutput } from "cursor-byok:provider";
import type { ModelSnapshot } from "cursor-byok:model";
import { buildAgentBody, requestIdentity } from "./agent_request.ts";
import type { AccountData } from "./resources.ts";
import { cosySession, qoderEncode } from "./sign.ts";

const HOSTS = ["https://api1.qoder.sh", "https://api2.qoder.sh", "https://api3.qoder.sh"];
const CHAT_PATH = "/algo/api/v2/service/pro/sse/agent_chat_generation" +
  "?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";

export class QoderChatError extends Error {
  constructor(
    readonly status: number | null,
    message: string,
    readonly retryAfterMs: number | null = null,
    readonly refreshableAuth = false,
  ) {
    super(message);
  }
}

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function number(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && value.trim()) {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : null;
  }
  return null;
}

function parseJson(value: string): unknown {
  try {
    return JSON.parse(value);
  } catch {
    return null;
  }
}

function errorMessage(value: unknown): string {
  if (typeof value === "string") {
    const parsed = parseJson(value);
    return parsed === null ? value : errorMessage(parsed);
  }
  const source = object(value);
  if (!source) return String(value ?? "Qoder request failed");
  const nested = object(source.error);
  return text(
    source.message ?? source.detail ?? nested?.message ?? source.error_description ?? source.error,
  ) ?? JSON.stringify(source);
}

function retryDelayMs(value: unknown): number | null {
  const source = object(typeof value === "string" ? parseJson(value) : value);
  if (!source) return null;
  const milliseconds = number(source.retryAfterMs ?? source.retry_after_ms);
  if (milliseconds !== null && milliseconds >= 0) return milliseconds;
  const seconds = number(source.retryAfter ?? source.retry_after ?? source.retryAfterSeconds);
  if (seconds !== null && seconds >= 0) return seconds * 1000;
  const queue = object(source.queue);
  const wait = number(queue?.waitTime ?? queue?.wait_time ?? source.waitTime);
  if (wait !== null && wait >= 0) return wait;
  return retryDelayMs(source.error) ?? retryDelayMs(source.body);
}

function upstreamError(status: number | null, body: unknown): QoderChatError {
  return new QoderChatError(
    status,
    status === null
      ? `Qoder error: ${errorMessage(body)}`
      : `Qoder error ${status}: ${errorMessage(body)}`,
    retryDelayMs(body),
  );
}

/** In-body failure such as `{"code":"105","message":"Login expired"}`. */
function chunkFailure(chunk: Record<string, unknown>): QoderChatError | null {
  const nested = object(chunk.error);
  const code = chunk.code ?? nested?.code ?? chunk.status ?? chunk.statusCode;
  const failed = (chunk.error !== undefined && chunk.error !== null) ||
    (code !== undefined && code !== null && code !== "" && code !== 200 && code !== "OK");
  if (!failed) return null;
  return upstreamError(number(code) ?? number(chunk.status ?? nested?.status), chunk);
}

function payloadFromBlock(lines: string[]): string | null {
  if (lines.length === 0) return null;
  // The gateway can inject a raw newline inside `data:`; valid JSON cannot
  // contain it, so joining the blank-line-delimited block restores the bytes.
  const joined = lines.join("");
  const data = joined.indexOf("data:");
  if (data < 0) return null;
  const payload = joined.slice(data + 5);
  return payload.startsWith(" ") ? payload.slice(1) : payload;
}

function completePayload(value: string): boolean {
  return value.trim() === "[DONE]" || parseJson(value) !== null;
}

/** Reassembles ordinary SSE events and Qoder's occasionally fragmented frames. */
async function* ssePayloads(lines: AsyncIterable<string>): AsyncGenerator<string> {
  let block: string[] = [];
  for await (const rawLine of lines) {
    const line = rawLine.replace(/\r$/, "");
    if (!line) {
      const payload = payloadFromBlock(block);
      if (payload) yield payload;
      block = [];
      continue;
    }
    if (block.length > 0 && line.startsWith("data:")) {
      const payload = payloadFromBlock(block);
      // An incomplete frame keeps collecting fragments; a complete one is a
      // whole event even when the gateway omitted the blank separator.
      if (payload && completePayload(payload)) {
        yield payload;
        block = [];
      }
    }
    block.push(line);
  }
  const payload = payloadFromBlock(block);
  if (payload) yield payload;
}

type ParsedPayload = { type: "done" } | { type: "skip" } | {
  type: "chunk";
  value: Record<string, unknown>;
};

function parsePayload(payload: string): ParsedPayload {
  if (payload.trim() === "[DONE]") return { type: "done" };
  const envelope = object(parseJson(payload));
  if (!envelope) throw new Error("Qoder SSE returned invalid JSON");
  const status = number(
    envelope.statusCodeValue ?? envelope.status_code_value ??
      (typeof envelope.statusCode === "number" ? envelope.statusCode : null),
  );
  if (status !== null && status !== 200) throw upstreamError(status, envelope.body ?? envelope);

  let body: unknown = envelope.body === undefined ? envelope : envelope.body;
  if (typeof body === "string") {
    if (body.trim() === "[DONE]") return { type: "done" };
    const nested = parseJson(body);
    if (nested !== null) body = nested;
    else if (body.trim().startsWith("{") || body.trim().startsWith("[")) {
      throw new Error("Qoder SSE envelope body returned invalid JSON");
    } else {
      body = { text: body };
    }
  }
  const chunk = object(body);
  if (!chunk) return { type: "skip" };
  const failure = chunkFailure(chunk);
  if (failure) throw failure;
  return { type: "chunk", value: chunk };
}

type ToolState = {
  callId: string;
  name: string;
  arguments: string;
  emitted: number;
  started: boolean;
};

type StreamState = {
  textOpen: boolean;
  thinkingOpen: boolean;
  text: string;
  reasoning: string;
  tools: Map<number, ToolState>;
  usage: ModelEvent | null;
  finish: "stop" | "length" | "tool-use" | null;
  sawDone: boolean;
};

type StreamText = { value: string; snapshot: boolean };

function contentText(value: unknown): string | null {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) {
    const parts = value.flatMap((item) => {
      if (typeof item === "string") return [item];
      const part = object(item);
      if (!part || (part.type !== undefined && part.type !== "text")) return [];
      const value = text(part.text ?? part.content);
      return value === null ? [] : [value];
    });
    return parts.length > 0 ? parts.join("") : null;
  }
  const part = object(value);
  if (!part) return null;
  return text(part.text ?? part.content);
}

function firstChoice(chunk: Record<string, unknown>): Record<string, unknown> | null {
  return Array.isArray(chunk.choices) ? object(chunk.choices[0]) : null;
}

function streamText(value: unknown, snapshot: boolean): StreamText | null {
  const content = contentText(value);
  return content === null ? null : { value: content, snapshot };
}

function reasoningText(chunk: Record<string, unknown>): StreamText | null {
  const choice = firstChoice(chunk);
  const delta = object(choice?.delta) ?? object(chunk.delta);
  const fromDelta = streamText(delta?.reasoning_content ?? delta?.reasoning, false);
  if (fromDelta !== null) return fromDelta;
  const message = object(choice?.message) ?? object(chunk.message);
  return streamText(message?.reasoning_content ?? message?.reasoning, true) ??
    streamText(chunk.reasoning_content ?? chunk.reasoning, false);
}

function responseText(chunk: Record<string, unknown>): StreamText | null {
  const choice = firstChoice(chunk);
  const delta = object(choice?.delta) ?? object(chunk.delta);
  const fromDelta = streamText(delta?.content ?? delta?.text, false);
  if (fromDelta !== null) return fromDelta;
  const message = object(choice?.message) ?? object(chunk.message);
  return streamText(message?.content ?? message?.text, true) ??
    streamText(chunk.content ?? chunk.delta ?? chunk.text, false);
}

function textDelta(current: string, next: StreamText): string {
  if (!next.value) return "";
  // Snapshot events carry the full accumulated text: emit only the new tail.
  // The length guard keeps the prefix check cheap when the delta is unrelated.
  if (next.snapshot && current && next.value.length > current.length && next.value.startsWith(current)) {
    return next.value.slice(current.length);
  }
  return next.value;
}

function mergeFragment(current: string, fragment: string): string {
  // Some providers resend the full accumulated arguments instead of a fragment.
  if (current === fragment) return current;
  if (fragment.length >= current.length) {
    if (current.length === 0 || fragment.startsWith(current)) return fragment;
  } else if (current.endsWith(fragment)) {
    return current;
  }
  return current + fragment;
}

function closeThinking(state: StreamState, output: ProviderOutput): void {
  if (!state.thinkingOpen) return;
  state.thinkingOpen = false;
  output.emit({ type: "thinking-end" });
}

function closeText(state: StreamState, output: ProviderOutput): void {
  if (!state.textOpen) return;
  state.textOpen = false;
  output.emit({ type: "text-end" });
}

function emitReasoning(value: StreamText, state: StreamState, output: ProviderOutput): void {
  const delta = textDelta(state.reasoning, value);
  if (!delta) return;
  closeText(state, output);
  if (!state.thinkingOpen) {
    state.thinkingOpen = true;
    output.emit({ type: "thinking-start" });
  }
  state.reasoning += delta;
  output.emit({ type: "thinking-delta", text: delta });
}

function emitText(value: StreamText, state: StreamState, output: ProviderOutput): void {
  const delta = textDelta(state.text, value);
  if (!delta) return;
  closeThinking(state, output);
  if (!state.textOpen) {
    state.textOpen = true;
    output.emit({ type: "text-start" });
  }
  state.text += delta;
  output.emit({ type: "text-delta", text: delta });
}

function toolCalls(chunk: Record<string, unknown>): { values: unknown[]; snapshot: boolean } {
  const choice = firstChoice(chunk);
  const delta = object(choice?.delta) ?? object(chunk.delta);
  if (Array.isArray(delta?.tool_calls)) return { values: delta.tool_calls, snapshot: false };
  const message = object(choice?.message) ?? object(chunk.message);
  if (Array.isArray(message?.tool_calls)) return { values: message.tool_calls, snapshot: true };
  return { values: Array.isArray(chunk.tool_calls) ? chunk.tool_calls : [], snapshot: false };
}

function updateTools(
  chunk: Record<string, unknown>,
  state: StreamState,
  output: ProviderOutput,
): void {
  const calls = toolCalls(chunk);
  for (const [position, raw] of calls.values.entries()) {
    const value = object(raw);
    if (!value) continue;
    const index = Math.max(0, Math.floor(number(value.index) ?? position));
    const fn = object(value.function) ?? value;
    let tool = state.tools.get(index);
    if (!tool) {
      tool = { callId: "", name: "", arguments: "", emitted: 0, started: false };
      state.tools.set(index, tool);
    }
    const callId = text(value.id ?? value.call_id);
    const name = text(fn.name);
    const rawArguments = fn.arguments;
    const fragment = typeof rawArguments === "string"
      ? rawArguments
      : rawArguments === undefined
      ? null
      : JSON.stringify(rawArguments);
    if (callId) tool.callId = mergeFragment(tool.callId, callId);
    if (name) tool.name = mergeFragment(tool.name, name);
    if (fragment !== null) {
      tool.arguments = calls.snapshot || typeof rawArguments !== "string"
        ? mergeFragment(tool.arguments, fragment)
        : tool.arguments + fragment;
    }
    if (!tool.started && tool.name && tool.callId) {
      closeThinking(state, output);
      closeText(state, output);
      tool.started = true;
      output.emit({ type: "tool-call-start", index, callId: tool.callId, name: tool.name });
    }
    if (tool.started && tool.emitted < tool.arguments.length) {
      output.emit({
        type: "tool-call-arguments-delta",
        index,
        delta: tool.arguments.slice(tool.emitted),
      });
      tool.emitted = tool.arguments.length;
    }
  }
}

function tokenCount(value: unknown): number | null {
  const count = number(value);
  return count === null ? null : Math.max(0, Math.floor(count));
}

function usageEvent(value: unknown): ModelEvent {
  const usage = object(value) ?? {};
  const promptDetails = object(usage.prompt_tokens_details ?? usage.input_tokens_details);
  const completionDetails = object(usage.completion_tokens_details ?? usage.output_tokens_details);
  const modelUsage: ModelUsage = {
    inputTokens: tokenCount(usage.prompt_tokens ?? usage.input_tokens),
    outputTokens: tokenCount(usage.completion_tokens ?? usage.output_tokens),
    totalTokens: tokenCount(usage.total_tokens),
    cacheReadTokens: tokenCount(promptDetails?.cached_tokens ?? usage.cache_read_tokens),
    cacheWriteTokens: tokenCount(promptDetails?.cache_write_tokens ?? usage.cache_write_tokens),
    reasoningTokens: tokenCount(completionDetails?.reasoning_tokens ?? usage.reasoning_tokens),
  };
  return { type: "usage", usage: modelUsage };
}

function mapFinish(value: unknown, hasTools: boolean): "stop" | "length" | "tool-use" | null {
  const reason = text(value);
  if (reason === null) return null;
  if (reason === "tool_calls" || reason === "function_call" || reason === "tool-use") {
    return "tool-use";
  }
  if (reason === "length" || reason === "max_tokens") return "length";
  if (
    reason === "stop" || reason === "end" || reason === "end_turn" || reason === "content_filter"
  ) {
    return "stop";
  }
  return hasTools ? "tool-use" : "stop";
}

function processChunk(
  chunk: Record<string, unknown>,
  state: StreamState,
  output: ProviderOutput,
): void {
  if (chunk.usage !== undefined && chunk.usage !== null) state.usage = usageEvent(chunk.usage);
  const reasoning = reasoningText(chunk);
  if (reasoning !== null) emitReasoning(reasoning, state, output);
  const visible = responseText(chunk);
  if (visible !== null) emitText(visible, state, output);
  updateTools(chunk, state, output);
  const choice = firstChoice(chunk);
  const finish = mapFinish(choice?.finish_reason ?? chunk.finish_reason, state.tools.size > 0);
  if (finish !== null) state.finish = finish;
}

async function readBody(lines: AsyncIterable<string>): Promise<string> {
  const body: string[] = [];
  for await (const line of lines) body.push(line);
  return body.join("\n");
}

function retryAfterHeader(headers: Record<string, string>): number | null {
  const key = Object.keys(headers).find((candidate) => candidate.toLowerCase() === "retry-after");
  const value = key ? headers[key] : null;
  if (!value) return null;
  const seconds = Number(value);
  if (Number.isFinite(seconds) && seconds >= 0) return seconds * 1000;
  const date = Date.parse(value);
  return Number.isFinite(date) ? Math.max(0, date - Date.now()) : null;
}

/** Stream one agent turn, emitting normalized events; throws `QoderChatError`. */
export async function streamQoderChat(
  input: { model: ModelSnapshot; request: LlmRequest; data: AccountData },
  output: ProviderOutput,
  context: PluginContext,
): Promise<void> {
  const identity = requestIdentity(input.request);
  const body = qoderEncode(
    JSON.stringify(buildAgentBody(input.model, input.request, input.data, identity)),
  );
  const session = await cosySession({
    uid: input.data.userId ?? "",
    name: input.data.userName ?? "",
    organizationId: input.data.profile?.organizationId ?? "",
    organizationName: input.data.profile?.organizationName ?? "",
    userType: input.data.userType ?? "",
    accessToken: input.data.accessToken,
    refreshToken: input.data.refreshToken ?? "",
  });

  let failure: Error | null = null;
  let response: Awaited<ReturnType<PluginContext["network"]["stream"]>> | null = null;
  for (const host of HOSTS) {
    const url = host + CHAT_PATH;
    const signed = session.sign(body, url);
    try {
      response = await context.network.stream(url, {
        method: "POST",
        headers: {
          ...signed.headers,
          accept: "text/event-stream",
          "cache-control": "no-cache",
          "x-model-key": input.model.id,
          "x-model-source": "system",
          "x-request-id": identity.requestId,
          "x-session-id": identity.sessionId,
        },
        body,
      });
      break;
    } catch (error) {
      failure = error instanceof Error ? error : new Error(String(error));
    }
  }
  if (!response) throw failure ?? new Error("Qoder gateway is unreachable");

  if (response.status < 200 || response.status >= 300) {
    const error = upstreamError(response.status, await readBody(response.lines));
    throw new QoderChatError(
      error.status,
      error.message,
      error.retryAfterMs ?? retryAfterHeader(response.headers),
      response.status === 401 || response.status === 403,
    );
  }

  const state: StreamState = {
    textOpen: false,
    thinkingOpen: false,
    text: "",
    reasoning: "",
    tools: new Map(),
    usage: null,
    finish: null,
    sawDone: false,
  };
  try {
    for await (const payload of ssePayloads(response.lines)) {
      const parsed = parsePayload(payload);
      if (parsed.type === "done") {
        state.sawDone = true;
        break;
      }
      if (parsed.type === "chunk") processChunk(parsed.value, state, output);
    }
  } catch (error) {
    // The gateway reports "Login expired" inside a 200 stream too, so mark
    // every credential failure as retryable once with a rotated token.
    if (
      error instanceof QoderChatError && isAuthenticationError(error) &&
      !error.message.toLowerCase().includes("timeout")
    ) {
      throw new QoderChatError(error.status, error.message, error.retryAfterMs, true);
    }
    throw error;
  }

  closeThinking(state, output);
  closeText(state, output);
  for (const [index, tool] of state.tools) {
    if (!tool.started) {
      if (!tool.name) throw new Error("Qoder tool call is missing name");
      tool.callId ||= `call-${index}`;
      output.emit({ type: "tool-call-start", index, callId: tool.callId, name: tool.name });
      if (tool.arguments) {
        output.emit({ type: "tool-call-arguments-delta", index, delta: tool.arguments });
      }
    }
    output.emit({ type: "tool-call-end", index });
  }
  if (state.usage !== null) output.emit(state.usage);
  const sawContent = state.text || state.reasoning || state.tools.size > 0;
  if (!sawContent && state.usage === null) {
    throw new Error("Qoder stream returned an empty response");
  }
  const reason = state.finish ??
    (state.sawDone ? (state.tools.size > 0 ? "tool-use" : "stop") : null);
  if (reason === null) throw new Error("Qoder stream ended without finish_reason");
  output.emit({ type: "done", reason });
}

export function isAuthenticationError(error: QoderChatError): boolean {
  const message = error.message.toLowerCase();
  return error.status === 401 || error.status === 403 || error.status === 105 ||
    message.includes("login expired") || message.includes("invalid token") ||
    message.includes("token expired") || message.includes("unauthorized") ||
    (message.includes("timeout") && (error.status === null || error.status < 500));
}

export function isQuotaError(error: QoderChatError): boolean {
  const message = error.message.toLowerCase();
  return error.status === 429 || message.includes("exceed_quota") ||
    message.includes("quota exceeded") || message.includes("quota_exceeded");
}

export function isQueuedError(error: QoderChatError): boolean {
  const message = error.message.toLowerCase();
  return message.includes("duplicate") || message.includes("model queued") ||
    message.includes("model is queued") || message.includes("10605") ||
    message.includes('"isqueued":true');
}
