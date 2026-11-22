//! OpenRouter provider for t8ful.
//!
//! The crate owns OpenRouter-specific protocol details: authentication, model
//! discovery, request attribution, and the `reasoning` request object. The
//! transport — server-sent-event framing, reconstruction of fragmented tool
//! calls, HTTP plumbing, and error mapping — is shared with every other
//! OpenAI-compatible provider through `t8ful-openai`, and is re-exported here so
//! existing paths keep working.

mod client;
mod models;

pub use client::{OpenRouterClient, Redacted};
pub use t8ful_openai::{Assembler, sse, stream};
