//! OpenCode Go provider for t8ful.
//!
//! Go speaks the OpenAI chat-completions dialect, so the transport, SSE
//! decoding, tool-call assembly, and error mapping come from `t8ful-openai`.
//! This crate owns what makes Go different: its two endpoints, its bearer key,
//! the `x-opencode-session` identifier it requires on every chat request, its
//! catalog shape, and its billing block.

mod client;
mod models;

pub use client::{DEFAULT_BASE_URL, OpenCodeGoClient, PROVIDER_ID, USER_AGENT, session_header};
