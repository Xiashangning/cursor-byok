/**
 * Projects the host's canonical LLM request onto Qoder's agent envelope.
 *
 * This is the request half of the CLI gateway protocol: the host speaks
 * `LlmRequest` (instructions, messages, tools, reasoning, limits) while the
 * gateway wants one flat agent body with a highlighted prompt, a model config
 * copy and OpenAI-shaped `contents` parts for images.
 */
import type { JsonValue } from "cursor-byok:plugin";
import type { LlmContentPart, LlmMessage, LlmRequest } from "cursor-byok:provider";
import type { ModelSnapshot } from "cursor-byok:model";
import type { AccountData } from "./resources.ts";
import { DEFAULT_USER_TYPE } from "./sign.ts";

export type RequestIdentity = {
  requestId: string;
  requestSetId: string;
  sessionId: string;
};

const REQUEST_SET_CACHE_SIZE = 512;
const requestSetIds = new Map<string, string>();

function requestSetId(sessionId: string): string {
  const existing = requestSetIds.get(sessionId);
  if (existing) return existing;
  const created = crypto.randomUUID();
  requestSetIds.set(sessionId, created);
  if (requestSetIds.size > REQUEST_SET_CACHE_SIZE) {
    const oldest = requestSetIds.keys().next().value;
    if (oldest !== undefined) requestSetIds.delete(oldest);
  }
  return created;
}

/** `request_set_id` stays stable per conversation so the gateway can group turns. */
export function requestIdentity(request: LlmRequest): RequestIdentity {
  const sessionId = request.cacheKey ?? crypto.randomUUID();
  return { requestId: crypto.randomUUID(), requestSetId: requestSetId(sessionId), sessionId };
}

function responseMeta(): JsonValue {
  return {
    id: "",
    usage: {
      prompt_tokens: 0,
      completion_tokens: 0,
      total_tokens: 0,
      completion_tokens_details: { reasoning_tokens: 0 },
      prompt_tokens_details: { cached_tokens: 0 },
    },
  };
}

function structured(role: string, content: string): Record<string, JsonValue> {
  return { role, content, response_meta: responseMeta(), reasoning_content_signature: "" };
}

function textOf(parts: LlmContentPart[]): string {
  return parts.flatMap((part) => part.type === "text" ? [part.text] : []).join("\n");
}

function imageUrl(part: Extract<LlmContentPart, { type: "image" }>): string {
  return `data:${part.mediaType};base64,${part.dataBase64}`;
}

function userMessage(parts: LlmContentPart[]): Record<string, JsonValue> {
  return {
    role: "user",
    content: "",
    contents: parts.map((part): JsonValue =>
      part.type === "text"
        ? { type: "text", text: part.text }
        : { type: "image_url", image_url: { url: imageUrl(part) } }
    ),
    response_meta: responseMeta(),
    reasoning_content_signature: "",
  };
}

function agentMessages(instructions: string, messages: LlmMessage[]): JsonValue[] {
  const built: JsonValue[] = [];
  if (instructions) built.push(structured("system", instructions));
  for (const message of messages) {
    if (message.role === "assistant") {
      const entry = structured("assistant", message.text);
      if (message.toolCalls.length > 0) {
        entry.tool_calls = message.toolCalls.map((call) => ({
          id: call.callId,
          type: "function",
          function: { name: call.name, arguments: JSON.stringify(call.arguments) },
        }));
      }
      built.push(entry);
      continue;
    }
    if (message.role === "tool") {
      built.push({
        ...structured("tool", message.content),
        name: message.name,
        tool_call_id: message.callId,
      });
      continue;
    }
    built.push(
      message.role === "user"
        ? userMessage(message.content)
        : structured("system", textOf(message.content)),
    );
  }
  return built;
}

