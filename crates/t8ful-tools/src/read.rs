//! Bounded, UTF-8-safe file reading.
//!
//! Line ranges keep their 1-based inclusive behavior. A byte offset reads a
//! bounded window for files that are one enormous line, where line ranges
//! cannot make progress.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

use crate::paths;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

/// Bytes one line may occupy before line mode gives up on it.
///
/// A line larger than the whole output budget cannot be returned by a line
/// range in any useful way, so it is never buffered in full: the read stops at
/// the budget and reports the byte offset where the line starts.
const MAX_LINE_BYTES: usize = MAX_BYTES;

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    byte_offset: Option<usize>,
}

#[derive(Debug, Default)]
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a bounded range of a UTF-8 text file. Lines are 1-based and the \
                          range is inclusive. For a file that is one long line, pass byte_offset \
                          (zero-based) instead; the response reports the next offset when \
                          truncated."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, absolute or relative to the launch directory" },
                    "start_line": { "type": "integer", "minimum": 1, "description": "First line to read (default 1)" },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Last line to read (default end of file)" },
                    "byte_offset": { "type": "integer", "minimum": 0, "description": "Zero-based byte offset; cannot be combined with start_line or end_line" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

        let path = paths::resolve(&ctx.launch_dir, &args.path);

        if path.is_dir() {
            return Err(ToolError::Failed(format!(
                "{} is a directory",
                path.display()
            )));
        }

        if let Some(offset) = args.byte_offset {
            if args.start_line.is_some() || args.end_line.is_some() {
                return Err(ToolError::InvalidArguments(
                    "byte_offset cannot be combined with start_line or end_line".to_owned(),
                ));
            }

            return read_bytes(&path, offset);
        }

        read_lines(&path, &args)
    }
}

fn read_lines(path: &Path, args: &ReadArgs) -> Result<ToolOutput, ToolError> {
    let file = open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut offset = 0_usize;
    let mut line_number = 0_usize;
    let mut total_lines = 0_usize;
    let mut selected_lines = 0_usize;
    let start = args.start_line.unwrap_or(1).max(1);
    let end = args.end_line.unwrap_or(usize::MAX);
    let mut rendered = String::new();
    let mut used = 0_usize;
    let mut written = 0_usize;
    let mut truncated = false;
    let mut overlong = false;
    let mut next_byte_offset = None;
    let mut crlf_lines = 0_usize;
    let mut lf_lines = 0_usize;

    if end < start {
        return Err(ToolError::InvalidArguments(format!(
            "end_line {end} is before start_line {start}"
        )));
    }

    loop {
        line.clear();

        // One byte past the budget tells "line ended at the limit" apart from
        // "the line is longer than this tool will buffer".
        let (read, ended) = read_line(&mut reader, &mut line).map_err(|error| {
            ToolError::Failed(format!("cannot read {}: {error}", path.display()))
        })?;

        if read == 0 {
            break;
        }

        line_number += 1;
        total_lines = line_number;

        let line_start = offset;

        offset += read;

        if !ended && read > MAX_LINE_BYTES {
            // The rest of this line is not read: a single line that large is
            // only readable through byte_offset, and skipping it could walk an
            // endless non-regular file.
            truncated = true;
            next_byte_offset = Some(line_start);
            overlong = true;

            break;
        }

        let text = std::str::from_utf8(&line).map_err(|_| not_utf8(path))?;
        let text = text.strip_suffix('\n').unwrap_or(text);

        // The carriage return is stripped for display, so the reader has to
        // report it: a multi-line `edit_file` old_text copied from this output
        // can never match a CRLF file byte-for-byte. Only lines the caller asked
        // for are counted, because the report tells the model how to build that
        // edit and a line it never saw cannot guide one.
        let (text, carriage_return) = match text.strip_suffix('\r') {
            Some(text) => (text, true),
            None => (text, false),
        };

        if line_number < start || line_number > end {
            continue;
        }

        if carriage_return {
            crlf_lines += 1;
        } else if ended {
            lf_lines += 1;
        }

        selected_lines += 1;

        if truncated {
            continue;
        }

        let cost = text.len() + 1;

        if written >= MAX_LINES || used + cost > MAX_BYTES {
            truncated = true;
            next_byte_offset = Some(line_start);

            continue;
        }

        rendered.push_str(text);
        rendered.push('\n');

        used += cost;
        written += 1;
    }

    // A scan stopped at an over-long line never saw the whole file, so its
    // counts say nothing about where `start_line` falls.
    if !truncated {
        if total_lines == 0 && start > 1 {
            return Err(ToolError::Failed(format!(
                "start_line {start} is beyond the end of {} which has 0 lines",
                path.display()
            )));
        }

        if total_lines > 0 && start > total_lines {
            return Err(ToolError::Failed(format!(
                "start_line {start} is beyond the end of {} which has {total_lines} lines",
                path.display()
            )));
        }
    }

    if truncated {
        let next_offset = next_byte_offset.unwrap_or(offset);

        if written == 0 {
            // Repeating the same start_line would return the same nothing, so
            // the only useful continuation is the byte offset.
            let reason = if overlong {
                format!("the first selected line is longer than {MAX_LINE_BYTES} bytes")
            } else {
                format!("the first selected line does not fit in {MAX_BYTES} bytes")
            };

            rendered.push_str(&format!(
                "\n[truncated: {reason}, which line mode cannot return; continue with \
                 byte_offset={next_offset}]\n"
            ));
        } else {
            let next_line = start + written;

            rendered.push_str(&format!(
                "\n[truncated: showed {written} of {selected_lines} selected lines; continue with \
                 start_line={next_line} or byte_offset={next_offset}]\n"
            ));
        }
    }

    let line_endings = (crlf_lines + lf_lines > 0).then(|| line_ending_style(crlf_lines, lf_lines));

    if crlf_lines > 0 {
        rendered.push_str(&format!(
            "\n[line endings: {style}; the trailing \\r of every line above was stripped \
             for display, so a multi-line edit_file old_text must contain the \\r bytes]\n",
            style = line_ending_style(crlf_lines, lf_lines)
        ));
    }

    let next_line = (truncated && written > 0).then_some(start + written);
    let metadata = json!({
        "path": path.display().to_string(),
        "start_line": start,
        "lines_returned": written,
        "total_lines": total_lines,
        "truncated": truncated,
        "line_endings": line_endings,
        "next_line": next_line,
        "next_byte_offset": next_byte_offset,
        "bytes_returned": used,
    });

    Ok(ToolOutput {
        content: rendered,
        truncated,
        metadata: Some(metadata),
    })
}

