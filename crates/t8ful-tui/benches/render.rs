//! Micro-benchmarks for the transcript, streaming markdown, and composer paths.
//!
//! Four shapes worth watching: a streamed delta on a 20 KB message, an appended
//! 256 KB tool body, a mention keypress over a large tree, and an 8 KB prompt.
//! Together they cover the paths where a small change turns into visible
//! latency — re-rendering a whole message per delta, re-wrapping a growing body,
//! scanning a hot completion list, and laying out a large composer.

use divan::{Bencher, black_box};

use t8ful_tui::commands::suggestions;
use t8ful_tui::markdown::{StreamingRenderer, render};
use t8ful_tui::state::{AppState, Entry, Header, ToolPreview};
use t8ful_tui::theme::Theme;
use t8ful_tui::ui::{TranscriptLayout, WrapCache, composer_lines, wrap_ranges};

fn main() {
    divan::main();
}

/// Terminal width of the performance baselines.
const WIDTH: usize = 78;

/// A markdown document of roughly `target` bytes with one of every block kind.
fn document(target: usize) -> String {
    let block = "## A heading\n\nA paragraph with `inline code`, **bold**, \
and a [link](https://example.test/reference) that wraps across a few lines.\n\n\
- first item\n- second item with a longer tail\n  - nested item\n\n\
> quoted remark\n\n| alpha | beta |\n|---|---|\n| 1 | 2 |\n\n\
```rust\nfn main() {\n    let answer = 42;\n}\n```\n\n";
    let mut out = String::with_capacity(target + block.len());

    while out.len() < target {
        out.push_str(block);
    }

    out
}

/// Tool output of roughly `target` bytes, one short line at a time.
fn body(target: usize) -> String {
    let mut out = String::with_capacity(target + 80);
    let mut line = 0;

    while out.len() < target {
        out.push_str(&format!("line {line:06} of the command output\n"));
        line += 1;
    }

    out
}

/// Synthetic project paths, one of every shape a real tree offers.
fn paths(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| match index % 4 {
            0 => format!("src/fuzzy_{index:05}.rs"),
            1 => format!("crates/module{}/src/main_{index:05}.rs", index % 97),
            2 => format!("docs/notes/{index:05}-matching.md"),
            _ => format!("vendor/deep/tree/path_{index:05}/file.rs"),
        })
        .collect()
}

#[divan::bench]
fn markdown_render_2kb(bencher: Bencher) {
    let document = document(2 * 1024);
    let theme = Theme::pi();

    bencher.bench(|| render(black_box(&document), WIDTH, &theme));
}

/// One restored history entry: 50 lines of plain text, no markup.
#[divan::bench]
fn markdown_render_plain_2500(bencher: Bencher) {
    let mut entry = String::new();

    for row in 0..50 {
        entry.push_str(&format!(
            "ROW{row:06} synthetic terminal transcript line......\n"
        ));
    }

    let theme = Theme::pi();

    bencher.bench(|| render(black_box(&entry), WIDTH, &theme).len());
}

#[divan::bench]
fn markdown_render_20kb(bencher: Bencher) {
    let document = document(20 * 1024);
    let theme = Theme::pi();

    bencher.bench(|| render(black_box(&document), WIDTH, &theme));
}

/// One streamed delta: a warm renderer takes a 20 KB message from 99 % to
/// complete. This is the paint cost per delta the budget covers.
#[divan::bench]
fn streaming_delta_20kb(bencher: Bencher) {
    let document = document(20 * 1024) + "and one more line of streamed prose.\n";
    let theme = Theme::pi();
    let cut = document.len() - 64;

    bencher
        .with_inputs(|| {
            let mut renderer = StreamingRenderer::new(WIDTH, theme);

            renderer.render(&document[..cut]);

            renderer
        })
        .bench_local_values(|mut renderer| {
            renderer.render(black_box(&document));
            black_box(renderer.rows().len())
        });
}

