//! Classifies official Connect errors using verified structured signals.
//! 判定条件(`code=not_found` + `aiserver.v1.ErrorDetails.error=ERROR_BAD_MODEL_NAME`)
use base64::engine::general_purpose::STANDARD as Base64Standard;
use base64::Engine;
use prost::Message;

use crate::cursor::protocol::proto::aiserver::v1 as ai;

#[cfg(test)]
pub(crate) fn captured_model_not_found_payload() -> Vec<u8> {
    let details = ai::ErrorDetails {
        error: ai::error_details::Error::BadModelName as i32,
        details: Some(ai::CustomErrorDetails {
            title: CAPTURED_TITLE.into(),
            detail: CAPTURED_DETAIL.into(),
            is_retryable: Some(false),
            show_request_id: Some(false),
            ..Default::default()
        }),
        is_expected: Some(true),
    };
    serde_json::json!({
        "error": {
            "code": CAPTURED_CODE,
            "message": CAPTURED_MESSAGE,
            "details": [{
                "type": "aiserver.v1.ErrorDetails",
                "value": Base64Standard.encode(details.encode_to_vec()),
            }],
        }
    })
    .to_string()
    .into_bytes()
}

/// 捕获载荷中的结构化取值;测试直接断言这些常量。
#[cfg(test)]
pub(crate) const CAPTURED_CODE: &str = "not_found";
#[cfg(test)]
pub(crate) const CAPTURED_MESSAGE: &str = "Error";
#[cfg(test)]
pub(crate) const CAPTURED_TITLE: &str = "AI Model Not Found";
#[cfg(test)]
pub(crate) const CAPTURED_DETAIL: &str =
    "Unknown model ID: cursor-byok-nonexistent-model-20261006-005e91c3d3b6";

/// 从 Connect 终止帧 JSON 中提取的结构化错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamError {
    /// Connect 错误码(`unauthenticated` / `not_found` / ...)。
    pub code: Option<String>,
    pub message: String,
    /// `details` 中每个 `aiserver.v1.ErrorDetails` 的解码结果与原始类型名。
    pub details: Vec<ErrorDetailsEntry>,
}

/// 一条结构化错误详情。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorDetailsEntry {
    /// details[].type。
    pub type_name: String,
    /// 解码后的 `aiserver.v1.ErrorDetails`(仅类型匹配时存在)。
    pub decoded: Option<DecodedErrorDetails>,
}

/// `aiserver.v1.ErrorDetails` 的可比较投影。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedErrorDetails {
    pub error: i32,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub is_retryable: Option<bool>,
    pub is_expected: Option<bool>,
}

/// 解析 Connect 终止帧 JSON 并提取结构化错误;非 JSON 或无 error 返回 None。
pub fn extract_end_stream(payload: &[u8]) -> Option<UpstreamError> {
    #[derive(serde::Deserialize)]
    struct WireEndStream {
        #[serde(default)]
        error: Option<WireError>,
    }
    #[derive(serde::Deserialize)]
    struct WireError {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        details: Vec<WireDetail>,
    }
    #[derive(serde::Deserialize)]
    struct WireDetail {
        #[serde(default, rename = "type")]
        type_name: Option<String>,
        #[serde(default)]
        value: Option<String>,
    }

    let parsed: WireEndStream = serde_json::from_slice(payload).ok()?;
    let error = parsed.error?;
    let mut details = Vec::new();
    for detail in &error.details {
        let type_name = detail.type_name.clone().unwrap_or_default();
        let decoded = if type_name == "aiserver.v1.ErrorDetails" {
            detail.value.as_deref().and_then(|value| {
                Base64Standard
                    .decode(value)
                    .ok()
                    .and_then(|bytes| ai::ErrorDetails::decode(bytes.as_slice()).ok())
                    .map(|details| DecodedErrorDetails {
                        error: details.error,
                        title: details.details.as_ref().map(|custom| custom.title.clone()),
                        detail: details.details.as_ref().map(|custom| custom.detail.clone()),
                        is_retryable: details.details.as_ref().and_then(|c| c.is_retryable),
                        is_expected: details.is_expected,
                    })
            })
        } else {
            None
        };
        details.push(ErrorDetailsEntry { type_name, decoded });
    }
    Some(UpstreamError {
        code: error.code,
        message: error.message.unwrap_or_default(),
        details,
    })
}

/// HTTP Connect errors use the error object directly, unlike streaming trailers.
pub fn extract_http(payload: &[u8]) -> Option<UpstreamError> {
    if let Some(error) = extract_end_stream(payload) {
        return Some(error);
    }
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    value.get("code")?.as_str()?;
    extract_end_stream(&serde_json::to_vec(&serde_json::json!({"error": value})).ok()?)
}

/// 使用真实账号实测的 Connect code + protobuf 枚举组合分类。
/// 文本仅供诊断，不用宽泛子串匹配覆盖鉴权、额度或其他错误。
pub fn is_model_not_found(error: &UpstreamError) -> bool {
    error.code.as_deref() == Some("not_found")
        && error.details.iter().any(|entry| {
            entry.decoded.as_ref().is_some_and(|decoded| {
                decoded.error == ai::error_details::Error::BadModelName as i32
            })
        })
}