/// The line-ending style of the lines read so far, from the stripped `\r`.
fn line_ending_style(crlf_lines: usize, lf_lines: usize) -> &'static str {
    match (crlf_lines, lf_lines) {
        (0, _) => "LF",
        (_, 0) => "CRLF",
        _ => "mixed",
    }
}

/// Read one line, stopping at the line budget.
///
/// Returns the bytes read and whether a newline ended the line within the
/// budget. A line longer than [`MAX_LINE_BYTES`] leaves the rest unread in the
/// reader; the caller stops instead of scanning it.
fn read_line(
    reader: &mut std::io::BufReader<File>,
    line: &mut Vec<u8>,
) -> std::io::Result<(usize, bool)> {
    let read = reader
        .by_ref()
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', line)?;

    Ok((read, line.last() == Some(&b'\n')))
}

fn read_bytes(path: &Path, offset: usize) -> Result<ToolOutput, ToolError> {
    let mut file = open(path)?;
    let file_bytes = file
        .metadata()
        .map_err(|error| ToolError::Failed(format!("cannot inspect {}: {error}", path.display())))?
        .len() as usize;

    if offset > file_bytes {
        return Err(ToolError::Failed(format!(
            "byte_offset {offset} is beyond the end of {} ({file_bytes} bytes)",
            path.display()
        )));
    }

    if offset == file_bytes {
        return Ok(ToolOutput {
            content: String::new(),
            truncated: false,
            metadata: Some(json!({
                "path": path.display().to_string(),
                "byte_offset": offset,
                "bytes_returned": 0,
                "file_bytes": file_bytes,
                "total_lines": Value::Null,
                "truncated": false,
                "next_byte_offset": Value::Null,
            })),
        });
    }

    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|error| ToolError::Failed(format!("cannot seek {}: {error}", path.display())))?;

    let limit = MAX_BYTES.min(file_bytes - offset);
    let mut bytes = vec![0_u8; limit];
    let read = file
        .read(&mut bytes)
        .map_err(|error| ToolError::Failed(format!("cannot read {}: {error}", path.display())))?;

    bytes.truncate(read);

    if offset > 0
        && bytes
            .first()
            .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
    {
        return Err(inside_character(path, offset));
    }

    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) => {
            let valid = &bytes[..error.valid_up_to()];

            match error.error_len() {
                // The window ended in the middle of a character: keep the
                // complete prefix and let the next call start at a boundary.
                None => std::str::from_utf8(valid).expect("valid prefix"),
                Some(_) if error.valid_up_to() == 0 && offset > 0 => {
                    return Err(inside_character(path, offset));
                }
                Some(_) => return Err(not_utf8(path)),
            }
        }
    };

    let consumed = text.len();
    let next = offset + consumed;
    let truncated = next < file_bytes;
    let mut content = text.to_owned();

    if truncated {
        content.push_str(&format!(
            "\n[truncated: showed bytes {offset}..{next} of {file_bytes}; continue with \
             byte_offset={next}]\n"
        ));
    }

    Ok(ToolOutput {
        content,
        truncated,
        metadata: Some(json!({
            "path": path.display().to_string(),
            "byte_offset": offset,
            "bytes_returned": consumed,
            "file_bytes": file_bytes,
            "total_lines": Value::Null,
            "truncated": truncated,
            "next_byte_offset": truncated.then_some(next),
        })),
    })
}

fn open(path: &Path) -> Result<File, ToolError> {
    // Refuse anything that is not a regular file before opening it. Opening a
    // FIFO for reading blocks until a writer appears, and a character device
    // such as /dev/zero never ends, so a byte budget alone cannot bound the
    // read. The metadata lookup follows symlinks, so a link to a regular file
    // still works.
    match std::fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(ToolError::Failed(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        Ok(_) => {}
        // A missing path keeps the existing not-found wording below.
        Err(_) => {}
    }

    File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            ToolError::Failed(format!("{} does not exist", path.display()))
        }
        std::io::ErrorKind::IsADirectory => {
            ToolError::Failed(format!("{} is a directory", path.display()))
        }
        _ => ToolError::Failed(format!("cannot read {}: {error}", path.display())),
    })
}

fn not_utf8(path: &Path) -> ToolError {
    ToolError::Failed(format!(
        "{} is not valid UTF-8; this tool only handles text files",
        path.display()
    ))
}

fn inside_character(path: &Path, offset: usize) -> ToolError {
    ToolError::Failed(format!(
        "byte_offset {offset} is inside a UTF-8 character in {}",
        path.display()
    ))
}
