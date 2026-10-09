import { defineProviderPlugin } from "cursor-byok:plugin";
import { qoderDeviceOAuth } from "./oauth.ts";
import { qoderProvider } from "./provider.ts";
import { presentAccount, refreshAccount, removeAccount, RESOURCE_TYPE } from "./resources.ts";

export default defineProviderPlugin({
  providers: [qoderProvider],
  resources: [{
    type: RESOURCE_TYPE,
    displayName: { "en-US": "Qoder accounts", "zh-CN": "Qoder 账号" },
    add: [qoderDeviceOAuth],
    present: presentAccount,
    refresh: refreshAccount,
    remove: removeAccount,
  }],
});
