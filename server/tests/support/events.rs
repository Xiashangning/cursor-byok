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
    tool_calls_response(model_call_id, &[(call_id, name, arguments.to_owned())])
}
/// A batch of tool calls that ends the model cycle with `FinishReason::ToolUse`.
/// Each entry is `(call_id, name, arguments)`; the index is the position.
pub fn tool_calls_response(model_call_id: &str, calls: &[(&str, &str, String)]) -> Vec<ModelEvent> {
    let mut events = vec![ModelEvent::Start {
        model_call_id: model_call_id.into(),
    }];
    for (index, (call_id, name, arguments)) in calls.iter().enumerate() {
        events.push(ModelEvent::ToolCallStart {
            index,
            call_id: (*call_id).into(),
            name: (*name).into(),
        });
        events.push(ModelEvent::ToolCallArgumentsDelta {
            index,
            delta: arguments.clone(),
        });
        events.push(ModelEvent::ToolCallEnd { index });
    }
    events.push(ModelEvent::Done(FinishReason::ToolUse));
    events
}
