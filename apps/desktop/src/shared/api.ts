import { invoke } from "@tauri-apps/api/core";

export type ModelType = "openai" | "anthropic";

export interface Model {
  model_hash: string;
  sort_order: number;
  display_name: string;
  group_name: string | null;
  type: ModelType;
  base_url: string;
  use_full_url: boolean;
  api_key: string;
  tooltip_data: string;
  model_id: string;
  reasoning_effort: string | null;
  effort_options: string[];
  context_options: string[];
  openai_endpoint: string;
  openai_extra_params_enabled: boolean;
  openai_extra_params: Record<string, unknown>;
  custom_headers_enabled: boolean;
  custom_headers: Record<string, string>;
  anthropic_extra_params_enabled: boolean;
  anthropic_extra_params: Record<string, unknown>;
  context_window_tokens: number | null;
  max_completion_tokens: number | null;
  anthropic_max_tokens: number | null;
  anthropic_thinking_effort: string | null;
  thinking_budget_tokens: number | null;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface ModelInput {
  sort_order: number;
  display_name: string;
  group_name: string | null;
  type: ModelType;
  base_url: string;
  use_full_url: boolean;
  api_key: string;
  tooltip_data: string;
  model_id: string;
  reasoning_effort: string | null;
  effort_options: string[];
  context_options: string[];
  openai_endpoint: string;
  openai_extra_params_enabled: boolean;
  openai_extra_params: Record<string, unknown>;
  custom_headers_enabled: boolean;
  custom_headers: Record<string, string>;
  anthropic_extra_params_enabled: boolean;
  anthropic_extra_params: Record<string, unknown>;
  context_window_tokens: number | null;
  max_completion_tokens: number | null;
  anthropic_max_tokens: number | null;
  anthropic_thinking_effort: string | null;
  thinking_budget_tokens: number | null;
}

export interface ModelDiscoveryInput {
  type: ModelType;
  base_url: string;
  api_key: string;
  custom_headers_enabled: boolean;
  custom_headers: Record<string, string>;
}

export interface LegacyModelImportPreviewItem {
  model_hash: string;
  display_name: string;
  model_id: string;
  type: ModelType;
  existing: boolean;
}

export interface LegacyModelImportPreview {
  source: string;
  total: number;
  new_models: number;
  existing_models: number;
  models: LegacyModelImportPreviewItem[];
}

export interface LegacyModelImportResult {
  imported: number;
  skipped: number;
  total: number;
}

export interface ModelConnectivityResult {
  duration_ms: number;
  first_valid_response_ms: number | null;
  output_tokens: number;
  tokens_per_second: number;
  tokens_estimated: boolean;
  output: string;
}

export type CaState = "missing" | "untrusted" | "ready" | "invalid";
export type IntegrationState = "disabled" | "enabled" | "degraded";
export interface CursorHarnessStatus {
  platform: string;
  ca: CaState;
  configured_models: number;
  enabled_models: number;
  integration: IntegrationState;
  proxy_url: string | null;
  ca_install_command: string | null;
}

export interface PortSettings {
  proxy_port: number;
  service_port: number;
}

export interface StatisticsStorage {
  bytes: number;
  call_count: number;
  trace_count: number;
}

export type StatisticsStorageScope = "details" | "all";

export type ProxyMode = "default" | "custom";

export interface ProxySettings {
  mode: ProxyMode;
  address: string;
  auth_enabled: boolean;
  username: string;
  has_password: boolean;
}

export interface ProxySettingsInput {
  mode: ProxyMode;
  address: string;
  auth_enabled: boolean;
  username: string;
  password?: string;
}

export type TabMode = "public" | "direct" | "custom";

export interface TabSettings {
  mode: TabMode;
  address: string;
}

export interface DesktopSettings {
  silent_start: boolean;
  show_dock_icon: boolean;
}

export interface CommitSettings {
  model_id: string;
  prompt: string;
}

export interface CommitSettingsView extends CommitSettings {
  default_prompt: string;
}

export type PluginRuntimeState = "uninitialized" | "initializing" | "ready" | "failed" | "unsupported";
export type PluginRuntimePhase = "checking" | "downloading" | "verifying" | "installing" | "validating";

export interface PluginRuntimeStatus {
  state: PluginRuntimeState;
  version: string;
  target: string | null;
  phase: PluginRuntimePhase | null;
  downloaded_bytes: number;
  total_bytes: number | null;
  error: string | null;
}

/** 插件提供的显示文本:纯字符串或 locale → 文本映射。 */
export type PluginLocalizedText = string | Record<string, string>;

export function pluginText(value: PluginLocalizedText | null | undefined, locale: string): string {
  if (!value) return "";
  if (typeof value === "string") return value;
  if (value[locale]) return value[locale];
  const language = locale.split("-")[0].toLowerCase();
  for (const [key, text] of Object.entries(value)) {
    const normalized = key.toLowerCase();
    if (normalized === language || normalized.startsWith(`${language}-`)) return text;
  }
  return value["en-US"] ?? value["en"] ?? Object.values(value)[0] ?? "";
}

export interface PluginResourceState {
  status: "ready" | "cooling" | "invalid";
  retryAtMs?: number | null;
  message?: string | null;
}

export interface PluginResourceMetric {
  id: string;
  label: PluginLocalizedText;
  unit: "percent" | "count";
  value: number;
  resetAtMs?: number | null;
}

export interface PluginResourceView {
  id: string;
  state: PluginResourceState;
  displayName: string;
  description: PluginLocalizedText | null;
  metrics: PluginResourceMetric[];
  createdAtMs: number;
}

export interface PluginAddMethod {
  type: "oauth2.0";
  id: string;
  displayName: PluginLocalizedText;
  description: PluginLocalizedText | null;
}

export interface PluginImportDescriptor {
  displayName: PluginLocalizedText;
  description: PluginLocalizedText | null;
  accept: string[];
  multiple: boolean;
}

export interface PluginResourceDescriptor {
  type: string;
  displayName: PluginLocalizedText;
  add: PluginAddMethod[];
  import: PluginImportDescriptor | null;
  canRefresh: boolean;
  canRemove: boolean;
  resources: PluginResourceView[];
}

export interface PluginModelDescriptor {
  id: string;
  pluginId: string;
  pluginName: string;
  providerId: string;
  modelId: string;
  displayName: string;
  description: string | null;
  icon: string;
  providerType: string;
  maxOutputTokens: number | null;
  images: boolean;
}

export interface PluginProviderDescriptor {
  id: string;
  pluginId: string;
  displayName: PluginLocalizedText;
  description: PluginLocalizedText | null;
  providerType: string;
  resourceType: string | null;
  hasModels: boolean;
  configured: boolean;
  models: PluginModelDescriptor[];
}

export interface PluginDescriptor {
  id: string;
  name: string;
  version: string;
  author: string | null;
  icon: string;
  providers: PluginProviderDescriptor[];
  resources: PluginResourceDescriptor[];
}

export interface PluginOAuthBegin {
  sessionId: string;
  userCode: string;
  verificationUrl: string;
  verificationUrlComplete: string | null;
  expiresAtMs: number;
  pollIntervalMs: number;
}

export type PluginOAuthPoll =
  | { status: "pending"; pollIntervalMs: number }
  | { status: "completed"; added: number; updated: number; modelSyncError: string | null }
  | { status: "denied"; message: string | null }
  | { status: "failed"; message: string };

export interface PluginImportFile {
  name: string;
  content: string;
}

export interface PluginImportResult {
  added: number;
  updated: number;
  warnings: string[];
  modelSyncError: string | null;
}

export type ConfiguredModel =
  | { kind: "builtin"; id: string; name: string; builtin: Model }
  | { kind: "plugin"; id: string; name: string; plugin: PluginModelDescriptor };

let cachedDisabledPluginModelIds: Set<string> = new Set();
let cachedDisabledPluginAccountIds: Set<string> = new Set();
let modelsLoaded = false;
let accountsLoaded = false;

export function getDisabledPluginModelIds(): Set<string> {
  if (!modelsLoaded) {
    modelsLoaded = true;
    void api.disabledPluginModels().then((ids) => {
      cachedDisabledPluginModelIds = new Set(ids);
      window.dispatchEvent(new CustomEvent("cursor_plugin_models_changed"));
    }).catch(() => {});
  }
  return new Set(cachedDisabledPluginModelIds);
}

export function setPluginModelEnabled(modelId: string, enabled: boolean): void {
  if (enabled) {
    cachedDisabledPluginModelIds.delete(modelId);
  } else {
    cachedDisabledPluginModelIds.add(modelId);
  }
  window.dispatchEvent(new CustomEvent("cursor_plugin_models_changed"));
  void api.setDisabledPluginModels([...cachedDisabledPluginModelIds]).catch(() => {});
}

export function setMultiplePluginModelsEnabled(modelIds: string[], enabled: boolean): void {
  if (enabled) {
    for (const id of modelIds) {
      cachedDisabledPluginModelIds.delete(id);
    }
  } else {
    for (const id of modelIds) {
      cachedDisabledPluginModelIds.add(id);
    }
  }
  window.dispatchEvent(new CustomEvent("cursor_plugin_models_changed"));
  void api.setDisabledPluginModels([...cachedDisabledPluginModelIds]).catch(() => {});
}

export function getDisabledPluginAccountIds(): Set<string> {
  if (!accountsLoaded) {
    accountsLoaded = true;
    void api.disabledPluginAccounts().then((ids) => {
      cachedDisabledPluginAccountIds = new Set(ids);
      window.dispatchEvent(new CustomEvent("cursor_plugin_accounts_changed"));
    }).catch(() => {});
  }
  return new Set(cachedDisabledPluginAccountIds);
}

export function setPluginAccountEnabled(accountId: string, enabled: boolean): void {
  if (enabled) {
    cachedDisabledPluginAccountIds.delete(accountId);
  } else {
    cachedDisabledPluginAccountIds.add(accountId);
  }
  window.dispatchEvent(new CustomEvent("cursor_plugin_accounts_changed"));
  void api.setDisabledPluginAccounts([...cachedDisabledPluginAccountIds]).catch(() => {});
}

export function updateCachedDisabledStates(models: string[], accounts: string[]): void {
  cachedDisabledPluginModelIds = new Set(models);
  cachedDisabledPluginAccountIds = new Set(accounts);
  modelsLoaded = true;
  accountsLoaded = true;
  window.dispatchEvent(new CustomEvent("cursor_plugin_models_changed"));
  window.dispatchEvent(new CustomEvent("cursor_plugin_accounts_changed"));
}

export function configuredPluginModels(plugins: PluginDescriptor[]): PluginModelDescriptor[] {
  const disabled = getDisabledPluginModelIds();
  return plugins.flatMap((plugin) =>
    plugin.providers.flatMap((provider) =>
      provider.configured ? provider.models.filter((m) => !disabled.has(m.id)) : []
    )
  );
}

export function configuredModels(models: Model[], plugins: PluginDescriptor[]): ConfiguredModel[] {
  return [
    ...models.map((model): ConfiguredModel => ({ kind: "builtin", id: model.model_hash, name: model.display_name, builtin: model })),
    ...configuredPluginModels(plugins).map((model): ConfiguredModel => ({ kind: "plugin", id: model.id, name: model.displayName, plugin: model })),
  ];
}

export interface OverviewMetrics {
  llm_calls: number;
  successful_calls: number;
  failed_calls: number;
  token_usage: number;
  prompt_tokens: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
}

export type TokenUsageGranularity = "minute" | "hour" | "day";

export interface OverviewTokenUsageBucket {
  bucket_start_ms: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
}

export interface Overview {
  metrics: OverviewMetrics;
  token_usage_granularity: TokenUsageGranularity;
  token_usage_series: OverviewTokenUsageBucket[];
}

export interface LlmCall {
  call_kind: "provider_llm" | "cursor_official";
  route: "local_byok" | "cursor_official";
  call_id: string;
  run_id: string;
  conversation_id: string;
  provider_call_index: number;
  model_hash: string | null;
  provider_type: string;
  provider_url: string;
  request_type: string;
  request_url: string;
  model_id: string;
  display_name: string;
  reasoning_effort: string | null;
  fast: boolean | null;
  status: string;
  finish_reason: string | null;
  created_at_ms: number;
  ttfb_ms: number | null;
  ttft_ms: number | null;
  ttfr_ms: number | null;
  duration_ms: number | null;
  input_tokens: number | null;
  output_tokens: number | null;
  total_tokens: number | null;
  cache_read_tokens: number | null;
  cache_write_tokens: number | null;
  reasoning_tokens: number | null;
  message_count: number;
  tool_count: number;
  http_status: number | null;
  error_kind: string | null;
  error_message: string | null;
  detailed: boolean;
}

export interface CallDetail {
  call: LlmCall;
  request: { headers: unknown; body: unknown; byte_count: number } | null;
  response_chunks: Array<{ seq: number; received_offset_ms: number; data: string; byte_count: number }>;
  cursor_trace: {
    trace: {
      request_id: string;
      conversation_id: string | null;
      route: "local_byok" | "cursor_official";
      model_id: string | null;
      status: string;
      request_bytes: number;
      response_bytes: number;
      response_event_count: number;
      http_status: number | null;
      received_at_ms: number;
      first_response_at_ms: number | null;
      finished_at_ms: number | null;
      error_message: string | null;
    };
    artifacts: Array<{
      seq: number;
      artifact_type: string;
      source: string;
      metadata: unknown;
      created_at_ms: number;
      byte_count: number;
      encoding: "utf8" | "base64";
      data: string;
    }>;
  } | null;
}

const packagedDesktop = "__TAURI_INTERNALS__" in window
  || window.location.protocol === "tauri:"
  || window.location.hostname === "tauri.localhost";
const API_ROOT = "/__byok-api__/api";

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(`${API_ROOT}${path}`, {
      ...init,
      headers: init?.body ? { "content-type": "application/json", ...init.headers } : init?.headers,
    });
  } catch (cause) {
    throw new Error(t("无法连接本地管理服务"), { cause });
  }
  if (!response.ok) {
    const body = await response.text();
    let message = body;
    try {
      const parsed = JSON.parse(body) as { message?: unknown };
      if (typeof parsed.message === "string") message = parsed.message;
    } catch {
      // Plain-text errors are already suitable for display.
    }
    throw new Error(message || `${response.status} ${response.statusText}`);
  }
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}