/// The same delta when it closes a fenced code block: the tail is re-rendered
/// once more with syntax highlighting.
#[divan::bench]
fn streaming_delta_20kb_closing_fence(bencher: Bencher) {
    let document = document(20 * 1024);
    let theme = Theme::pi();
    let cut = document.len() - 64;

    bencher
        .with_inputs(|| {
            let mut renderer = StreamingRenderer::new(WIDTH, theme);

            renderer.render(&document[..cut]);

            renderer
        })
        .bench_local_values(|mut renderer| {
            renderer.render(black_box(&document));
            black_box(renderer.rows().len())
        });
}

/// The previous paint path: re-render the whole message for every delta.
#[divan::bench]
fn streaming_delta_20kb_before(bencher: Bencher) {
    let document = document(20 * 1024);
    let theme = Theme::pi();

    bencher.bench(|| render(black_box(&document), WIDTH, &theme).len());
}

/// A whole 20 KB message streamed in 64-byte appends.
#[divan::bench]
fn streaming_whole_20kb(bencher: Bencher) {
    let document = document(20 * 1024);
    let theme = Theme::pi();

    bencher.bench_local(|| {
        let mut renderer = StreamingRenderer::new(WIDTH, theme);
        let mut end = 0;

        while end < document.len() {
            end = (end + 64).min(document.len());

            if !document.is_char_boundary(end) {
                continue;
            }

            renderer.render(&document[..end]);
        }

        black_box(renderer.rows().len())
    });
}

#[divan::bench]
fn wrap_ranges_256kb(bencher: Bencher) {
    let body = body(256 * 1024);

    bencher.bench(|| wrap_ranges(black_box(&body), WIDTH).len());
}

/// One progress chunk on a running tool card, before the wrap cache: re-wrap
/// and re-materialize the whole accumulated body, then drop all but its tail.
#[divan::bench]
fn tool_body_chunk_256kb_before(bencher: Bencher) {
    let body = body(256 * 1024);

    bencher.bench(|| {
        let ranges = wrap_ranges(&body, WIDTH);
        let skip = ranges.len().saturating_sub(4);
        let rows = ranges[skip..]
            .iter()
            .map(|(start, end)| body[*start..*end].to_owned())
            .collect::<Vec<_>>();

        black_box(rows.len())
    });
}

/// The same chunk with the wrap cache warm and only the tail materialized.
#[divan::bench]
fn tool_body_chunk_256kb(bencher: Bencher) {
    let body = body(256 * 1024);
    let cut = body.len() - 128;

    bencher
        .with_inputs(|| {
            let mut cache = WrapCache::new(WIDTH);

            cache.page(&body[..cut], Some(4));

            cache
        })
        .bench_local_values(|mut cache| {
            let (_, rows) = cache.page(black_box(&body), Some(4));
            let materialized = rows
                .iter()
                .map(|(start, end)| body[*start..*end].to_owned())
                .collect::<Vec<_>>();

            black_box(materialized.len())
        });
}

/// The same 256 KB body appended 128 bytes at a time, as a running tool does.
#[divan::bench]
fn wrap_cache_append_256kb(bencher: Bencher) {
    let body = body(256 * 1024);

    bencher.bench_local(|| {
        let mut cache = WrapCache::new(WIDTH);
        let mut end = 0;
        let mut rows = 0;

        while end < body.len() {
            end = (end + 128).min(body.len());

            if !body.is_char_boundary(end) {
                continue;
            }

            rows = cache.page(&body[..end], Some(4)).0;
        }

        black_box(rows)
    });
}

#[divan::bench(args = ["@src/fuz", "@mod"])]
fn mention_suggestions_50k(bencher: Bencher, input: &str) {
    let files = paths(50_000);

    bencher.bench(|| suggestions(black_box(input), &[], &[], &files, &[], 0).len());
}

