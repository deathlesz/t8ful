//! Markdown rendering for assistant messages.
//!
//! Rendering happens at layout time and is display-only: the conversation,
//! journal, and provider requests keep the raw markdown source. The renderer
//! never emits a line wider than the requested width, and tolerates incomplete
//! markdown while a response is still streaming.

use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::theme::Theme;

/// One rendered markdown line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLine {
    pub spans: Vec<Span<'static>>,
    pub band: Option<Color>,
}

/// Markdown extensions every renderer enables.
fn parser_options() -> Options {
    let mut options = Options::empty();

    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    options
}

/// Render markdown for a terminal of `width` columns.
pub fn render(text: &str, width: usize, theme: &Theme) -> Vec<RenderedLine> {
    let width = width.max(1);
    let mut renderer = Renderer::new(width, theme);

    for event in Parser::new_ext(text, parser_options()) {
        renderer.event(event);
    }

    renderer.finish()
}

/// Render a prefix that ends at a block boundary, keeping the blank separator
/// line that a following block would sit behind.
fn render_prefix(text: &str, width: usize, theme: &Theme) -> Vec<RenderedLine> {
    let mut renderer = Renderer::new(width, theme);

    for event in Parser::new_ext(text, parser_options()) {
        renderer.event(event);
    }

    // No `finish`: a completed block has nothing buffered, and its trailing
    // blank line belongs to the gap before whatever arrives next.
    renderer.writer.lines
}

/// One rendered row, ready for the transcript layout.
pub type StreamRow = (Line<'static>, Option<Color>);

fn rows(rendered: Vec<RenderedLine>) -> Vec<StreamRow> {
    rendered
        .into_iter()
        .map(|line| (Line::from(line.spans), line.band))
        .collect()
}

/// A markdown document rendered while it is still arriving.
///
/// Finished top-level blocks are rendered once and kept; only the incomplete
/// tail is rendered again, so the cost of a streamed delta is proportional to
/// the block still growing instead of to the whole message.
///
/// One display rule differs from [`render`]: an unterminated fenced code block
/// is drawn without syntax colors, and is highlighted once its closing fence
/// arrives. Highlighting a growing block would redo the same work every frame.
#[derive(Debug)]
pub struct StreamingRenderer {
    width: usize,
    theme: Theme,
    /// Text already rendered into `rows`.
    rendered: String,
    rows: Vec<StreamRow>,
    /// Rows covered by `rendered`; the rows after it are the unfinished tail.
    stable_rows: usize,
    /// A link-reference definition can retarget a link in an earlier block, so
    /// a document with one is always rendered in full.
    reference_links: bool,
}

impl StreamingRenderer {
    pub fn new(width: usize, theme: Theme) -> Self {
        Self {
            width: width.max(1),
            theme,
            rendered: String::new(),
            rows: Vec::new(),
            stable_rows: 0,
            reference_links: false,
        }
    }

    /// Rows of the text so far, excluding trailing blank lines.
    pub fn rows(&self) -> &[StreamRow] {
        let mut len = self.rows.len();

        while len > 0 && self.rows[len - 1].0.spans.is_empty() {
            len -= 1;
        }

        &self.rows[..len]
    }

    /// Render the text so far, reusing everything already finished.
    pub fn render(&mut self, text: &str) -> &[StreamRow] {
        let width = self.width;

        if !text.starts_with(&self.rendered) {
            self.rendered.clear();
            self.rows.clear();
            self.stable_rows = 0;
        }

        if self.reference_links || defines_references(&text[self.rendered.len()..]) {
            self.reference_links = true;
            self.rows = rows(render(text, width, &self.theme));
            self.rendered.clear();
            self.rendered.push_str(text);
            self.stable_rows = self.rows.len();

            return self.rows();
        }

        let stable = stable_end(text, self.rendered.len());

        if stable > self.rendered.len() {
            let chunk = &text[self.rendered.len()..stable];

            self.rows.truncate(self.stable_rows);
            self.rows
                .extend(rows(render_prefix(chunk, width, &self.theme)));
            self.rendered.push_str(chunk);
            self.stable_rows = self.rows.len();
        } else {
            self.rows.truncate(self.stable_rows);
        }

        let tail = &text[self.rendered.len()..];

        self.rows
            .extend(rows(render_tail(tail, width, &self.theme)));

        self.rows()
    }
}

/// Whether the new text defines a link reference.
fn defines_references(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();

        line.starts_with('[')
            && line
                .split_once("]:")
                .is_some_and(|(label, _)| label.len() > 1)
    })
}

/// Byte offset just past the last top-level block that no later text can
/// extend.
///
/// A block only becomes final when a blank line follows it, and a list or a
/// block quote is never final: a later line can still add an item or a quoted
/// paragraph to it. Link reference definitions are the other cross-block
/// dependency, and the caller checks for them first.
fn stable_end(text: &str, from: usize) -> usize {
    let mut depth = 0usize;
    let mut end = from;

    for (event, range) in Parser::new_ext(&text[from..], parser_options()).into_offset_iter() {
        let boundary = from + range.end;

        match event {
            Event::Start(_) => depth += 1,
            Event::End(kind) => {
                depth = depth.saturating_sub(1);

                if depth == 0
                    && !matches!(kind, TagEnd::List(_) | TagEnd::BlockQuote(_))
                    && followed_by_blank_line(&text[boundary..])
                {
                    end = boundary;
                }
            }
            // A rule is a leaf event with no closing tag.
            Event::Rule if depth == 0 && followed_by_blank_line(&text[boundary..]) => {
                end = boundary;
            }
            _ => {}
        }
    }

    end
}

/// Whether the rest of the document starts with a blank line.
fn followed_by_blank_line(rest: &str) -> bool {
    for character in rest.chars() {
        match character {
            '\r' | ' ' | '\t' => {}
            '\n' => return true,
            _ => return false,
        }
    }

    false
}

