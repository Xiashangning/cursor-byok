//! Provides isolated stores and canonical message fixtures for tests.
#![allow(dead_code)]

use std::sync::Arc;

use cursor_server::{
    cursor::prompting::{PromptAssets, PromptCompiler},
    cursor::TransportRegistry,
    model::{CanonicalMessage, ModelConfigInput, ModelType, Origin, Role, OPENAI_CHAT_ENDPOINT},
    store::Store,
};

use super::fake_provider::FakeProvider;

pub async fn temp_store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    (directory, store)
}

/// Prompt assets loaded from `server/prompt/cursor`.
pub fn prompt_assets() -> PromptAssets {
    PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap()
}

/// A transport registry wired to `store` and `provider` with the real prompts.
pub fn registry(store: Store, provider: FakeProvider) -> TransportRegistry {
    TransportRegistry::new(
        store,
        Arc::new(provider),
        PromptCompiler::new(prompt_assets()),
    )
}

pub fn user(id: &str, text: &str) -> CanonicalMessage {
    CanonicalMessage::text(id, Role::User, Origin::User, text)
}

/// An OpenAI chat-completions model fixture; only `model_id` and the context
/// window vary across test sites.
pub fn openai_model_input(model_id: &str, context_window_tokens: Option<u64>) -> ModelConfigInput {
    ModelConfigInput {
        sort_order: 0,
        display_name: "Test Model".into(),
        group_name: None,
        model_type: ModelType::OpenAi,
        base_url: "https://example.com/v1/chat/completions".into(),
        use_full_url: true,
        api_key: "test-key".into(),
        tooltip_data: model_id.into(),
        model_id: model_id.into(),
        reasoning_effort: None,
        effort_options: Vec::new(),
        context_options: Vec::new(),
        openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
        openai_extra_params_enabled: false,
        openai_extra_params: serde_json::json!({}),
        custom_headers_enabled: false,
        custom_headers: serde_json::json!({}),
        anthropic_extra_params_enabled: false,
        anthropic_extra_params: serde_json::json!({}),
        context_window_tokens,
        max_completion_tokens: None,
        anthropic_max_tokens: None,
        anthropic_thinking_effort: None,
        thinking_budget_tokens: None,
    }
}
