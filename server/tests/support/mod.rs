//! Shared scaffolding for the integration test binaries.
//!
//! Each test file declares `mod support;` and imports what it needs, e.g.
//! `use support::{drive, kv_ack, run_request, text_response, FakeProvider};`.

#![allow(dead_code)] // every test binary compiles this module and uses a subset
#![allow(unused_imports)] // re-exports below are consumed by a subset of binaries

pub mod events;
pub mod fake_cursor;
pub mod fake_provider;
pub mod fixtures;
pub mod pump;
pub mod requests;
pub mod wait;
pub mod wire;

pub use events::{text_response, text_response_with_usage, tool_response};
pub use fake_cursor::decode_single;
pub use fake_provider::FakeProvider;
pub use fixtures::{openai_model_input, prompt_assets, registry, temp_store, user};
pub use pump::{drive, text_of, PumpOutput};
pub use requests::{resume_action, run_request, user_message_action};
pub use wait::wait_for_provider_requests;
pub use wire::{
    acknowledge_kv, kv_ack, read_success, request_context_success, stream_close,
    subagent_await_complete, subagent_result_error, subagent_result_success,
};