/// Render the unfinished end of a document.
fn render_tail(text: &str, width: usize, theme: &Theme) -> Vec<RenderedLine> {
    if text.is_empty() {
        return Vec::new();
    }

    let Some((before, language, body)) = split_open_fence(text) else {
        return render(text, width, theme);
    };

    let mut rendered = render_prefix(before, width, theme);

    rendered.extend(plain_code(language, body, width, theme));

    rendered
}

/// Split a tail whose last block is an unterminated fenced code block into the
/// text before the fence, its language label, and the code so far.
fn split_open_fence(text: &str) -> Option<(&str, &str, &str)> {
    let mut open: Option<(char, usize)> = None;
    let mut fence_start = 0;
    let mut body_start = 0;
    let mut language = "";
    let mut offset = 0;

    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        if indent <= 3 {
            match open {
                Some((marker, length)) => {
                    let closing = trimmed.trim_end();

                    if closing.len() >= length && closing.chars().all(|found| found == marker) {
                        open = None;
                    }
                }
                None => {
                    if let Some((marker, length, info)) = fence_opener(trimmed) {
                        open = Some((marker, length));
                        language = info;
                        fence_start = offset;
                        body_start = offset + raw.len();
                    }
                }
            }
        }

        offset += raw.len();
    }

    open?;

    Some((&text[..fence_start], language, &text[body_start..]))
}

/// Detect a fenced code block opener, returning its marker, length, and info
/// string.
fn fence_opener(trimmed: &str) -> Option<(char, usize, &str)> {
    let marker = trimmed.chars().next()?;

    if marker != '`' && marker != '~' {
        return None;
    }

    let length = trimmed.chars().take_while(|found| *found == marker).count();

    if length < 3 {
        return None;
    }

    let info = trimmed[length..].trim();

    if marker == '`' && info.contains('`') {
        return None;
    }

    Some((marker, length, info))
}

/// Draw an unterminated code block without syntax highlighting.
fn plain_code(language: &str, body: &str, width: usize, theme: &Theme) -> Vec<RenderedLine> {
    let mut lines = Vec::new();

    if !language.is_empty() {
        lines.push(RenderedLine {
            spans: vec![Span::styled(
                format!(" {}", elide(language, width.saturating_sub(1))),
                Style::default().fg(theme.dim),
            )],
            band: None,
        });
    }

    let style = Style::default().fg(theme.fg).bg(theme.overlay);
    let inner = width.saturating_sub(1).max(1);

    for line in body.lines() {
        let units = line.chars().map(|character| (character, style)).collect();

        for wrapped in wrap_units(units, inner) {
            let mut spans = vec![Span::styled(" ", Style::default().bg(theme.overlay))];

            spans.extend(style_runs(wrapped));
            lines.push(RenderedLine { spans, band: None });
        }
    }

    lines
}

struct Renderer<'a> {
    width: usize,
    theme: &'a Theme,
    writer: Writer<'a>,
    emphasis: usize,
    strong: usize,
    strike: usize,
    quote_depth: usize,
    heading: Option<u8>,
    lists: Vec<ListState>,
    links: Vec<LinkState>,
    images: Vec<LinkState>,
    code_block: Option<CodeBlockState>,
    table: Option<TableState>,
}

enum ListState {
    Unordered,
    Ordered(u64),
}

struct LinkState {
    destination: String,
    text: String,
}

struct CodeBlockState {
    language: String,
    text: String,
}

#[derive(Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    header_rows: usize,
}

impl<'a> Renderer<'a> {
    fn new(width: usize, theme: &'a Theme) -> Self {
        Self {
            width,
            theme,
            writer: Writer::new(width, theme),
            emphasis: 0,
            strong: 0,
            strike: 0,
            quote_depth: 0,
            heading: None,
            lists: Vec::new(),
            links: Vec::new(),
            images: Vec::new(),
            code_block: None,
            table: None,
        }
    }

    fn finish(mut self) -> Vec<RenderedLine> {
        self.writer.finish();

        while self
            .writer
            .lines
            .last()
            .is_some_and(|line| line.spans.is_empty())
        {
            self.writer.lines.pop();
        }

        self.writer.lines
    }

    fn event(&mut self, event: Event<'_>) {
        if self.table.is_some() && self.table_event(&event) {
            return;
        }

        if let Some(block) = &mut self.code_block {
            match event {
                Event::Text(text) => {
                    block.text.push_str(&text);

                    return;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let block = self.code_block.take().expect("code block");
                    self.finish_code_block(block);

                    return;
                }
                _ => {}
            }
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                let style = self.inline_style();

                self.record_link_text(&text);
                self.writer.text(&text, style);
            }
            Event::Code(code) => {
                let style = Style::default()
                    .fg(self.theme.accent)
                    .bg(self.theme.overlay);

                self.record_link_text(&code);
                self.writer.text(&code, style);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let style = Style::default().fg(self.theme.dim);

                self.writer.text(&html, style);
            }
            Event::SoftBreak => self.writer.soft_space(),
            Event::HardBreak => self.writer.hard_break(),
            Event::Rule => {
                self.writer.block_end();
                let rule = "─".repeat(self.width);
                let style = Style::default().fg(self.theme.rule);

                self.writer.push_line(vec![Span::styled(rule, style)]);
                self.writer.blank_line();
            }
            Event::TaskListMarker(checked) => {
                let style = Style::default().fg(self.theme.accent);
                let marker = if checked { "[x] " } else { "[ ] " };

                self.writer.text(marker, style);
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() && self.quote_depth == 0 {
                    self.writer.clear_prefix();
                }
            }
            Tag::Heading { level, .. } => {
                let level = heading_level(level);

                self.heading = Some(level);
                self.writer.clear_prefix();

                if level == 1 {
                    let style = Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD);

                    self.writer.set_prefix(
                        vec![Span::styled("▌ ", style)],
                        2,
                        vec![Span::styled("  ", Style::default().fg(self.theme.dim))],
                        2,
                    );
                }
            }
            Tag::BlockQuote(_) => {
                self.quote_depth += 1;
                self.writer.set_prefix(
                    vec![Span::styled("▏ ", Style::default().fg(self.theme.dim))],
                    2,
                    vec![Span::styled("▏ ", Style::default().fg(self.theme.dim))],
                    2,
                );
            }
            Tag::CodeBlock(kind) => {
                self.writer.block_end();

                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };

