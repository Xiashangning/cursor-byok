import { defineProviderPlugin } from "cursor-byok:plugin";

export default defineProviderPlugin({
  providers: [],
  resources: [{
    type: "account",
    displayName: "Account",
    present: () => ({ displayName: "Fixture", metrics: [] }),
    actions: [{
      id: "details",
      displayName: "Details",
      run: async (resource) => ({
        title: "Details",
        cards: [{ id: resource.id, title: "Card" }],
        patch: { privateData: { token: "rotated-fixture-token" }, state: { status: "ready" } },
      }),
    }, {
      id: "wait",
      displayName: "Wait",
      run: (_resource, _input, context) => new Promise((_resolve, reject) => {
        context.signal.addEventListener("abort", () => reject(new Error("cancelled")), { once: true });
      }),
    }],
  }],
});
