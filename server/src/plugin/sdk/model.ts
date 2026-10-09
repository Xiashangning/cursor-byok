import type { JsonValue, LocalizedText, PluginContext } from "./plugin.ts";
import type { ResourceSnapshot } from "./resource.ts";

export type ModelCapabilities = {
  images?: boolean;
};

export type ModelDefinition = {
  id: string;
  displayName: string;
  description?: string;
  maxOutputTokens?: number;
  /** 该模型可选的 effort 档位;缺省时宿主使用内置默认值。 */
  effortOptions?: string[];
  /** 该模型可选的上下文窗口;缺省时宿主使用内置默认值。 */
  contextOptions?: string[];
  capabilities?: ModelCapabilities;
  /** 之后的调用原样传回;永远不会展示给用户。 */
  privateData?: JsonValue;
};

/** 宿主目录中持久化的一条模型。 */
export type ModelSnapshot = ModelDefinition;

export type ModelListInput = {
  /** 模型发现需要认证时为首个可用资源,否则为 null。 */
  resource: ResourceSnapshot | null;
};

/** 一条随额度一同动态刷新的模型备注,展示在 Cursor hover 的额度行下方。 */
export type ModelNote = {
  modelId: string;
  text: LocalizedText;
};

export type ModelSupport = {
  /** 列举成功后,宿主用返回值整体替换该 Provider 的模型目录。 */
  list(input: ModelListInput, context: PluginContext): Promise<ModelDefinition[]>;
  /**
   * 可选:按已同步的模型目录给出随额度 60s 刷新的模型备注(如限免、额度消耗倍率)。
   * 宿主在与额度摘要同一轮调用,把该 Provider 已持久化的模型快照(含 privateData)
   * 一并传入,插件据此派生备注,结果按模型 ID 缓存并拼进 hover。
   * 不实现则零开销;实现应当避免再次拉取上游目录。
   */
  notes?(
    input: { models: ModelSnapshot[] },
    context: PluginContext,
  ): Promise<ModelNote[]>;
};