                self.code_block = Some(CodeBlockState {
                    language,
                    text: String::new(),
                });
            }
            Tag::List(start) => {
                self.writer.finish_line();
                self.lists.push(match start {
                    Some(number) => ListState::Ordered(number),
                    None => ListState::Unordered,
                });
            }
            Tag::Item => {
                let depth = self.lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let bullet = match self.lists.last_mut() {
                    Some(ListState::Ordered(number)) => {
                        let bullet = format!("{number}. ");

                        *number += 1;
                        bullet
                    }
                    _ => "• ".to_owned(),
                };
                let prefix = format!("{indent}{bullet}");
                let width = prefix.chars().count();
                let bullet_style = Style::default().fg(self.theme.accent);

                self.writer.set_prefix(
                    vec![
                        Span::styled(indent, Style::default().fg(self.theme.dim)),
                        Span::styled(bullet, bullet_style),
                    ],
                    width,
                    vec![Span::styled(
                        " ".repeat(width),
                        Style::default().fg(self.theme.dim),
                    )],
                    width,
                );
            }
            Tag::HtmlBlock => self.writer.block_end(),
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.links.push(LinkState {
                    destination: dest_url.into_string(),
                    text: String::new(),
                });
            }
            Tag::Image { dest_url, .. } => {
                self.images.push(LinkState {
                    destination: dest_url.into_string(),
                    text: String::new(),
                });
            }
            Tag::Table(_) => {
                self.writer.block_end();
                self.table = Some(TableState::default());
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.writer.finish_line();

                if self.lists.is_empty() && self.quote_depth == 0 {
                    self.writer.blank_line();
                }
            }
            TagEnd::Heading(_) => {
                self.heading = None;
                self.writer.finish_line();
                self.writer.clear_prefix();
            }
            TagEnd::BlockQuote(_) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.writer.finish_line();
                self.writer.clear_prefix();

                if self.quote_depth == 0 {
                    self.writer.blank_line();
                }
            }
            TagEnd::CodeBlock => {}
            TagEnd::HtmlBlock => {
                self.writer.finish_line();
                self.writer.blank_line();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.writer.finish_line();
                self.writer.clear_prefix();
                self.writer.blank_line();
            }
            TagEnd::Item => {
                self.writer.finish_line();
            }
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some(link) = self.links.pop() {
                    self.finish_link(link);
                }
            }
            TagEnd::Image => {
                if let Some(image) = self.images.pop() {
                    self.finish_link(image);
                }
            }
            TagEnd::Table => self.finish_table(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);

                    if !row.is_empty() {
                        table.rows.push(row);
                    }

                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);

                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = std::mem::take(&mut table.cell);

                    table.row.push(cell);
                }
            }
            _ => {}
        }
    }

    fn table_event(&mut self, event: &Event<'_>) -> bool {
        let Some(table) = &mut self.table else {
            return false;
        };

        match event {
            Event::Text(text) | Event::Code(text) => {
                table.cell.push_str(text);

                true
            }
            Event::SoftBreak | Event::HardBreak => {
                table.cell.push(' ');

                true
            }
            Event::Start(Tag::TableRow) => {
                table.row.clear();

                true
            }
            Event::Start(Tag::TableCell) => {
                table.cell.clear();

                true
            }
            Event::End(TagEnd::TableCell) => {
                let cell = std::mem::take(&mut table.cell);

                table.row.push(cell);

                true
            }
            Event::End(TagEnd::TableRow) => {
                let row = std::mem::take(&mut table.row);

                table.rows.push(row);

                true
            }
            Event::End(TagEnd::TableHead) => {
                let row = std::mem::take(&mut table.row);

                if !row.is_empty() {
                    table.rows.push(row);
                }

                table.header_rows = table.rows.len();

                true
            }
            Event::End(TagEnd::Table) => {
                self.finish_table();

                true
            }
            _ => false,
        }
    }

    fn finish_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };

        if table.rows.is_empty() {
            return;
        }

        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let rows = table
            .rows
            .into_iter()
            .map(|mut row| {
                row.resize(columns, String::new());
                row.iter()
                    .map(|cell| cell.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let Some(widths) = column_widths(&rows, self.width) else {
            // Even one character per column does not fit; degrade to plain
            // rows rather than overflowing the terminal.
            let style = Style::default().fg(self.theme.fg);

            for row in &rows {
                let line = elide(&row.join(" │ "), self.width);

                self.writer.push_line(vec![Span::styled(line, style)]);
            }

            self.writer.blank_line();

            return;
        };

        let header_style = Style::default()
            .fg(self.theme.fg)
            .add_modifier(Modifier::BOLD);
        let body_style = Style::default().fg(self.theme.fg);
        let separator_style = Style::default().fg(self.theme.dim);

        for (index, row) in rows.iter().enumerate() {
            let style = if index < table.header_rows {
                header_style
            } else {
                body_style
            };
            let wrapped = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| wrap_cell(cell, *width))
                .collect::<Vec<_>>();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);

            for line in 0..height {
                let mut spans = Vec::new();

                for (column, (cell_lines, width)) in wrapped.iter().zip(&widths).enumerate() {
                    if column > 0 {
                        spans.push(Span::styled(" │ ", separator_style));
                    }

                    let text = cell_lines
                        .get(line)
                        .map(|text| text.trim_end())
                        .unwrap_or_default();
                    let last = column + 1 == columns;
                    let padding = width.saturating_sub(text.chars().count());

                    spans.push(Span::styled(
                        if last {
                            text.to_owned()
                        } else {
                            format!("{text}{}", " ".repeat(padding))
                        },
                        style,
                    ));
                }

                self.writer.push_line(spans);
            }

            if table.header_rows > 0 && index + 1 == table.header_rows {
                let rule = widths
                    .iter()
                    .map(|width| "─".repeat(*width))
                    .collect::<Vec<_>>()
                    .join("─┼─");

                self.writer.push_line(vec![Span::styled(
                    rule,
                    Style::default().fg(self.theme.rule),
                )]);
            }
        }

        self.writer.blank_line();
    }

    fn finish_code_block(&mut self, block: CodeBlockState) {
        if !block.language.is_empty() {
            self.writer.push_line(vec![Span::styled(
                format!(" {}", elide(&block.language, self.width.saturating_sub(1))),
                Style::default().fg(self.theme.dim),
            )]);
        }

        let lines = highlight_code(&block.text, &block.language, self.theme);
        let width = self.width.saturating_sub(1).max(1);

        for units in lines {
            for wrapped in wrap_units(units, width) {
                let mut spans = vec![Span::styled(" ", Style::default().bg(self.theme.overlay))];

                spans.extend(style_runs(wrapped));

                self.writer.push_line(spans);
            }
        }

        self.writer.blank_line();
    }

    fn record_link_text(&mut self, text: &str) {
        if let Some(link) = self.links.last_mut() {
            link.text.push_str(text);
        }

        if let Some(image) = self.images.last_mut() {
            image.text.push_str(text);
        }
    }

    fn finish_link(&mut self, link: LinkState) {
        let destination = link.destination.trim();
        let label = link.text.trim();

        if destination.is_empty() || destination == label || label.is_empty() {
            return;
        }

        let style = Style::default().fg(self.theme.dim);

        self.writer.text(&format!(" ({destination})"), style);
    }

    fn inline_style(&self) -> Style {
        let mut style = Style::default().fg(self.theme.fg);

        if let Some(level) = self.heading {
            style = match level {
                1 => Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
                2 => Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
                _ => Style::default()
                    .fg(self.theme.fg)
                    .add_modifier(Modifier::BOLD),
            };
        }

        if self.quote_depth > 0 {
            style = style.fg(self.theme.dim).add_modifier(Modifier::ITALIC);
        }

        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }

        if self.strong > 0 || self.heading.is_some() {
            style = style.add_modifier(Modifier::BOLD);
        }

        if self.strike > 0 {
            style = style.fg(self.theme.dim).add_modifier(Modifier::CROSSED_OUT);
        }

        style
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Column widths for a table rendered in `width` columns, or `None` when even
/// one character per column cannot fit.
///
/// Columns start at their natural width and the widest ones shrink until the
/// table fits; cell text then wraps inside its column instead of being elided.
fn column_widths(rows: &[Vec<String>], width: usize) -> Option<Vec<usize>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);

    if columns == 0 {
        return None;
    }

    let separators = 3 * (columns - 1);

    if width <= separators + columns {
        return None;
    }

    let available = width - separators;
    let mut widths = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| row.get(column).map_or(0, |cell| cell.chars().count()))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect::<Vec<_>>();

    // Cap every column at the widest value that still fits, keeping wide
    // columns balanced instead of starving the first one.
    let mut low = 1;
    let mut high = widths.iter().copied().max().unwrap_or(1);

    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let total: usize = widths.iter().map(|width| (*width).min(middle)).sum();

        if total <= available {
            low = middle;
        } else {
            high = middle - 1;
        }
    }

    widths
        .iter_mut()
        .for_each(|width| *width = (*width).min(low));

    Some(widths)
}

