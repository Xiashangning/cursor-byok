import { defineProviderPlugin } from "cursor-byok:plugin";
import { kimiDeviceOAuth } from "./oauth.ts";
import { kimiProvider } from "./provider.ts";
import { credentialImport, presentAccount, refreshAccount, RESOURCE_TYPE } from "./resources.ts";

export default defineProviderPlugin({
  providers: [kimiProvider],
  resources: [{
    type: RESOURCE_TYPE,
    displayName: { "en-US": "Kimi accounts", "zh-CN": "Kimi 账号" },
    add: [kimiDeviceOAuth],
    import: credentialImport,
    present: presentAccount,
    refresh: refreshAccount,
  }],
});
