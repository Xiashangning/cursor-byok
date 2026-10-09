//! Provides isolated stores and canonical message fixtures for tests.
#![allow(dead_code)]

use std::sync::Arc;

use cursor_server::{
    cursor::prompting::{PromptAssets, PromptCompiler},
    cursor::TransportRegistry,
    model::{
        CanonicalMessage, ModelConfigInput, ModelType, Origin, Role, ToolCall, OPENAI_CHAT_ENDPOINT,
    },
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

/// A `ToolCall` fixture. `arguments` is stored both as JSON text and value;
/// `call_id` and `model_call_id` are stable identifiers for dispatch tests.
pub fn tool_call(
    call_id: &str,
    name: &str,
    arguments: serde_json::Value,
    model_call_id: &str,
) -> ToolCall {
    ToolCall {
        index: 0,
        call_id: call_id.into(),
        model_call_id: model_call_id.into(),
        name: name.into(),
        arguments_text: arguments.to_string(),
        arguments,
        argument_error: None,
    }
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
        default_context: None,
        default_effort: None,
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
        thinking_budget_tokens: None,
    }
}

pub fn official_model_not_found_payload() -> Vec<u8> {
    use base64::{engine::general_purpose::STANDARD as Base64, Engine};
    use cursor_server::cursor::protocol::proto::aiserver::v1 as ai;
    use prost::Message;

    let details = ai::ErrorDetails {
        error: ai::error_details::Error::BadModelName as i32,
        details: Some(ai::CustomErrorDetails {
            title: "AI Model Not Found".into(),
            detail: "Unknown model ID: cursor-byok-nonexistent-model-20261006-005e91c3d3b6".into(),
            is_retryable: Some(false),
            show_request_id: Some(false),
            ..Default::default()
        }),
        is_expected: Some(true),
    };
    serde_json::json!({
        "error": {
            "code": "not_found",
            "message": "Error",
            "details": [{
                "type": "aiserver.v1.ErrorDetails",
                "value": Base64.encode(details.encode_to_vec()),
            }],
        }
    })
    .to_string()
    .into_bytes()
}
