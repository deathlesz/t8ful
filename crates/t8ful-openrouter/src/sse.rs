//! Incremental server-sent-events decoder.
//!
//! The decoder is fed raw response bytes and never assumes a chunk boundary
//! lines up with a frame, a line, or a UTF-8 code point. Lines are split on raw
//! bytes because `\r` and `\n` cannot occur inside a multi-byte UTF-8 sequence.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Longest single SSE line accepted, in bytes.
pub const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
/// Longest accumulated SSE frame accepted, in bytes.
pub const MAX_SSE_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// A response that cannot be a bounded SSE stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    #[error("an SSE line exceeded the {limit}-byte limit")]
    LineTooLong { limit: usize },

    #[error("an SSE frame exceeded the {limit}-byte limit")]
    FrameTooLong { limit: usize },
}

/// Buffering SSR parser for one HTTP response body.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    event: Option<String>,
    frame_bytes: usize,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of response bytes and return every frame it completed.
    ///
    /// An over-limit line or frame clears the decoder and returns an error, so
    /// a malformed stream is reported instead of retained.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        if chunk.len() > MAX_SSE_FRAME_BYTES {
            self.reset();

            return Err(SseError::FrameTooLong {
                limit: MAX_SSE_FRAME_BYTES,
            });
        }

        self.buffer.extend_from_slice(chunk);

        let lines = self.take_lines(false);

        if self.buffer.len() > MAX_SSE_LINE_BYTES {
            self.reset();

            return Err(SseError::LineTooLong {
                limit: MAX_SSE_LINE_BYTES,
            });
        }

        self.consume_lines(lines)
    }

    /// Flush the remaining bytes at end of stream.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>, SseError> {
        let lines = self.take_lines(true);
        let mut events = self.consume_lines(lines)?;

        if let Some(event) = self.dispatch() {
            events.push(event);
        }

        Ok(events)
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.data_lines.clear();
        self.event = None;
        self.frame_bytes = 0;
    }

    fn take_lines(&mut self, end_of_stream: bool) -> Vec<String> {
        let mut lines = Vec::new();
        let mut start = 0;
        let mut consumed = 0;
        let mut index = 0;
        let buffer = &self.buffer;

        // One forward scan: the previous version searched from index zero for
        // every line and drained once per line, which is quadratic in the
        // number of frames a chunk holds.
        while index < buffer.len() {
            let byte = buffer[index];

            if byte != b'\n' && byte != b'\r' {
                index += 1;

                continue;
            }

            // A trailing `\r` may still be the first half of `\r\n`.
            if byte == b'\r' && index + 1 == buffer.len() && !end_of_stream {
                break;
            }

            let mut next = index + 1;

            if byte == b'\r' && buffer.get(index + 1) == Some(&b'\n') {
                next = index + 2;
            }

            lines.push(String::from_utf8_lossy(&buffer[start..index]).into_owned());

            start = next;
            consumed = next;
            index = next;
        }

        self.buffer.drain(..consumed);

        if end_of_stream && !self.buffer.is_empty() {
            let remainder = std::mem::take(&mut self.buffer);
            lines.push(String::from_utf8_lossy(&remainder).into_owned());
        }

        lines
    }

    fn consume_lines(&mut self, lines: Vec<String>) -> Result<Vec<SseEvent>, SseError> {
        let mut events = Vec::new();

        for line in lines {
            if line.len() > MAX_SSE_LINE_BYTES {
                self.reset();

                return Err(SseError::LineTooLong {
                    limit: MAX_SSE_LINE_BYTES,
                });
            }

            if line.is_empty() {
                if let Some(event) = self.dispatch() {
                    events.push(event);
                }

                continue;
            }

            if line.starts_with(':') {
                continue;
            }

            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line.as_str(), ""),
            };

            match field {
                "data" => {
                    self.frame_bytes = self.frame_bytes.saturating_add(value.len() + 1);

                    if self.frame_bytes > MAX_SSE_FRAME_BYTES {
                        self.reset();

                        return Err(SseError::FrameTooLong {
                            limit: MAX_SSE_FRAME_BYTES,
                        });
                    }

                    self.data_lines.push(value.to_owned());
                }
                "event" => self.event = Some(value.to_owned()),
                _ => {}
            }
        }

        Ok(events)
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        let data_lines = std::mem::take(&mut self.data_lines);
        self.frame_bytes = 0;

        if data_lines.is_empty() && event.is_none() {
            return None;
        }

        Some(SseEvent {
            event,
            data: data_lines.join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SSE_LINE_BYTES, SseDecoder, SseError};

    fn decode_in_chunks(chunks: &[&[u8]]) -> Vec<String> {
        let mut decoder = SseDecoder::new();
        let mut data = Vec::new();

        for chunk in chunks {
            data.extend(
                decoder
                    .push(chunk)
                    .expect("chunk within limits")
                    .into_iter()
                    .map(|event| event.data),
            );
        }

        data.extend(
            decoder
                .finish()
                .expect("stream within limits")
                .into_iter()
                .map(|event| event.data),
        );
        data
    }

    #[test]
    fn decodes_plain_frames() {
        let events = decode_in_chunks(&[b"data: one\n\ndata: two\n\n"]);

        assert_eq!(events, vec!["one", "two"]);
    }

    #[test]
    fn decodes_every_split_point() {
        let payload = b"event: message\r\ndata: {\"a\":1}\r\n\r\ndata: [DONE]\r\n\r\n";

        for split in 0..payload.len() {
            let (head, tail) = payload.split_at(split);
            let events = decode_in_chunks(&[head, tail]);

            assert_eq!(events, vec!["{\"a\":1}", "[DONE]"], "split at {split}");
        }
    }

    #[test]
    fn decodes_one_byte_at_a_time() {
        let payload = b"data: alpha\n\ndata: beta\n\n";
        let chunks = payload.chunks(1).collect::<Vec<_>>();
        let events = decode_in_chunks(&chunks);

        assert_eq!(events, vec!["alpha", "beta"]);
    }

    #[test]
    fn joins_multiline_data_and_skips_comments() {
        let events = decode_in_chunks(&[b": keep-alive\ndata: line one\ndata: line two\n\n"]);

        assert_eq!(events, vec!["line one\nline two"]);
    }

    #[test]
    fn flushes_a_final_frame_without_trailing_blank_line() {
        let events = decode_in_chunks(&[b"data: last"]);

        assert_eq!(events, vec!["last"]);
    }

    #[test]
    fn keeps_utf8_split_across_chunks() {
        let payload = "data: héllo wörld\n\n".as_bytes();
        let split = payload.len() - 8;
        let (head, tail) = payload.split_at(split);
        let events = decode_in_chunks(&[head, tail]);

        assert_eq!(events, vec!["héllo wörld"]);
    }

    #[test]
    fn an_overlong_line_is_rejected_and_clears_the_decoder() {
        let mut decoder = SseDecoder::new();
        let payload = format!("data: {}\n\n", "a".repeat(MAX_SSE_LINE_BYTES + 1));
        let error = decoder.push(payload.as_bytes()).expect_err("line limit");

        assert!(matches!(error, SseError::LineTooLong { .. }), "{error}");

        let events = decoder
            .push(b"data: ok\n\n")
            .expect("decoder state was reset");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "ok");
    }

    #[test]
    fn an_overlong_frame_is_rejected_across_chunks() {
        let mut decoder = SseDecoder::new();
        let line = "a".repeat(MAX_SSE_LINE_BYTES - 32);
        let mut frame = Vec::new();

        for _ in 0..3 {
            frame.extend_from_slice(b"data: ");
            frame.extend_from_slice(line.as_bytes());
            frame.push(b'\n');
        }

        let events = decoder.push(&frame).expect("each line is within the limit");

        assert!(events.is_empty());

        let mut overflow = Vec::new();

        for _ in 0..2 {
            overflow.extend_from_slice(b"data: ");
            overflow.extend_from_slice(line.as_bytes());
            overflow.push(b'\n');
        }

        let error = decoder.push(&overflow).expect_err("frame limit");

        assert!(matches!(error, SseError::FrameTooLong { .. }), "{error}");
    }

    #[test]
    fn ignores_unknown_fields_and_blank_comments() {
        let events = decode_in_chunks(&[b"id: 42\nretry: 100\ndata: value\n\n:\n\n"]);

        assert_eq!(events, vec!["value"]);
    }
}