/// Wrap one cell to its column width, preserving every character.
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let units = text
        .chars()
        .map(|character| (character, Style::default()))
        .collect();

    wrap_units(units, width)
        .into_iter()
        .map(|line| line.into_iter().map(|(character, _)| character).collect())
        .collect()
}

struct Writer<'a> {
    width: usize,
    lines: Vec<RenderedLine>,
    current: Vec<Span<'static>>,
    column: usize,
    word: Vec<(char, Style)>,
    word_width: usize,
    pending_space: bool,
    /// Style of the whitespace waiting to be emitted.
    ///
    /// Without it, a gap between two words of the same styled run (inline code,
    /// bold text) would drop that run's background and look broken up.
    space_style: Option<Style>,
    first_prefix: Vec<Span<'static>>,
    first_prefix_width: usize,
    next_prefix: Vec<Span<'static>>,
    next_prefix_width: usize,
    prefix_used: bool,
    current_prefix_width: usize,
    content_started: bool,
    theme: &'a Theme,
}

impl<'a> Writer<'a> {
    fn new(width: usize, theme: &'a Theme) -> Self {
        Self {
            width,
            lines: Vec::new(),
            current: Vec::new(),
            column: 0,
            word: Vec::new(),
            word_width: 0,
            pending_space: false,
            space_style: None,
            first_prefix: Vec::new(),
            first_prefix_width: 0,
            next_prefix: Vec::new(),
            next_prefix_width: 0,
            prefix_used: false,
            current_prefix_width: 0,
            content_started: false,
            theme,
        }
    }

    fn clear_prefix(&mut self) {
        self.first_prefix.clear();
        self.first_prefix_width = 0;
        self.next_prefix.clear();
        self.next_prefix_width = 0;
    }

    fn set_prefix(
        &mut self,
        first: Vec<Span<'static>>,
        first_width: usize,
        next: Vec<Span<'static>>,
        next_width: usize,
    ) {
        self.first_prefix = first;
        self.first_prefix_width = first_width.min(self.width);
        self.prefix_used = false;
        self.next_prefix = next;
        self.next_prefix_width = next_width.min(self.width);
    }

    fn text(&mut self, text: &str, style: Style) {
        for character in text.chars() {
            match character {
                '\n' => {
                    self.flush_word();
                    self.hard_break();
                }
                character if character.is_whitespace() => {
                    self.flush_word();
                    self.pending_space = true;
                    self.space_style = Some(style);
                }
                character => {
                    self.word.push((character, style));
                    self.word_width += 1;
                }
            }
        }
    }

    fn soft_space(&mut self) {
        // A source newline between two words is a space; keep the style of the
        // word it follows so styled runs stay continuous across the break.
        let style = self.word.last().map(|(_, style)| *style);

        self.flush_word();
        self.pending_space = true;
        self.space_style = style;
    }

