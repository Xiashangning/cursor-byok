//! Eight-character model IDs; display settings do not participate in identity.
use sha2::{Digest, Sha256};

use crate::Result;

use super::configuration::{
    normalize_openai_endpoint, resolve_request_url, ModelConfigInput, ModelType,
    OPENAI_RESPONSES_ENDPOINT,
};

fn short_id(preimage: &str) -> String {
    let digest = Sha256::digest(preimage.as_bytes());
    hex::encode(&digest[..4])
}

/// Newline-separated protocol, request URL, upstream model, and API key.
pub fn builtin_model_id(input: &ModelConfigInput) -> Result<String> {
    let request_url = resolve_request_url(
        input.model_type,
        &input.base_url,
        &input.openai_endpoint,
        input.use_full_url,
    )?;
    let protocol = match input.model_type {
        ModelType::Anthropic => "anthropic",
        ModelType::OpenAi
            if normalize_openai_endpoint(&input.openai_endpoint)? == OPENAI_RESPONSES_ENDPOINT =>
        {
            "openai-responses"
        }
        ModelType::OpenAi => "openai-chat",
    };
    Ok(short_id(&format!(
        "{protocol}\n{request_url}\n{}\n{}",
        input.model_id, input.api_key
    )))
}

/// Newline-separated stable plugin ID and upstream model; provider is excluded.
pub fn plugin_model_id(plugin_id: &str, upstream_model_id: &str) -> String {
    short_id(&format!("{plugin_id}\n{upstream_model_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILTIN_VECTOR_1_EXPECTED: &str = "d594df65";
    const BUILTIN_VECTOR_2_EXPECTED: &str = "339e95a5";
    const PLUGIN_VECTOR_1_EXPECTED: &str = "07929241";
    const PLUGIN_VECTOR_2_EXPECTED: &str = "b9558652";
    const PLUGIN_VECTOR_3_EXPECTED: &str = "7b3a6f3a";

    fn input() -> ModelConfigInput {
        serde_json::from_value(serde_json::json!({
            "type": "openai", "display_name": "Display", "tooltip_data": "Tooltip",
            "base_url": "https://api.example.com", "model_id": "provider-model-a",
            "api_key": "secret-key-1"
        }))
        .unwrap()
    }

    #[test]
    fn fixed_identity_vectors() {
        for (key, expected) in [
            ("secret-key-1", BUILTIN_VECTOR_1_EXPECTED),
            ("secret-key-2", BUILTIN_VECTOR_2_EXPECTED),
        ] {
            let mut model = input();
            model.api_key = key.into();
            assert_eq!(builtin_model_id(&model).unwrap(), expected);
        }
        for (plugin, model, expected) in [
            ("dev.example", "org/gpt-5", PLUGIN_VECTOR_1_EXPECTED),
            ("dev.example", "claude-4-opus", PLUGIN_VECTOR_2_EXPECTED),
            ("other.plugin", "org/gpt-5", PLUGIN_VECTOR_3_EXPECTED),
        ] {
            assert_eq!(plugin_model_id(plugin, model), expected);
        }
    }

    #[test]
    fn identity_tracks_configuration_not_display_settings() {
        let model = input();
        let id = builtin_model_id(&model).unwrap();
        let mut renamed = model.clone();
        renamed.display_name = "Renamed".into();
        renamed.tooltip_data.clear();
        renamed.context_window_tokens = Some(1_000_000);
        assert_eq!(builtin_model_id(&renamed).unwrap(), id);
        for (url, upstream, key) in [
            (
                "https://other.example.com",
                "provider-model-a",
                "secret-key-1",
            ),
            ("https://api.example.com", "other-model", "secret-key-1"),
            (
                "https://api.example.com",
                "provider-model-a",
                "secret-key-2",
            ),
        ] {
            let mut changed = model.clone();
            changed.base_url = url.into();
            changed.model_id = upstream.into();
            changed.api_key = key.into();
            assert_ne!(builtin_model_id(&changed).unwrap(), id);
        }
        let mut invalid = model;
        invalid.base_url = "not a url".into();
        assert!(builtin_model_id(&invalid).is_err());
    }

    #[test]
    fn full_url_does_not_hide_request_protocol() {
        let mut model = input();
        model.use_full_url = true;
        let responses = builtin_model_id(&model).unwrap();
        model.openai_endpoint = crate::model::OPENAI_CHAT_ENDPOINT.into();
        let chat = builtin_model_id(&model).unwrap();
        model.model_type = ModelType::Anthropic;
        let anthropic = builtin_model_id(&model).unwrap();
        assert_ne!(responses, chat);
        assert_ne!(responses, anthropic);
        assert_ne!(chat, anthropic);
    }
}