/** Latest user text; the gateway highlights it and names the chat record. */
function latestPrompt(messages: LlmMessage[]): string {
  for (let index = messages.length - 1; index >= 0; index--) {
    const message = messages[index];
    if (message.role !== "user") continue;
    const value = textOf(message.content);
    if (value.trim()) return value;
  }
  return "";
}

/** The client's "Fast" toggle is the `highspeed` switch. */
const FAST_SWITCH_KEY = "highspeed";

/**
 * Fast mode turns on the `highspeed` switch the catalog advertises for the
 * model; the gateway reads the selection from `business.feature_switches`.
 */
export function fastFeatureSwitches(entry: JsonValue): Record<string, JsonValue> | null {
  const model = entry !== null && typeof entry === "object" && !Array.isArray(entry)
    ? entry as Record<string, JsonValue>
    : {};
  const switches = model.function_switches ?? model.feature_switches;
  if (!Array.isArray(switches)) return null;
  for (const raw of switches) {
    const item = raw !== null && typeof raw === "object" && !Array.isArray(raw)
      ? raw as Record<string, JsonValue>
      : {};
    if (typeof item.key !== "string") continue;
    const key = item.key.trim();
    if (key.toLowerCase() === FAST_SWITCH_KEY) return { [key]: true };
  }
  return null;
}

export function buildAgentBody(
  model: ModelSnapshot,
  request: LlmRequest,
  data: AccountData,
  identity: RequestIdentity,
): Record<string, JsonValue> {
  const entry = model.privateData !== null && typeof model.privateData === "object" &&
      !Array.isArray(model.privateData)
    ? model.privateData as Record<string, JsonValue>
    : {};
  const isVl = model.capabilities?.images === true || entry.is_vl === true;
  const isReasoning = entry.is_reasoning === true;
  const imageUrls = request.messages.flatMap((message) =>
    message.role === "user"
      ? message.content.flatMap((part) => part.type === "image" ? [imageUrl(part)] : [])
      : []
  );
  const prompt = latestPrompt(request.messages);
  const modelConfig: Record<string, JsonValue> = {
    key: model.id,
    display_name: model.displayName,
    format: "openai",
    is_vl: isVl,
    is_reasoning: isReasoning,
    source: "system",
  };
  const parameters: Record<string, JsonValue> = {};
  if (request.maxOutputTokens !== null) parameters.max_tokens = request.maxOutputTokens;
  if (request.reasoning.enabled && request.reasoning.effort !== null) {
    parameters.reasoning_effort = request.reasoning.effort;
  }
  const fastSwitches = request.latency === "fast"
    ? fastFeatureSwitches(model.privateData ?? null)
    : null;

  return {
    request_id: identity.requestId,
    request_set_id: identity.requestSetId,
    chat_record_id: identity.requestId,
    session_id: identity.sessionId,
    stream: true,
    chat_task: "FREE_INPUT",
    chat_context: {
      text: { type: "text", text: prompt },
      extra: {
        modelConfig: { key: model.id, is_reasoning: isReasoning, is_vl: isVl },
        originalContent: { type: "text", text: prompt },
      },
      ...(imageUrls.length > 0 ? { imageUrls } : {}),
    },
    source: 1,
    version: "3",
    aliyun_user_type: data.userType || DEFAULT_USER_TYPE,
    session_type: "qodercli",
    agent_id: "agent_common",
    task_id: "common",
    model_config: modelConfig,
    parameters,
    tools: request.tools.map((tool) => ({
      type: "function",
      function: { name: tool.name, description: tool.description, parameters: tool.parameters },
    })),
    business: {
      id: crypto.randomUUID(),
      name: prompt.slice(0, 30) || "chat",
      begin_at: Date.now(),
      product: "cli",
      type: "agent",
      stage: "start",
      ...(fastSwitches ? { feature_switches: fastSwitches } : {}),
    },
    ...(imageUrls.length > 0 ? { image_urls: imageUrls } : {}),
    messages: agentMessages(request.instructions, request.messages),
  };
}