pub fn model_not_found_task_error(model: &str) -> String {
    format!(
        "{model} 模型未找到 / Model {model} not found。\
请调整 model 参数,改用可用模型 ID 后重新发起 Task。\
Ask the parent agent to retry with an available model ID."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn end_stream(code: &str, details: Vec<(String, String)>) -> Vec<u8> {
        serde_json::json!({
            "error": {
                "code": code,
                "message": "msg",
                "details": details
                    .into_iter()
                    .map(|(type_name, value)| serde_json::json!({
                        "type": type_name,
                        "value": value,
                    }))
                    .collect::<Vec<_>>(),
            }
        })
        .to_string()
        .into_bytes()
    }

    fn error_details_value(error: i32, title: &str, detail: &str) -> String {
        let details = ai::ErrorDetails {
            error,
            details: Some(ai::CustomErrorDetails {
                title: title.into(),
                detail: detail.into(),
                is_retryable: Some(false),
                ..Default::default()
            }),
            is_expected: Some(true),
        };
        Base64Standard.encode(details.encode_to_vec())
    }

    #[test]
    fn authentication_error_is_extracted_and_not_model_not_found() {
        let payload = end_stream(
            "unauthenticated",
            vec![(
                "aiserver.v1.ErrorDetails".into(),
                error_details_value(
                    2,
                    "Authentication error",
                    "If you are logged in, try logging out and back in.",
                ),
            )],
        );
        let error = extract_end_stream(&payload).unwrap();
        assert_eq!(error.code.as_deref(), Some("unauthenticated"));
        assert_eq!(error.message, "msg");
        assert_eq!(error.details.len(), 1);
        let decoded = error.details[0].decoded.as_ref().unwrap();
        assert_eq!(decoded.error, 2);
        assert_eq!(decoded.title.as_deref(), Some("Authentication error"));
        assert!(!is_model_not_found(&error));
    }

    #[test]
    fn unknown_detail_types_are_kept_without_decoding() {
        let payload = end_stream(
            "not_found",
            vec![("other.v1.Something".into(), "AAAA".into())],
        );
        let error = extract_end_stream(&payload).unwrap();
        assert_eq!(error.details.len(), 1);
        assert!(error.details[0].decoded.is_none());
        // Connect `not_found` 单独不构成模型不存在证据。
        assert!(!is_model_not_found(&error));
    }

    #[test]
    fn non_json_payloads_and_missing_errors_are_ignored() {
        assert!(extract_end_stream(b"not json").is_none());
        assert!(extract_end_stream(b"{}").is_none());
        assert!(extract_end_stream(br#"{"error":null}"#).is_none());
    }

    fn captured_model_not_found() -> UpstreamError {
        extract_end_stream(&captured_model_not_found_payload()).unwrap()
    }

    #[test]
    fn real_account_capture_identifies_bad_model_name() {
        let error = captured_model_not_found();
        assert_eq!(error.code.as_deref(), Some(CAPTURED_CODE));
        assert_eq!(error.message, CAPTURED_MESSAGE);
        let details = error.details[0].decoded.as_ref().unwrap();
        assert_eq!(details.error, ai::error_details::Error::BadModelName as i32);
        assert_eq!(details.title.as_deref(), Some(CAPTURED_TITLE));
        assert_eq!(details.detail.as_deref(), Some(CAPTURED_DETAIL));
        assert_eq!(details.is_retryable, Some(false));
        assert_eq!(details.is_expected, Some(true));
        assert!(is_model_not_found(&error));
    }

    #[test]
    fn model_not_found_requires_verified_structured_combination() {
        let original = captured_model_not_found();
        for code in [
            "unauthenticated",
            "permission_denied",
            "resource_exhausted",
            "unavailable",
        ] {
            let mut error = original.clone();
            error.code = Some(code.into());
            assert!(!is_model_not_found(&error));
        }
        // 即使文案相同，其他结构化错误也不应被重新分类。
        for value in [0, 1, 2, 4, 7, 8, 9, 10, 39, 41] {
            let mut error = original.clone();
            error.details[0].decoded.as_mut().unwrap().error = value;
            assert!(!is_model_not_found(&error));
        }
        let mut error = original;
        error.details.clear();
        error.message = "AI Model Not Found: Unknown model ID: example".into();
        assert!(!is_model_not_found(&error));
    }

    #[test]
    fn connect_error_decoding_is_independent_of_http_chunk_boundaries() {
        use crate::cursor::protocol::connect::{FrameDecoder, END_STREAM_FLAG};
        let payload = captured_model_not_found_payload();
        let mut wire = vec![0, 0, 0, 0, 3, 1, 2, 3];
        wire.push(END_STREAM_FLAG);
        wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        wire.extend_from_slice(&payload);
        for split in 0..=wire.len() {
            let mut decoder = FrameDecoder::default();
            let mut frames = decoder.push(&wire[..split]).unwrap();
            frames.extend(decoder.push(&wire[split..]).unwrap());
            decoder.finish().unwrap();
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0].1.as_ref(), &[1, 2, 3]);
            assert!(is_model_not_found(
                &extract_end_stream(&frames[1].1).unwrap()
            ));
        }
        let mut decoder = FrameDecoder::default();
        let mut frames = Vec::new();
        for byte in &wire {
            frames.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(frames.len(), 2);
        decoder.finish().unwrap();
        let mut decoder = FrameDecoder::default();
        decoder.push(&wire[..wire.len() - 1]).unwrap();
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn http_errors_and_stream_errors_have_the_same_classification() {
        let payload = captured_model_not_found_payload();
        let fixture: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let http = serde_json::to_vec(&fixture["error"]).unwrap();
        assert_eq!(extract_http(&http).unwrap(), captured_model_not_found());
        assert!(extract_http(b"<html>502 Bad Gateway</html>").is_none());
        assert!(!is_model_not_found(
            &extract_http(br#"{"code":"resource_exhausted","message":"quota exceeded"}"#).unwrap()
        ));
    }

    #[test]
    fn task_error_text_carries_both_languages_and_the_retry_hint() {
        let text = model_not_found_task_error("abcd1234");
        assert!(text.contains("abcd1234 模型未找到"));
        assert!(text.contains("Model abcd1234 not found"));
        assert!(text.contains("可用模型 ID"));
    }
}
