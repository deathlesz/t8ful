//! Micro-benchmarks for the incremental server-sent-events decoder.
//!
//! The transport hands the decoder arbitrary chunk boundaries; these runs feed
//! many small frames per chunk and one very large frame, and report throughput.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};

use t8ful_openai::sse::SseDecoder;

fn main() {
    divan::main();
}

/// An SSE body of `frames` data frames, each a small JSON delta.
fn frames(count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count * 80);

    for index in 0..count {
        let payload = format!("{{\"choices\":[{{\"delta\":{{\"content\":\"token {index}\"}}}}]}}");

        out.extend_from_slice(b"data: ");
        out.extend_from_slice(payload.as_bytes());
        out.extend_from_slice(b"\n\n");
    }

    out
}

/// The previous line extraction: search from index zero for every line and
/// drain the buffer once per line. Kept so the "before" number stays
/// reproducible.
#[derive(Default)]
struct LegacyDecoder {
    buffer: Vec<u8>,
}

impl LegacyDecoder {
    fn lines(&mut self, end_of_stream: bool) -> Vec<String> {
        let mut lines = Vec::new();

        while let Some(position) = self
            .buffer
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
        {
            if self.buffer[position] == b'\r' && position + 1 == self.buffer.len() && !end_of_stream
            {
                break;
            }

            let mut next = position + 1;

            if self.buffer[position] == b'\r' && self.buffer.get(position + 1) == Some(&b'\n') {
                next = position + 2;
            }

            let line = String::from_utf8_lossy(&self.buffer[..position]).into_owned();

            self.buffer.drain(..next);
            lines.push(line);
        }

        if end_of_stream && !self.buffer.is_empty() {
            let remainder = std::mem::take(&mut self.buffer);
            lines.push(String::from_utf8_lossy(&remainder).into_owned());
        }

        lines
    }
}

#[divan::bench]
fn many_small_frames_before(bencher: Bencher) {
    let body = frames(1024);
    const CHUNK: usize = 8 * 1024;

    bencher
        .counter(BytesCount::new(body.len()))
        .with_inputs(|| body.clone())
        .bench_local_values(|body| {
            let mut decoder = LegacyDecoder::default();
            let mut lines = 0;

            for chunk in body.chunks(CHUNK) {
                decoder.buffer.extend_from_slice(chunk);
                lines += decoder.lines(false).len();
            }

            lines + decoder.lines(true).len()
        });
}

#[divan::bench]
fn many_small_frames(bencher: Bencher) {
    let body = frames(1024);
    const CHUNK: usize = 8 * 1024;

    bencher
        .counter(BytesCount::new(body.len()))
        .with_inputs(|| body.clone())
        .bench_local_values(|body| {
            let mut decoder = SseDecoder::new();
            let mut events = 0;

            for chunk in body.chunks(CHUNK) {
                events += decoder
                    .push(chunk)
                    .expect("benchmark chunk within limits")
                    .len();
            }

            events
                + decoder
                    .finish()
                    .expect("benchmark stream within limits")
                    .len()
        });
}

#[divan::bench]
fn one_large_frame(bencher: Bencher) {
    let mut body = b"data: ".to_vec();

    body.extend(std::iter::repeat_n(b'x', 256 * 1024));
    body.extend_from_slice(b"\n\n");

    bencher
        .counter(BytesCount::new(body.len()))
        .with_inputs(|| body.clone())
        .bench_local_values(|body| {
            let mut decoder = SseDecoder::new();
            let events = decoder
                .push(black_box(&body))
                .expect("benchmark frame within limits");

            events.len()
                + decoder
                    .finish()
                    .expect("benchmark stream within limits")
                    .len()
        });
}