export const api = {
  models: () => request<Model[]>("/models"),
  createModels: (models: ModelInput[]) => request<Model[]>("/models", { method: "POST", body: JSON.stringify({ models }) }),
  reorderModels: (modelHashes: string[]) => request<Model[]>("/models/order", { method: "PUT", body: JSON.stringify({ model_hashes: modelHashes }) }),
  discoverModels: (input: ModelDiscoveryInput) => request<{ models: string[] }>("/models/discover", { method: "POST", body: JSON.stringify(input) }),
  previewV0049Models: () => request<LegacyModelImportPreview>("/models/import-v0049"),
  importV0049Models: () => request<LegacyModelImportResult>("/models/import-v0049", { method: "POST" }),
  updateModel: (hash: string, model: ModelInput) => request<Model>(`/models/${hash}`, { method: "PUT", body: JSON.stringify(model) }),
  deleteModel: (hash: string) => request<void>(`/models/${hash}`, { method: "DELETE" }),
  testModel: (hash: string, testId: string, signal?: AbortSignal) => request<ModelConnectivityResult>(`/models/${encodeURIComponent(hash)}/test/${encodeURIComponent(testId)}`, { method: "POST", signal }),
  cancelModelTest: (hash: string, testId: string) => request<void>(`/models/${encodeURIComponent(hash)}/test/${encodeURIComponent(testId)}`, { method: "DELETE" }),
  overview: (filter?: { startMs: number; endMs: number; modelHashes?: string[]; bucketMs?: number }) => {
    const params = new URLSearchParams();
    if (filter) {
      params.set("start_ms", String(filter.startMs));
      params.set("end_ms", String(filter.endMs));
      if (filter.modelHashes?.length) params.set("model_hashes", JSON.stringify(filter.modelHashes));
      if (filter.bucketMs) params.set("bucket_ms", String(filter.bucketMs));
    }
    const query = params.toString();
    return request<Overview>(`/overview${query ? `?${query}` : ""}`);
  },
  cursorHarness: () => request<CursorHarnessStatus>("/harness/cursor/status"),
  initializeCursorCa: () => request<CursorHarnessStatus>("/harness/cursor/ca/initialize", { method: "POST" }),
  plugins: () => request<PluginDescriptor[]>("/plugins"),
  disabledPluginModels: () => request<string[]>("/plugins/disabled-models"),
  setDisabledPluginModels: (modelIds: string[]) => request<string[]>("/plugins/disabled-models", { method: "PUT", body: JSON.stringify({ modelIds }) }),
  disabledPluginAccounts: () => request<string[]>("/plugins/disabled-accounts"),
  setDisabledPluginAccounts: (accountIds: string[]) => request<string[]>("/plugins/disabled-accounts", { method: "PUT", body: JSON.stringify({ accountIds }) }),
  pluginOAuthBegin: (pluginId: string, resourceType: string, methodId: string) => request<PluginOAuthBegin>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/add/${encodeURIComponent(methodId)}/begin`, { method: "POST" }),
  pluginOAuthPoll: (sessionId: string, signal?: AbortSignal) => request<PluginOAuthPoll>(`/plugins/oauth/${encodeURIComponent(sessionId)}/poll`, { method: "POST", signal }),
  importPluginResources: (pluginId: string, resourceType: string, files: PluginImportFile[]) => request<PluginImportResult>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/import`, { method: "POST", body: JSON.stringify(files) }),
  refreshPluginResource: (pluginId: string, resourceType: string, resourceId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}/refresh`, { method: "POST" }),
  deletePluginResource: (pluginId: string, resourceType: string, resourceId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/${encodeURIComponent(resourceId)}`, { method: "DELETE" }),
  syncPluginModels: (pluginId: string, providerId: string) => request<{ models: number }>(`/plugins/${encodeURIComponent(pluginId)}/providers/${encodeURIComponent(providerId)}/models/sync`, { method: "POST" }),
  pluginResourceExportUrl: (servicePort: number, pluginId: string, resourceType: string) => `http://127.0.0.1:${servicePort}${API_ROOT}/plugins/${encodeURIComponent(pluginId)}/resources/${encodeURIComponent(resourceType)}/export`,
  removePluginConfiguration: (pluginId: string) => request<void>(`/plugins/${encodeURIComponent(pluginId)}`, { method: "DELETE" }),
  pluginRuntime: () => request<PluginRuntimeStatus>("/plugins/runtime"),
  initializePluginRuntime: () => request<PluginRuntimeStatus>("/plugins/runtime", { method: "POST" }),
  cancelPluginRuntimeInitialization: () => request<PluginRuntimeStatus>("/plugins/runtime", { method: "DELETE" }),
  openCursorCaInstallTerminal: async (command: string) => {
    if (!packagedDesktop) throw new Error(t("请在桌面应用中打开终端安装 CA"));
    await invoke("open_terminal_with_command", { command });
  },
  copyCursorText: async (text: string) => {
    if (!packagedDesktop) throw new Error(t("请在桌面应用中复制到系统剪贴板"));
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(text);
  },
  setCursorEnabled: (enabled: boolean) => request<CursorHarnessStatus>("/harness/cursor/enabled", { method: "PUT", body: JSON.stringify({ enabled }) }),
  calls: () => request<LlmCall[]>("/llm-calls?limit=200"),
  call: (id: string) => request<CallDetail>(`/llm-calls/${encodeURIComponent(id)}`),
  openCallDetails: async (id: string) => {
    const url = new URL(window.location.href);
    url.hash = `/calls/${encodeURIComponent(id)}`;
    await request<void>("/desktop/open-external-url", { method: "POST", body: JSON.stringify({ url: url.toString() }) });
  },
  openExternalUrl: (url: string) => request<void>("/desktop/open-external-url", { method: "POST", body: JSON.stringify({ url }) }),
  observability: () => request<{ detailed: boolean }>("/settings/observability"),
  setObservability: (detailed: boolean) => request<{ detailed: boolean }>("/settings/observability", { method: "PUT", body: JSON.stringify({ detailed }) }),
  ports: () => request<PortSettings>("/settings/ports"),
  setPorts: (settings: PortSettings) => request<PortSettings>("/settings/ports", { method: "PUT", body: JSON.stringify(settings) }),
  statisticsStorage: () => request<StatisticsStorage>("/settings/storage/statistics"),
  clearStatisticsStorage: (scope: StatisticsStorageScope) => request<StatisticsStorage>("/settings/storage/statistics", { method: "DELETE", body: JSON.stringify({ scope }) }),
  proxySettings: () => request<ProxySettings>("/settings/proxy"),
  setProxySettings: (settings: ProxySettingsInput) => request<ProxySettings>("/settings/proxy", { method: "PUT", body: JSON.stringify(settings) }),
  tabSettings: () => request<TabSettings>("/settings/tab"),
  setTabSettings: (settings: TabSettings) => request<TabSettings>("/settings/tab", { method: "PUT", body: JSON.stringify(settings) }),
  desktopSettings: () => request<DesktopSettings>("/settings/desktop"),
  setDesktopSettings: (settings: DesktopSettings) => request<DesktopSettings>("/settings/desktop", { method: "PUT", body: JSON.stringify(settings) }),
  commitSettings: () => request<CommitSettingsView>("/settings/commit"),
  setCommitSettings: (settings: CommitSettings) => request<CommitSettingsView>("/settings/commit", { method: "PUT", body: JSON.stringify(settings) }),
};