    fn hard_break(&mut self) {
        self.flush_word();
        self.newline();
    }

    fn finish_line(&mut self) {
        self.flush_word();

        if self.content_started || !self.current.is_empty() {
            self.newline();
        }
    }

    fn block_end(&mut self) {
        self.finish_line();
        self.blank_line();
    }

    fn blank_line(&mut self) {
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(RenderedLine {
                spans: Vec::new(),
                band: None,
            });
        }
    }

    fn push_line(&mut self, spans: Vec<Span<'static>>) {
        self.finish_line();
        self.lines.push(RenderedLine { spans, band: None });
    }

    fn finish(&mut self) {
        self.flush_word();

        if self.content_started || !self.current.is_empty() {
            self.newline();
        }
    }

    fn ensure_prefix(&mut self) {
        if self.content_started {
            return;
        }

        let (prefix, width) = if self.prefix_used {
            (&self.next_prefix, self.next_prefix_width)
        } else {
            (&self.first_prefix, self.first_prefix_width)
        };

        self.current.extend(prefix.clone());
        self.column += width;
        self.current_prefix_width = width;
        self.content_started = true;
    }

    fn flush_word(&mut self) {
        if self.word.is_empty() {
            return;
        }

        let word = std::mem::take(&mut self.word);
        let word_width = std::mem::take(&mut self.word_width);

        self.ensure_prefix();

        let space = usize::from(self.pending_space && self.column > self.current_prefix_width);

        if self.column > self.current_prefix_width && self.column + space + word_width > self.width
        {
            self.newline();
            self.ensure_prefix();
        }

        let mut remaining = word;
        let mut first_chunk = true;

        loop {
            let space = usize::from(
                first_chunk && self.pending_space && self.column > self.current_prefix_width,
            );

            if space == 1 {
                let style = self
                    .space_style
                    .unwrap_or_else(|| Style::default().fg(self.theme.fg));

                self.current.push(Span::styled(" ", style));
                self.column += 1;
            }

            let available = self.width.saturating_sub(self.column);

            if available == 0 {
                self.newline();
                self.ensure_prefix();
                first_chunk = false;

                continue;
            }

            if remaining.len() <= available {
                let width = remaining.len();

                self.push_run(remaining);
                self.column += width;

                break;
            }

            let tail = remaining.split_off(available);

            self.push_run(remaining);
            self.column += available;
            remaining = tail;
            self.newline();
            self.ensure_prefix();
            first_chunk = false;
        }

        self.pending_space = false;
        self.space_style = None;
    }

    fn push_run(&mut self, run: Vec<(char, Style)>) {
        let mut style = None;
        let mut text = String::new();

        for (character, next_style) in run {
            if style != Some(next_style) {
                if let Some(previous) = style {
                    self.current
                        .push(Span::styled(std::mem::take(&mut text), previous));
                }

                style = Some(next_style);
            }

            text.push(character);
        }

        if let Some(style) = style {
            self.current.push(Span::styled(text, style));
        }
    }

    fn newline(&mut self) {
        let spans = std::mem::take(&mut self.current);

        self.lines.push(RenderedLine { spans, band: None });
        self.column = 0;
        self.content_started = false;
        self.prefix_used = true;
        self.current_prefix_width = 0;
        self.pending_space = false;
        self.space_style = None;
    }
}

fn elide(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    if text.chars().count() <= width {
        return text.to_owned();
    }

    let mut short: String = text.chars().take(width.saturating_sub(1)).collect();

    short.push('…');
    short
}

fn style_runs(units: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut style = None;
    let mut text = String::new();

    for (character, next_style) in units {
        if style != Some(next_style) {
            if let Some(previous) = style {
                spans.push(Span::styled(std::mem::take(&mut text), previous));
            }

            style = Some(next_style);
        }

        text.push(character);
    }

    if let Some(style) = style {
        spans.push(Span::styled(text, style));
    }

    spans
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn highlight_code(text: &str, language: &str, theme: &Theme) -> Vec<Vec<(char, Style)>> {
    let syntax = if language.is_empty() {
        None
    } else {
        syntaxes().find_syntax_by_token(language)
    };

    highlight_code_with(
        text,
        syntax,
        theme,
        Style::default().fg(theme.fg).bg(theme.overlay),
    )
}

/// Highlight display-only tool preview text without the code-block background.
///
/// Returns wrapped lines of styled spans so tool output can reuse the same
/// syntect colors as markdown code blocks.
pub(crate) fn highlight_preview(
    text: &str,
    language: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let fallback = Style::default().fg(theme.fg);
    let lines = highlight_code_with(text, preview_syntax(language, text), theme, fallback);

    lines
        .into_iter()
        .flat_map(|units| wrap_units(units, width))
        .map(style_runs)
        .collect()
}

/// Resolve the syntax for a preview, from the most explicit hint to the
/// weakest guess: the tool-supplied language token, then a file extension, then
/// the document's first line (a shebang or mode line).
///
/// A preview with no usable hint renders plainly rather than guessing wrong.
fn preview_syntax(
    language: Option<&str>,
    text: &str,
) -> Option<&'static syntect::parsing::SyntaxReference> {
    let syntaxes = syntaxes();
    let language = language.unwrap_or_default().trim();

    if !language.is_empty() {
        // syntect's token lookup already tries the file extensions, but keeping
        // the explicit step documents the order callers rely on.
        if let Some(syntax) = syntaxes.find_syntax_by_token(language) {
            return Some(syntax);
        }

        if let Some(syntax) = syntaxes.find_syntax_by_extension(language) {
            return Some(syntax);
        }
    }

    text.lines()
        .next()
        .and_then(|line| syntaxes.find_syntax_by_first_line(line))
}

