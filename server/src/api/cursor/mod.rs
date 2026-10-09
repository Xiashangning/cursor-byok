//! Wires the Cursor-facing API routes.

pub mod bidi;
mod handlers;
pub mod proxy;
pub mod run_sse;

pub use handlers::{router, router_with_proxy};
