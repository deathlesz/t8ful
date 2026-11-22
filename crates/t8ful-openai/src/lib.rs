//! OpenAI-compatible wire protocol shared by every t8ful provider.
//!
//! OpenRouter and OpenCode Go both speak the OpenAI chat-completions dialect.
//! They differ only in the endpoint path, the spelling of a few request fields,
//! and their attributed headers, so the streaming decoder, the tool-call
//! assembler, the HTTP plumbing, and the prompt serialization live here once.
//! A provider supplies a [`ProtocolProfile`] plus its own headers and catalog.

pub mod client;
pub mod error;
pub mod messages;
pub mod profile;
pub mod pump;
pub mod sse;
pub mod stream;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use client::{HttpStream, Secret, build_http_client, read_capped_body};
pub use error::{bounded_error_body, check_response, stalled, summarize, transport_error};
pub use messages::{message_values, request_body};
pub use profile::{ProtocolProfile, ReasoningStyle, UsageStyle};
pub use pump::{MAX_MODEL_CATALOG_BYTES, STREAM_IDLE_TIMEOUT, pump_stream};
pub use sse::{SseDecoder, SseEvent};
pub use stream::Assembler;
