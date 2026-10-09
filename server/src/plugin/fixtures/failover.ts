import { defineProviderPlugin } from "cursor-byok:plugin";

let attempts = 0;
export default defineProviderPlugin({
  resources: [{ type: "account", displayName: "Account", present: () => ({ displayName: "Fixture", metrics: [] }) }],
  providers: [{
    id: "test", displayName: "Test", providerType: "test", resourceType: "account",
    invoke(input, output) {
      attempts++;
      if (input.model.id === "after-output") {
        output.emit({ type: "text-start" });
        output.emit({ type: "text-delta", text: `attempt:${attempts}` });
      }
      if (attempts === 1 || input.model.id === "after-output") {
        return Promise.resolve({ status: "resource-error", message: "fixture quota", patch: { state: { status: "invalid", message: "fixture quota" } } });
      }
      output.emit({ type: "text-start" });
      output.emit({ type: "text-delta", text: `attempt:${attempts}` });
      output.emit({ type: "text-end" });
      output.emit({ type: "done", reason: "stop" });
      return Promise.resolve({ status: "completed" });
    },
  }],
});