fn highlight_code_with(
    text: &str,
    syntax: Option<&syntect::parsing::SyntaxReference>,
    theme: &Theme,
    fallback: Style,
) -> Vec<Vec<(char, Style)>> {
    let plain = || {
        text.lines()
            .map(|line| {
                line.chars()
                    .map(|character| (character, fallback))
                    .collect()
            })
            .collect::<Vec<_>>()
    };

    let Some(syntax) = syntax else {
        return plain();
    };

    let syntaxes = syntaxes();
    let mut parser = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();

    for line in LinesWithEndings::from(text) {
        let content = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or_else(|| line.strip_suffix('\n').unwrap_or(line));
        let ops = parser.parse_line(line, syntaxes).unwrap_or_default();
        let mut units = Vec::new();
        let mut last = 0;

        for (offset, op) in ops {
            let offset = offset.min(content.len());

            if offset < last || !content.is_char_boundary(offset) {
                continue;
            }

            if offset > last {
                for character in content[last..offset].chars() {
                    units.push((character, scope_style(&stack, theme, fallback)));
                }
            }

            let _ = stack.apply(&op);
            last = offset;
        }

        if last < content.len() {
            for character in content[last..].chars() {
                units.push((character, scope_style(&stack, theme, fallback)));
            }
        }

        lines.push(units);
    }

    if lines.is_empty() {
        lines.push(Vec::new());
    }

    lines
}

fn scope_style(stack: &ScopeStack, theme: &Theme, fallback: Style) -> Style {
    // The innermost scope is the most specific one, so the first recognizable
    // scope on the stack decides; outer scopes are structural context only.
    for scope in stack.as_slice().iter().rev() {
        if let Some(style) = named_style(&scope.build_string(), theme, fallback) {
            return style;
        }
    }

    fallback
}

fn named_style(name: &str, theme: &Theme, fallback: Style) -> Option<Style> {
    let style = if name.starts_with("keyword.") || name.starts_with("storage.") {
        fallback.fg(theme.title)
    } else if name.starts_with("markup.heading")
        || name.starts_with("entity.name.section.")
        || name.starts_with("punctuation.definition.heading.")
    {
        fallback.fg(theme.title).add_modifier(Modifier::BOLD)
    } else if name.starts_with("markup.bold") || name.starts_with("punctuation.definition.bold.") {
        fallback.add_modifier(Modifier::BOLD)
    } else if name.starts_with("markup.italic")
        || name.starts_with("punctuation.definition.italic.")
    {
        fallback.add_modifier(Modifier::ITALIC)
    } else if name.starts_with("markup.strikethrough") {
        fallback.add_modifier(Modifier::CROSSED_OUT)
    } else if name.starts_with("markup.underline.link")
        || name.starts_with("markup.link")
        || name.starts_with("meta.link")
        || name.starts_with("string.other.link")
    {
        fallback.fg(theme.accent).add_modifier(Modifier::UNDERLINED)
    } else if name.starts_with("markup.underline") {
        fallback.add_modifier(Modifier::UNDERLINED)
    } else if name.starts_with("markup.raw") || name.starts_with("punctuation.definition.raw.") {
        fallback.fg(theme.warning)
    } else if name.starts_with("markup.quote") {
        fallback.fg(theme.dim).add_modifier(Modifier::ITALIC)
    } else if name.starts_with("markup.list")
        || name.starts_with("punctuation.definition.list_item.")
    {
        fallback.fg(theme.accent)
    } else if name.starts_with("string.") {
        fallback.fg(theme.success)
    } else if name.starts_with("comment.") {
        fallback.fg(theme.dim).add_modifier(Modifier::ITALIC)
    } else if name.starts_with("constant.numeric.") {
        fallback.fg(theme.warning)
    } else if name.starts_with("constant.language.") {
        fallback.fg(theme.warning).add_modifier(Modifier::BOLD)
    } else if name.starts_with("entity.name.function.") || name.starts_with("support.") {
        fallback.fg(theme.accent)
    } else if name.starts_with("entity.name.type.")
        || name.starts_with("entity.name.class.")
        || name.starts_with("entity.name.struct.")
    {
        fallback.fg(theme.title)
    } else if name.starts_with("variable.") {
        fallback.fg(theme.tool)
    } else if name.starts_with("invalid.") {
        fallback.fg(theme.error)
    } else {
        return None;
    };

    Some(style)
}

