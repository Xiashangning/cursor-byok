//! Exposes provider-independent search capabilities.
mod cache;
mod catalog;
mod engine;
mod federation;
mod fetch;
mod search_provider;

pub(crate) use cache::{cache_bytes, clear_caches};
pub use cache::{WebCache, WebCacheEntry};
pub use engine::{HtmlEngine, JsonEngine, SearchEngine, SearchHit};
pub use federation::{SearchError, WebSearch};
pub use fetch::{FetchError, FetchedPage, WebFetch};
pub(crate) use search_provider::execute as execute_semble;
