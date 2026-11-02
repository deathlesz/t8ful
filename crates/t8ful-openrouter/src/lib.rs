//! OpenRouter provider for t8ful.
//!
//! The crate owns OpenRouter-specific protocol details: authentication, model
//! discovery, server-sent-event framing, reconstruction of fragmented tool
//! calls, and error mapping. Everything it exposes is expressed in `t8ful-core`
//! types so the agent loop and frontends stay provider-agnostic.

mod client;
mod models;
pub mod sse;
pub mod stream;

pub use client::{OpenRouterClient, Redacted};
pub use stream::Assembler;
