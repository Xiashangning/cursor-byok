import type { JsonValue, PluginContext } from "./plugin.ts";
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

export type ModelSupport = {
  /** 列举成功后,宿主用返回值整体替换该 Provider 的模型目录。 */
  list(input: ModelListInput, context: PluginContext): Promise<ModelDefinition[]>;
};