/// The pre-optimization matcher, kept so the "before" number stays
/// reproducible: a `Vec<char>` per query and per candidate, and a full sort of
/// every match.
fn legacy_score(query: &str, haystack: &str) -> Option<i32> {
    fn boundary(character: char) -> bool {
        character.is_whitespace() || matches!(character, '/' | '-' | '_' | ':' | '.')
    }

    fn folded(left: char, right: char) -> bool {
        left == right || left.to_lowercase().eq(right.to_lowercase())
    }

    let query = query.chars().collect::<Vec<_>>();

    if query.is_empty() {
        return Some(0);
    }

    let haystack = haystack.chars().collect::<Vec<_>>();
    let mut total = 0;
    let mut next = 0;
    let mut previous = None;

    for expected in query {
        let position =
            (next..haystack.len()).find(|&position| folded(haystack[position], expected))?;
        let contiguous = previous.is_some_and(|previous: usize| position == previous + 1);
        let at_boundary = position == 0 || boundary(haystack[position - 1]);

        total += if contiguous {
            3
        } else if at_boundary {
            2
        } else {
            1
        };
        previous = Some(position);
        next = position + 1;
    }

    Some(total)
}

fn legacy_mention(query: &str, files: &[String]) -> usize {
    let mut rows = files
        .iter()
        .filter_map(|path| legacy_score(query, path).map(|score| (score, path)))
        .collect::<Vec<_>>();

    rows.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.len().cmp(&right.len()))
            .then_with(|| left.cmp(right))
    });

    rows.truncate(50);

    rows.len()
}

#[divan::bench(args = ["src/fuz", "mod"])]
fn mention_suggestions_50k_before(bencher: Bencher, query: &str) {
    let files = paths(50_000);

    bencher.bench(|| legacy_mention(black_box(query), &files));
}

#[divan::bench]
fn composer_lines_8kb(bencher: Bencher) {
    let text = "abcdefghij".repeat(800);

    bencher.bench(|| composer_lines(black_box(&text), WIDTH).len());
}

/// The previous composer wrap, which recounted the whole line in progress for
/// every character it appended.
#[divan::bench]
fn composer_lines_8kb_before(bencher: Bencher) {
    let text = "abcdefghij".repeat(800);

    fn legacy(input: &str, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut lines = Vec::new();

        for logical in input.split('\n') {
            if logical.is_empty() {
                lines.push(String::new());

                continue;
            }

            let mut current = String::new();

            for character in logical.chars() {
                if current.chars().count() == width {
                    lines.push(std::mem::take(&mut current));
                }

                current.push(character);
            }

            lines.push(current);
        }

        lines
    }

    bencher.bench(|| legacy(black_box(&text), WIDTH).len());
}

/// A 20 000-line transcript: mostly prose, with tool cards sprinkled through it.
fn transcript() -> AppState {
    let mut state = AppState::new(Header {
        model: "vendor/model".to_owned(),
        mode: t8ful_core::policy::Mode::Coding,
        session_id: "bench".to_owned(),
        project_root: std::path::PathBuf::from("/work/project"),
        launch_dir: std::path::PathBuf::from("/work/project"),
        version: "0.1.0".to_owned(),
        branch: None,
    });

    state.entries = (0..20_000)
        .map(|index| match index % 20 {
            0 => Entry::Tool {
                call_id: format!("call-{index}"),
                name: "bash".to_owned(),
                status: "ok".to_owned(),
                content: format!("output {index}\nl2\nl3\nl4\nl5\nl6\n"),
                truncated: false,
                title: format!("command {index}"),
                preview: ToolPreview::default(),
                duration: None,
                placement: Default::default(),
            },
            _ => Entry::User(format!("prompt {index} with a little more text to wrap")),
        })
        .collect();

    state
}

/// Ctrl-O on a 20 000-line transcript, rebuilding only the tool cards.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn toggle_expanded_20k(bencher: Bencher) {
    bencher
        .with_inputs(|| {
            let state = transcript();
            let mut layout = TranscriptLayout::default();

            layout.sync(&state, WIDTH);

            (state, layout)
        })
        .bench_local_values(|(mut state, mut layout)| {
            state.toggle_expanded();
            layout.sync(&state, WIDTH);
            black_box(layout.total_lines())
        });
}

/// The same toggle before partial invalidation: the whole layout was cleared
/// and every entry rendered again.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn toggle_expanded_20k_before(bencher: Bencher) {
    bencher
        .with_inputs(transcript)
        .bench_local_values(|mut state| {
            state.toggle_expanded();

            let mut layout = TranscriptLayout::default();

            layout.sync(&state, WIDTH);
            black_box(layout.total_lines())
        });
}