pub(crate) fn wrap_units(units: Vec<(char, Style)>, width: usize) -> Vec<Vec<(char, Style)>> {
    if units.is_empty() {
        return vec![Vec::new()];
    }

    let width = width.max(1);
    let indent_units = units
        .iter()
        .take_while(|(character, _)| *character == ' ')
        .cloned()
        .collect::<Vec<_>>();
    let indent_width = indent_units.len().min(width.saturating_sub(1));
    let indent = indent_units[..indent_width].to_vec();
    let mut lines = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut last_space = None;

    for (character, style) in units {
        if character == ' ' && current.len() < width {
            last_space = Some(current.len());
        }

        if current.len() == width {
            let split = last_space
                .filter(|index| *index > indent_width)
                .map(|index| index + 1);

            if let Some(split) = split {
                let tail = current.split_off(split);

                lines.push(std::mem::take(&mut current));
                current = indent.clone();
                current.extend(tail);
            } else {
                lines.push(std::mem::take(&mut current));
                current = indent.clone();
            }

            last_space = None;
        }

        current.push((character, style));
    }

    if !current.is_empty() {
        lines.push(current);
    }

    if lines.is_empty() {
        lines.push(Vec::new());
    }

    lines
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;
    use ratatui::text::Span;

    use super::{RenderedLine, StreamRow, StreamingRenderer, render, rows};
    use crate::theme::Theme;

    fn lines(text: &str, width: usize) -> Vec<RenderedLine> {
        render(text, width, &Theme::pi())
    }

    fn preview(text: &str, language: Option<&str>, width: usize) -> Vec<Vec<Span<'static>>> {
        super::highlight_preview(text, language, width, &Theme::pi())
    }

    fn preview_text(lines: &[Vec<Span<'static>>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn colors(lines: &[Vec<Span<'static>>]) -> Vec<ratatui::style::Color> {
        lines
            .iter()
            .flatten()
            .filter(|span| !span.content.trim().is_empty())
            .filter_map(|span| span.style.fg)
            .collect()
    }

    fn plain(lines: &[RenderedLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn assert_fits(lines: &[RenderedLine], width: usize) {
        for line in lines {
            let length = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();

            assert!(length <= width, "{length} > {width}: {line:?}");
        }
    }

    #[test]
    fn headings_lists_and_inline_styles_render_without_syntax() {
        let rendered = lines(
            "# Title\n\n- one\n- **two** and *three*\n\nUse `cargo test`.\n",
            60,
        );
        let text = plain(&rendered).join("\n");

        assert!(text.contains("▌ Title"), "{text}");
        assert!(!text.contains("# Title"), "{text}");
        assert!(text.contains("• one"), "{text}");
        assert!(text.contains("two"), "{text}");
        assert!(text.contains("three"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
        assert!(!text.contains('`'), "{text}");
        assert!(!text.contains("**"), "{text}");
    }

    #[test]
    fn inline_code_uses_the_accent_color_on_the_overlay_background() {
        let theme = Theme::pi();
        let rendered = render("run `cargo test` now", 40, &theme);
        let code = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("cargo"))
            .expect("inline code span");

        assert_eq!(code.style.fg, Some(theme.accent));
        assert_eq!(code.style.bg, Some(theme.overlay));
    }

    #[test]
    fn fences_highlight_known_languages_and_keep_unknown_ones_plain() {
        let theme = Theme::pi();
        let rendered = render(
            "```rust\nfn main() {\n    let value = 42; // note\n    println!(\"hi\");\n}\n```\n",
            60,
            &theme,
        );
        let colors = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| !span.content.trim().is_empty())
            .filter_map(|span| span.style.fg)
            .collect::<Vec<_>>();

        assert!(colors.len() > 1, "{colors:?}");
        assert!(colors.contains(&theme.title), "{colors:?}");
        assert!(colors.contains(&theme.success), "{colors:?}");
        assert!(colors.contains(&theme.dim), "{colors:?}");
        assert!(colors.contains(&theme.warning), "{colors:?}");
        assert!(plain(&rendered).join("\n").contains("rust"));

        let rendered = render("```not-a-language\nplain text\n```\n", 40, &theme);
        let text = plain(&rendered).join("\n");

        assert!(text.contains("plain text"), "{text}");

        let content = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("plain text"))
            .expect("code span");

        assert_eq!(content.style.bg, Some(theme.overlay));
        assert_eq!(content.style.fg, Some(theme.fg));
    }

    #[test]
    fn an_unclosed_fence_renders_while_streaming_and_settles() {
        let streaming = lines("```rust\nfn main() {\n", 40);
        let settled = lines("```rust\nfn main() {\n}\n```\n", 40);

        assert!(!plain(&streaming).is_empty());
        assert!(plain(&streaming).join("\n").contains("fn main"));
        assert!(plain(&settled).join("\n").contains("fn main"));
    }

    #[test]
    fn tables_align_wrap_and_never_overflow() {
        let rendered = lines(
            "| tool | status |\n| --- | --- |\n| read_file | ok |\n| edit | denied |\n",
            40,
        );
        let text = plain(&rendered);
        let rows = text
            .iter()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();

        assert!(rows.len() >= 4, "{text:?}");
        assert!(rows[1].contains('┼'), "{text:?}");

        let separator = rows[0].find('│').expect("a separator in the header");

        for row in &rows[2..] {
            assert_eq!(
                row.find('│'),
                Some(separator),
                "a body row is not aligned: {text:?}"
            );
        }

        assert!(rows[0].contains("tool"), "{text:?}");
        assert!(rows[2].starts_with("read_file"), "{text:?}");

        let wide = lines(
            "| step | result |\n| --- | --- |\n| 3 | cargo test --workspace --all-targets and then some more words here |\n",
            32,
        );
        let text = plain(&wide).join("\n");

        assert!(text.contains("cargo test"), "{text}");
        assert!(text.contains("more words here"), "{text}");
        assert!(!text.contains('…'), "{text}");
        assert_fits(&wide, 32);

        let narrow = lines(
            "| tool | status |\n| --- | --- |\n| read_file | ok |\n| edit_file | denied |\n",
            24,
        );
        let text = plain(&narrow).join("\n");

        assert!(text.contains("tool"), "{text}");
        assert!(text.contains("denied"), "{text}");
        assert!(text.contains('│'), "{text}");
        assert_fits(&narrow, 24);
    }

    #[test]
    fn long_tokens_hard_break_and_never_overflow() {
        for text in [
            "supercalifragilisticexpialidocious",
            "```rust\nlet thing = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\";\n```\n",
            "| aaaaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbbbb |\n| --- | --- |\n",
        ] {
            let rendered = lines(text, 16);

            assert_fits(&rendered, 16);
        }
    }

    #[test]
    fn a_full_width_line_followed_by_a_space_does_not_panic() {
        let units = "0123456789 "
            .chars()
            .map(|character| (character, Style::default()))
            .collect();

        let wrapped = super::wrap_units(units, 10);

        assert!(wrapped.iter().all(|line| line.len() <= 10));
    }

    #[test]
    fn html_renders_literally_and_dimly() {
        let theme = Theme::pi();
        let rendered = render("<div>raw</div>\n", 40, &theme);
        let text = plain(&rendered).join("\n");
        let span = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("<div>"))
            .expect("html span");

        assert!(text.contains("<div>raw</div>"), "{text}");
        assert_eq!(span.style.fg, Some(theme.dim));
    }

    #[test]
    fn paragraphs_and_quotes_keep_their_spacing_and_gutter() {
        let rendered = lines("first paragraph\n\nsecond paragraph\n", 40);
        let text = plain(&rendered);

        assert_eq!(text.iter().filter(|line| line.is_empty()).count(), 1);
        assert!(text.iter().any(|line| line.contains("first")));
        assert!(text.iter().any(|line| line.contains("second")));

        let theme = Theme::pi();
        let rendered = render("> quoted text\n", 40, &theme);
        let text = plain(&rendered).join("\n");
        let italic = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| {
                span.style.fg == Some(theme.dim)
                    && span
                        .style
                        .add_modifier
                        .contains(ratatui::style::Modifier::ITALIC)
            });

        assert!(text.contains("▏ quoted text"), "{text}");
        assert!(italic);
    }

    #[test]
    fn links_and_images_show_their_destination() {
        let rendered = lines(
            "see [docs](https://example.com/docs) and ![alt](https://example.com/a.png)\n",
            100,
        );
        let text = plain(&rendered).join("\n");

        assert!(text.contains("docs (https://example.com/docs)"), "{text}");
        assert!(text.contains("alt (https://example.com/a.png)"), "{text}");
    }

    #[test]
    fn code_blocks_preserve_indentation_and_blank_lines() {
        let rendered = lines("```rust\nfn main() {\n\n    let x = 1;\n}\n```\n", 40);
        let text = plain(&rendered);

        assert!(
            text.iter().any(|line| line.contains("    let x = 1;")),
            "{text:?}"
        );
        assert!(text.iter().any(|line| line.trim().is_empty()), "{text:?}");
    }

    #[test]
    fn every_line_uses_the_theme_and_fits_the_width() {
        let theme = Theme::pi();
        let rendered = render(
            "## Heading\n\nA paragraph with **bold** text and a [link](https://x).\n\n\
             - item\n- item two\n\n> quote\n\n```python\nprint('hi')\n```\n",
            32,
            &theme,
        );

        assert_fits(&rendered, 32);

        for line in &rendered {
            for span in &line.spans {
                assert!(span.style.fg.is_some() || span.style.bg == Some(theme.overlay));
            }
        }
    }

    #[test]
    fn empty_input_produces_no_lines() {
        assert!(lines("", 40).is_empty());
    }

    /// A document with one of every block kind this renderer handles, minus
    /// code fences (an open fence is deliberately drawn unhighlighted).
    const CORPUS: &str = "# Heading\n\nA paragraph with `inline code`, **bold**, \
and ~~struck~~ text plus a [link](https://example.test).\n\n## Second\n\n- one\n\
- two\n  - nested\n\n1. first\n2. second\n\n> quoted line\n> more\n\n| a | b |\n\
|---|---|\n| 1 | 2 |\n\nA long unbroken word: supercalifragilisticexpialidocious\n\n\
Unicode: héllo → wörld\n";

    fn expected(text: &str, width: usize) -> Vec<StreamRow> {
        rows(render(text, width, &Theme::pi()))
    }

    #[test]
    fn every_prefix_and_appended_block_matches_a_full_render() {
        for width in [8, 20, 60] {
            let mut stream = StreamingRenderer::new(width, Theme::pi());

            for end in 0..=CORPUS.len() {
                if !CORPUS.is_char_boundary(end) {
                    continue;
                }

                let prefix = &CORPUS[..end];

                assert_eq!(
                    stream.render(prefix),
                    expected(prefix, width),
                    "width {width} prefix {end}"
                );
            }
        }

        let mut stream = StreamingRenderer::new(60, Theme::pi());

        for end in [0, 10, 24, 48, 96, 160] {
            stream.render(&CORPUS[..end]);
        }

        assert_eq!(stream.render(CORPUS), expected(CORPUS, 60));
    }

    #[test]
    fn reference_definitions_and_open_fences_fall_back_to_a_full_render() {
        let text = "See [docs] for the rules.\n\n[docs]: https://example.test/rules\n";
        let mut stream = StreamingRenderer::new(40, Theme::pi());

        stream.render("See [docs] for the rules.\n\n");

        let rendered = stream.render(text).to_vec();

        assert_eq!(rendered, expected(text, 40));

        let flat = rendered
            .iter()
            .map(|(line, _)| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(flat.contains("https://example.test/rules"), "{flat}");

        let theme = Theme::pi();
        let width = 40;
        let open = "Intro\n\n```rust\nfn main() {\n    let answer = 42;\n";
        let mut stream = StreamingRenderer::new(width, theme);

        let plain = stream.render(open).to_vec();
        let text = plain
            .iter()
            .map(|(line, _)| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains(" rust"), "{text}");
        assert!(text.contains("fn main() {"), "{text}");
        assert!(text.contains("let answer = 42;"), "{text}");

        for (line, _) in &plain {
            let body = line.to_string();

            if body.trim() == "rust" {
                continue;
            }

            for span in &line.spans {
                assert!(
                    span.style.fg.is_none() || span.style.fg == Some(theme.fg),
                    "unhighlighted while streaming: {span:?}"
                );
            }
        }

        let closed = format!("{open}```\n");
        let highlighted = stream.render(&closed).to_vec();

        assert_eq!(highlighted, expected(&closed, width));
        assert!(
            highlighted
                .iter()
                .flat_map(|(line, _)| &line.spans)
                .any(|span| span
                    .style
                    .fg
                    .is_some_and(|color| { color != theme.fg && color != theme.dim })),
            "the closed fence is highlighted again"
        );
    }

    #[test]
    fn a_preview_without_a_known_language_stays_plain() {
        let theme = Theme::pi();
        let rendered = preview("plain text\nsecond line\n", Some("not-a-language"), 40);

        assert_eq!(preview_text(&rendered), "plain text\nsecond line");
        assert!(
            colors(&rendered).iter().all(|color| *color == theme.fg),
            "{rendered:?}"
        );
    }

    #[test]
    fn a_preview_guesses_the_language_from_the_first_line() {
        let theme = Theme::pi();
        let rendered = preview("#!/bin/sh\necho hi\n", None, 40);
        let palette = colors(&rendered);

        assert!(palette.contains(&theme.accent), "{palette:?}");
    }

    #[test]
    fn preview_lines_never_exceed_the_width() {
        let rendered = preview(
            "# A heading that is far too long for the preview width\n",
            Some("md"),
            12,
        );

        for line in &rendered {
            let length = line
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();

            assert!(length <= 12, "{length} > 12: {line:?}");
        }
    }
}
