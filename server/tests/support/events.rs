//! Provider `ModelEvent` stream fixtures.

#![allow(dead_code)]

use cursor_server::{
    model::Usage,
    provider::{FinishReason, ModelEvent},
};

/// A plain text response that ends the run. No `Usage` event.
pub fn text_response(model_call_id: &str, text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: model_call_id.into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta(text.into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]
}

/// A text response carrying token usage; `total_tokens` is `input + output`.
pub fn text_response_with_usage(
    model_call_id: &str,
    text: &str,
    input: u64,
    output: u64,
) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: model_call_id.into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta(text.into()),
        ModelEvent::TextEnd,
        ModelEvent::Usage(Usage {
            input_tokens: Some(input),
            context_input_tokens: Some(input),
            output_tokens: Some(output),
            total_tokens: Some(input + output),
            ..Default::default()
        }),
        ModelEvent::Done(FinishReason::Stop),
    ]
}

/// A single tool call that ends the model cycle with `FinishReason::ToolUse`.
pub fn tool_response(
    model_call_id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: model_call_id.into(),
        },
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: call_id.into(),
            name: name.into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: arguments.into(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Done(FinishReason::ToolUse),
    ]
}
