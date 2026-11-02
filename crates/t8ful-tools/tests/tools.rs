use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use t8ful_core::tool::{ToolCall, ToolContext, ToolRegistry, ToolStatus};
use t8ful_tools::{ManagedProcesses, NoSkills, builtin_registry};

struct Fixture {
    dir: tempfile::TempDir,
    registry: ToolRegistry,
    processes: Arc<ManagedProcesses>,
    context: ToolContext,
}

impl Fixture {
    fn new() -> Self {
        Self::with_capacity(256 * 1024)
    }

    fn with_capacity(capacity: usize) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let processes = Arc::new(ManagedProcesses::new(capacity));
        let registry = builtin_registry(Arc::clone(&processes), Arc::new(NoSkills));
        let mut context = ToolContext::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        context.default_timeout = Duration::from_secs(20);
        context.max_output_bytes = capacity;
        context.deny_env = vec!["T8FUL_TEST_SECRET".to_owned()];

        Self {
            dir,
            registry,
            processes,
            context,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path(relative);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parents");
        }

        std::fs::write(path, contents).expect("write fixture file");
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative)).expect("read fixture file")
    }

    async fn call(&self, tool: &str, arguments: Value) -> Result<Value, String> {
        let call = ToolCall::new("call-1", tool, arguments);
        let tool = self.registry.get(tool).expect("tool registered");

        match tool.execute(&call, &self.context).await {
            Ok(output) => Ok(json!({
                "content": output.content,
                "truncated": output.truncated,
                "metadata": output.metadata,
            })),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn ok(&self, tool: &str, arguments: Value) -> Value {
        self.call(tool, arguments)
            .await
            .unwrap_or_else(|error| panic!("{tool} failed: {error}"))
    }

    async fn fail(&self, tool: &str, arguments: Value) -> String {
        self.call(tool, arguments)
            .await
            .expect_err("tool call should fail")
    }
}

fn content(value: &Value) -> &str {
    value["content"].as_str().expect("content string")
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_file_returns_an_inclusive_range() {
    let fixture = Fixture::new();

    fixture.write("notes.txt", "one\ntwo\nthree\nfour\n");

    let output = fixture
        .ok(
            "read_file",
            json!({ "path": "notes.txt", "start_line": 2, "end_line": 3 }),
        )
        .await;

    assert_eq!(content(&output), "two\nthree\n");
    assert_eq!(output["metadata"]["total_lines"], json!(4));
    assert_eq!(output["truncated"], json!(false));
}

#[tokio::test]
async fn read_file_reports_truncation_for_large_ranges() {
    let fixture = Fixture::new();
    let lines = (1..=2500)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    fixture.write("big.txt", &lines);

    let output = fixture.ok("read_file", json!({ "path": "big.txt" })).await;

    assert_eq!(output["truncated"], json!(true));
    assert_eq!(output["metadata"]["lines_returned"], json!(2000));
    assert!(content(&output).contains("[truncated:"));
}

#[tokio::test]
async fn read_file_reports_missing_paths_and_directories() {
    let fixture = Fixture::new();

    let missing = fixture
        .fail("read_file", json!({ "path": "nope.txt" }))
        .await;

    assert!(missing.contains("does not exist"), "{missing}");

    std::fs::create_dir(fixture.path("dir")).unwrap();

    let directory = fixture.fail("read_file", json!({ "path": "dir" })).await;

    assert!(directory.contains("is a directory"), "{directory}");
}

#[tokio::test]
async fn read_file_returns_empty_content_for_an_empty_file() {
    let fixture = Fixture::new();

    fixture.write("empty.txt", "");

    let output = fixture
        .ok("read_file", json!({ "path": "empty.txt" }))
        .await;

    assert_eq!(content(&output), "");
    assert_eq!(output["truncated"], json!(false));
    assert_eq!(output["metadata"]["total_lines"], json!(0));
}

#[tokio::test]
async fn read_file_byte_offset_continues_a_long_line() {
    let fixture = Fixture::new();
    let body = "x".repeat(60 * 1024);

    fixture.write("long.txt", &body);

    let first = fixture.ok("read_file", json!({ "path": "long.txt" })).await;

    assert_eq!(first["truncated"], json!(true));
    assert_eq!(first["metadata"]["next_byte_offset"], json!(0));
    assert!(
        content(&first).contains("byte_offset=0"),
        "{}",
        content(&first)
    );
    // Nothing was returned for the selected line, so repeating the same
    // start_line would give the same emptiness; only the byte offset helps.
    assert_eq!(first["metadata"]["next_line"], Value::Null);
    assert!(
        !content(&first).contains("start_line="),
        "the hint repeated the start it could not use: {}",
        content(&first)
    );
    assert!(
        content(&first).contains("which line mode cannot return"),
        "{}",
        content(&first)
    );

    let second = fixture
        .ok("read_file", json!({ "path": "long.txt", "byte_offset": 0 }))
        .await;

    assert_eq!(second["metadata"]["bytes_returned"], json!(50 * 1024));
    assert_eq!(second["metadata"]["next_byte_offset"], json!(50 * 1024));
    assert_eq!(second["truncated"], json!(true));
    assert!(
        content(&second).contains("continue with byte_offset=51200"),
        "{}",
        content(&second)
    );

    let third = fixture
        .ok(
            "read_file",
            json!({ "path": "long.txt", "byte_offset": 50 * 1024 }),
        )
        .await;

    assert_eq!(third["truncated"], json!(false));
    assert_eq!(content(&third), "x".repeat(10 * 1024));
    assert_eq!(third["metadata"]["next_byte_offset"], json!(Value::Null));
}

/// A path that produces bytes forever must not be buffered or scanned forever.
///
/// Regression: `read_until` collected the whole line before the size check, so
/// a line that never ends (a readable non-regular path such as `/dev/zero`)
/// allocated without bound and never returned.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn read_file_refuses_a_non_regular_file_instead_of_reading_forever() {
    let fixture = Fixture::new();
    // A character device never reaches end-of-file, and a FIFO blocks in open
    // until a writer appears, so a byte budget cannot bound either one.
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        fixture.fail("read_file", json!({ "path": "/dev/zero" })),
    )
    .await
    .expect("read_file blocked on a character device");

    assert!(
        error.contains("not a regular file"),
        "unexpected error: {error}"
    );
}

/// One line far larger than the output budget is reported at its byte offset
/// rather than materialized in full.
#[tokio::test]
async fn read_file_reports_an_oversized_line_at_its_offset() {
    let fixture = Fixture::new();
    let body = format!("first line\n{}\nlast line", "y".repeat(4 * 1024 * 1024));

    fixture.write("huge.txt", &body);

    let output = fixture
        .ok("read_file", json!({ "path": "huge.txt", "start_line": 2 }))
        .await;

    assert_eq!(output["truncated"], json!(true));
    assert_eq!(output["metadata"]["next_byte_offset"], json!(11));
    assert_eq!(output["metadata"]["lines_returned"], json!(0));
    assert!(
        content(&output).contains("byte_offset=11"),
        "{}",
        content(&output)
    );
}

#[tokio::test]
async fn read_file_byte_offsets_do_not_split_utf8_characters() {
    let fixture = Fixture::new();
    let body = "€".repeat(20_000);

    fixture.write("euro.txt", &body);

    let first = fixture
        .ok("read_file", json!({ "path": "euro.txt", "byte_offset": 0 }))
        .await;

    assert_eq!(first["metadata"]["bytes_returned"], json!(51_198));
    assert_eq!(first["metadata"]["next_byte_offset"], json!(51_198));
    assert!(!content(&first).contains('�'), "{}", &content(&first)[..64]);

    let inside = fixture
        .fail("read_file", json!({ "path": "euro.txt", "byte_offset": 1 }))
        .await;

    assert!(inside.contains("UTF-8 character"), "{inside}");

    let boundary = fixture
        .ok("read_file", json!({ "path": "euro.txt", "byte_offset": 3 }))
        .await;

    assert!(content(&boundary).starts_with('€'));
}

#[tokio::test]
async fn read_file_rejects_invalid_byte_offsets() {
    let fixture = Fixture::new();

    fixture.write("small.txt", "abc\n");

    let beyond = fixture
        .fail(
            "read_file",
            json!({ "path": "small.txt", "byte_offset": 10 }),
        )
        .await;

    assert!(beyond.contains("beyond the end"), "{beyond}");

    let at_end = fixture
        .ok(
            "read_file",
            json!({ "path": "small.txt", "byte_offset": 4 }),
        )
        .await;

    assert_eq!(content(&at_end), "");
    assert_eq!(at_end["truncated"], json!(false));
}

#[tokio::test]
async fn read_file_rejects_byte_offset_with_a_line_range() {
    let fixture = Fixture::new();

    fixture.write("small.txt", "one\ntwo\n");

    let error = fixture
        .fail(
            "read_file",
            json!({ "path": "small.txt", "byte_offset": 0, "start_line": 1 }),
        )
        .await;

    assert!(
        error.contains("cannot be combined") || error.contains("mutually exclusive"),
        "{error}"
    );
}

#[tokio::test]
async fn read_file_scans_large_files_without_collecting_every_line() {
    let fixture = Fixture::new();
    let body = (1..=20_000)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();

    fixture.write("many.txt", &body);

    let output = fixture.ok("read_file", json!({ "path": "many.txt" })).await;

    assert_eq!(output["truncated"], json!(true));
    assert_eq!(output["metadata"]["lines_returned"], json!(2000));
    assert_eq!(output["metadata"]["total_lines"], json!(20_000));
    assert_eq!(output["metadata"]["next_line"], json!(2001));
    assert!(
        output["metadata"]["next_byte_offset"].as_u64().unwrap() > 0,
        "{}",
        content(&output)
    );
}

#[tokio::test]
async fn read_file_rejects_a_start_beyond_the_file() {
    let fixture = Fixture::new();

    fixture.write("small.txt", "only\n");

    let error = fixture
        .fail(
            "read_file",
            json!({ "path": "small.txt", "start_line": 10 }),
        )
        .await;

    assert!(error.contains("beyond the end"), "{error}");
}

#[tokio::test]
async fn read_file_reports_only_the_line_endings_it_shows() {
    let fixture = Fixture::new();

    // Line 3 is the only LF line, and it is the only line the range shows.
    fixture.write("half.txt", "a\r\nb\r\nc\n");

    let output = fixture
        .ok(
            "read_file",
            json!({ "path": "half.txt", "start_line": 3, "end_line": 3 }),
        )
        .await;

    assert_eq!(
        output["metadata"]["line_endings"],
        json!("LF"),
        "the report must describe the shown line, not the whole file: {}",
        content(&output)
    );
    assert!(
        !content(&output).contains("[line endings: CRLF;")
            && !content(&output).contains("[line endings: mixed;"),
        "a CRLF line outside the range must not guide the edit: {}",
        content(&output)
    );
}

#[tokio::test]
async fn read_file_reports_crlf_line_endings_so_a_multiline_edit_can_match() {
    let fixture = Fixture::new();

    fixture.write("windows.txt", "one\r\ntwo\r\nthree\r\n");

    let output = fixture
        .ok("read_file", json!({ "path": "windows.txt" }))
        .await;

    assert_eq!(output["metadata"]["line_endings"], json!("CRLF"));
    assert!(
        content(&output).contains("[line endings: CRLF;"),
        "the reader must report the style it stripped: {}",
        content(&output)
    );

    // An old_text copied from the displayed lines cannot match a CRLF file,
    // which is exactly the failure the report has to explain.
    let mismatch = fixture
        .fail(
            "edit_file",
            json!({ "path": "windows.txt", "edits": [
                { "old_text": "one\ntwo", "new_text": "1\n2" },
            ] }),
        )
        .await;

    assert!(mismatch.contains("no exact match"), "{mismatch}");

    // Guided by the report, the edit includes the carriage returns.
    fixture
        .ok(
            "edit_file",
            json!({ "path": "windows.txt", "edits": [
                { "old_text": "one\r\ntwo", "new_text": "1\r\n2" },
            ] }),
        )
        .await;

    assert_eq!(fixture.read("windows.txt"), "1\r\n2\r\nthree\r\n");
}

#[tokio::test]
async fn read_file_reports_mixed_line_endings() {
    let fixture = Fixture::new();

    fixture.write("mixed.txt", "one\r\ntwo\nthree\r\n");

    let output = fixture
        .ok("read_file", json!({ "path": "mixed.txt" }))
        .await;

    assert_eq!(output["metadata"]["line_endings"], json!("mixed"));
    assert!(
        content(&output).contains("[line endings: mixed;"),
        "{}",
        content(&output)
    );
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_file_creates_and_replaces() {
    let fixture = Fixture::new();

    let created = fixture
        .ok("write_file", json!({ "path": "a.txt", "content": "first" }))
        .await;

    assert!(content(&created).contains("created"));
    assert_eq!(fixture.read("a.txt"), "first");

    let replaced = fixture
        .ok(
            "write_file",
            json!({ "path": "a.txt", "content": "second" }),
        )
        .await;

    assert!(content(&replaced).contains("replaced"));
    assert_eq!(fixture.read("a.txt"), "second");
}

#[tokio::test]
async fn write_file_requires_create_parents_for_missing_directories() {
    let fixture = Fixture::new();

    let error = fixture
        .fail(
            "write_file",
            json!({ "path": "nested/a.txt", "content": "x" }),
        )
        .await;

    assert!(error.contains("does not exist"), "{error}");
    assert!(!fixture.path("nested/a.txt").exists());

    fixture
        .ok(
            "write_file",
            json!({ "path": "nested/a.txt", "content": "x", "create_parents": true }),
        )
        .await;

    assert_eq!(fixture.read("nested/a.txt"), "x");
}

#[cfg(unix)]
#[tokio::test]
async fn write_file_preserves_the_existing_mode() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();

    fixture.write("script.sh", "#!/bin/sh\n");
    std::fs::set_permissions(
        fixture.path("script.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    fixture
        .ok(
            "write_file",
            json!({ "path": "script.sh", "content": "#!/bin/sh\necho hi\n" }),
        )
        .await;

    let mode = std::fs::metadata(fixture.path("script.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o755);
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_file_replaces_a_unique_match() {
    let fixture = Fixture::new();

    fixture.write("lib.rs", "fn main() {\n    println!(\"old\");\n}\n");

    let output = fixture
        .ok(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [{
                    "old_text": "println!(\"old\");",
                    "new_text": "println!(\"new\");"
                }]
            }),
        )
        .await;

    assert!(
        content(&output).contains("applied 1 edit (1 replacement)"),
        "{}",
        content(&output)
    );
    assert!(fixture.read("lib.rs").contains("println!(\"new\");"));
}

#[tokio::test]
async fn edit_file_refuses_ambiguous_matches_without_writing() {
    let fixture = Fixture::new();
    let original = "value();\nvalue();\n";

    fixture.write("lib.rs", original);

    let error = fixture
        .fail(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [{ "old_text": "value();", "new_text": "other();" }]
            }),
        )
        .await;

    assert!(error.contains("edits[0]:"), "{error}");
    assert!(error.contains("matches 2 times"), "{error}");
    assert!(error.contains("1, 2"), "{error}");
    assert_eq!(fixture.read("lib.rs"), original);
}

#[tokio::test]
async fn edit_file_refuses_a_missing_match_without_writing() {
    let fixture = Fixture::new();

    fixture.write("lib.rs", "fn main() {}\n");

    let error = fixture
        .fail(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [{ "old_text": "nothing like this", "new_text": "x" }]
            }),
        )
        .await;

    assert!(error.contains("edits[0]:"), "{error}");
    assert!(error.contains("no exact match"), "{error}");
    assert_eq!(fixture.read("lib.rs"), "fn main() {}\n");
}

#[tokio::test]
async fn edit_file_can_replace_every_occurrence() {
    let fixture = Fixture::new();

    fixture.write("lib.rs", "a();\na();\n");

    let output = fixture
        .ok(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [{
                    "old_text": "a();",
                    "new_text": "b();",
                    "replace_all": true
                }]
            }),
        )
        .await;

    assert!(
        content(&output).contains("applied 1 edit (2 replacements)"),
        "{}",
        content(&output)
    );
    assert_eq!(fixture.read("lib.rs"), "b();\nb();\n");
}

#[tokio::test]
async fn edit_file_counts_overlapping_matches() {
    let fixture = Fixture::new();

    fixture.write("overlap.txt", "aaa");

    let error = fixture
        .fail(
            "edit_file",
            json!({
                "path": "overlap.txt",
                "edits": [{ "old_text": "aa", "new_text": "b" }]
            }),
        )
        .await;

    assert!(error.contains("edits[0]:"), "{error}");
    assert!(error.contains("matches 2 times"), "{error}");
    assert_eq!(fixture.read("overlap.txt"), "aaa");
}

#[tokio::test]
async fn edit_file_refuses_overlapping_replace_all() {
    let fixture = Fixture::new();

    fixture.write("overlap.txt", "aaa");

    let error = fixture
        .fail(
            "edit_file",
            json!({
                "path": "overlap.txt",
                "edits": [{
                    "old_text": "aa",
                    "new_text": "b",
                    "replace_all": true
                }]
            }),
        )
        .await;

    assert!(error.contains("edits[0]:"), "{error}");
    assert!(error.contains("overlap"), "{error}");
    assert_eq!(fixture.read("overlap.txt"), "aaa");
}

#[tokio::test]
async fn edit_file_applies_several_edits_in_one_write() {
    let fixture = Fixture::new();

    fixture.write("lib.rs", "one\ntwo\nthree\n");

    let output = fixture
        .ok(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [
                    { "old_text": "one", "new_text": "1" },
                    { "old_text": "three", "new_text": "3" }
                ]
            }),
        )
        .await;

    assert!(
        content(&output).contains("applied 2 edits (2 replacements)"),
        "{}",
        content(&output)
    );
    assert_eq!(fixture.read("lib.rs"), "1\ntwo\n3\n");

    assert_eq!(output["metadata"]["edits"], serde_json::json!(2));
    assert_eq!(output["metadata"]["replacements"], serde_json::json!(2));
}

#[tokio::test]
async fn edit_file_matches_every_edit_against_the_original_text() {
    let fixture = Fixture::new();

    fixture.write("lib.rs", "a b");

    fixture
        .ok(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [
                    { "old_text": "a", "new_text": "b" },
                    { "old_text": "b", "new_text": "c" }
                ]
            }),
        )
        .await;

    assert_eq!(fixture.read("lib.rs"), "b c");
}

#[tokio::test]
async fn edit_file_applies_edits_listed_in_reverse_file_order() {
    let fixture = Fixture::new();

    fixture.write("lib.rs", "first second");

    fixture
        .ok(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [
                    { "old_text": "second", "new_text": "B" },
                    { "old_text": "first", "new_text": "A" }
                ]
            }),
        )
        .await;

    assert_eq!(fixture.read("lib.rs"), "A B");
}

#[tokio::test]
async fn a_later_bad_edit_leaves_the_file_untouched() {
    let fixture = Fixture::new();
    let original = "one two\n";

    fixture.write("lib.rs", original);

    let error = fixture
        .fail(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [
                    { "old_text": "one", "new_text": "1" },
                    { "old_text": "missing", "new_text": "2" }
                ]
            }),
        )
        .await;

    assert!(error.contains("edits[1]:"), "{error}");
    assert_eq!(fixture.read("lib.rs"), original);
}

#[tokio::test]
async fn overlapping_edits_are_rejected() {
    let fixture = Fixture::new();
    let original = "abcdef";

    fixture.write("lib.rs", original);

    let error = fixture
        .fail(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [
                    { "old_text": "abcd", "new_text": "x" },
                    { "old_text": "cdef", "new_text": "y" }
                ]
            }),
        )
        .await;

    assert!(error.contains("edits[1]:"), "{error}");
    assert!(error.contains("overlaps edits[0]"), "{error}");
    assert_eq!(fixture.read("lib.rs"), original);
}

#[tokio::test]
async fn duplicate_old_text_is_an_overlap() {
    let fixture = Fixture::new();
    let original = "same";

    fixture.write("lib.rs", original);

    let error = fixture
        .fail(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [
                    { "old_text": "same", "new_text": "a" },
                    { "old_text": "same", "new_text": "b" }
                ]
            }),
        )
        .await;

    assert!(error.contains("overlaps"), "{error}");
    assert_eq!(fixture.read("lib.rs"), original);
}

#[tokio::test]
async fn each_edit_has_its_own_replace_all_flag() {
    let fixture = Fixture::new();

    fixture.write("lib.rs", "a x x\n");

    fixture
        .ok(
            "edit_file",
            json!({
                "path": "lib.rs",
                "edits": [
                    { "old_text": "a", "new_text": "A" },
                    { "old_text": "x", "new_text": "X", "replace_all": true }
                ]
            }),
        )
        .await;

    assert_eq!(fixture.read("lib.rs"), "A X X\n");
}

#[tokio::test]
async fn an_empty_edits_array_is_rejected() {
    let fixture = Fixture::new();

    fixture.write("lib.rs", "unchanged");

    let error = fixture
        .fail("edit_file", json!({ "path": "lib.rs", "edits": [] }))
        .await;

    assert!(error.contains("at least one edit"), "{error}");
    assert_eq!(fixture.read("lib.rs"), "unchanged");
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create symlink");
}

#[cfg(unix)]
#[tokio::test]
async fn write_file_rejects_existing_and_dangling_leaf_symlinks() {
    let fixture = Fixture::new();

    fixture.write("target.txt", "original");
    make_symlink("target.txt", &fixture.path("existing-link.txt"));
    make_symlink("missing.txt", &fixture.path("dangling-link.txt"));

    let existing = fixture
        .fail(
            "write_file",
            json!({ "path": "existing-link.txt", "content": "changed" }),
        )
        .await;

    assert!(existing.contains("symbolic link"), "{existing}");
    assert_eq!(fixture.read("target.txt"), "original");
    assert!(
        std::fs::symlink_metadata(fixture.path("existing-link.txt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let dangling = fixture
        .fail(
            "write_file",
            json!({ "path": "dangling-link.txt", "content": "created" }),
        )
        .await;

    assert!(dangling.contains("symbolic link"), "{dangling}");
    assert!(!fixture.path("missing.txt").exists());
    assert!(
        std::fs::symlink_metadata(fixture.path("dangling-link.txt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn edit_file_rejects_leaf_symlinks() {
    let fixture = Fixture::new();

    fixture.write("target.txt", "old\n");
    make_symlink("target.txt", &fixture.path("link.txt"));

    let error = fixture
        .fail(
            "edit_file",
            json!({
                "path": "link.txt",
                "edits": [{ "old_text": "old", "new_text": "new" }]
            }),
        )
        .await;

    assert!(error.contains("symbolic link"), "{error}");
    assert_eq!(fixture.read("target.txt"), "old\n");

    make_symlink("missing.txt", &fixture.path("dangling.txt"));

    let dangling = fixture
        .fail(
            "edit_file",
            json!({
                "path": "dangling.txt",
                "edits": [{ "old_text": "old", "new_text": "new" }]
            }),
        )
        .await;

    assert!(dangling.contains("symbolic link"), "{dangling}");
    assert!(!fixture.path("missing.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_rejects_leaf_symlinks() {
    let fixture = Fixture::new();

    fixture.write("target.txt", "old\n");
    make_symlink("target.txt", &fixture.path("link.txt"));

    let patch = "--- a/link.txt\n+++ b/link.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let error = fixture.fail("apply_patch", json!({ "patch": patch })).await;

    assert!(error.contains("symbolic link"), "{error}");
    assert_eq!(fixture.read("target.txt"), "old\n");

    make_symlink("missing.txt", &fixture.path("dangling.txt"));

    let create = "--- /dev/null\n+++ b/dangling.txt\n@@ -0,0 +1 @@\n+created\n";
    let dangling = fixture
        .fail("apply_patch", json!({ "patch": create }))
        .await;

    assert!(dangling.contains("symbolic link"), "{dangling}");
    assert!(!fixture.path("missing.txt").exists());
}

// ---------------------------------------------------------------------------
// apply_patch
// ---------------------------------------------------------------------------

const PATCH: &str = "--- a/app.txt\n+++ b/app.txt\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+gamma\n";

#[tokio::test]
async fn apply_patch_updates_a_file() {
    let fixture = Fixture::new();

    fixture.write("app.txt", "alpha\nbeta\n");

    let output = fixture.ok("apply_patch", json!({ "patch": PATCH })).await;

    assert!(content(&output).contains("app.txt"));
    assert_eq!(fixture.read("app.txt"), "alpha\ngamma\n");
}

#[tokio::test]
async fn apply_patch_creates_new_files() {
    let fixture = Fixture::new();

    let patch = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+first\n+second\n";
    let output = fixture.ok("apply_patch", json!({ "patch": patch })).await;

    assert!(content(&output).contains("new.txt"));
    assert_eq!(fixture.read("new.txt"), "first\nsecond\n");
}

#[tokio::test]
async fn apply_patch_applies_multiple_files_together() {
    let fixture = Fixture::new();

    fixture.write("one.txt", "one\n");
    fixture.write("two.txt", "two\n");

    let patch = "--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-one\n+ONE\n\
                 --- a/two.txt\n+++ b/two.txt\n@@ -1 +1 @@\n-two\n+TWO\n";

    fixture.ok("apply_patch", json!({ "patch": patch })).await;

    assert_eq!(fixture.read("one.txt"), "ONE\n");
    assert_eq!(fixture.read("two.txt"), "TWO\n");
}

#[tokio::test]
async fn apply_patch_writes_nothing_when_any_file_conflicts() {
    let fixture = Fixture::new();

    fixture.write("one.txt", "one\n");
    fixture.write("two.txt", "something else\n");

    let patch = "--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-one\n+ONE\n\
                 --- a/two.txt\n+++ b/two.txt\n@@ -1 +1 @@\n-two\n+TWO\n";

    let error = fixture.fail("apply_patch", json!({ "patch": patch })).await;

    assert!(error.contains("no files were written"), "{error}");
    assert_eq!(fixture.read("one.txt"), "one\n");
    assert_eq!(fixture.read("two.txt"), "something else\n");
}

#[tokio::test]
async fn apply_patch_create_rejects_an_existing_destination() {
    let fixture = Fixture::new();

    fixture.write("existing.txt", "old\n");

    let patch = "--- /dev/null\n+++ b/existing.txt\n@@ -0,0 +1 @@\n+new\n";
    let error = fixture.fail("apply_patch", json!({ "patch": patch })).await;

    assert!(error.contains("already exists"), "{error}");
    assert_eq!(fixture.read("existing.txt"), "old\n");
}

#[tokio::test]
async fn apply_patch_rejects_repeated_and_aliased_targets() {
    let fixture = Fixture::new();

    fixture.write("target.txt", "old\n");

    let patch = "--- a/target.txt\n+++ b/target.txt\n@@ -1 +1 @@\n-old\n+new\n\
                 --- a/./target.txt\n+++ b/./target.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let error = fixture.fail("apply_patch", json!({ "patch": patch })).await;

    assert!(error.contains("more than once"), "{error}");
    assert_eq!(fixture.read("target.txt"), "old\n");
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_rejects_a_symlinked_parent_alias() {
    let fixture = Fixture::new();

    fixture.write("dir/f.txt", "old\n");
    std::os::unix::fs::symlink("dir", fixture.path("link")).unwrap();

    let patch = "--- a/dir/f.txt\n+++ b/dir/f.txt\n@@ -1 +1 @@\n-old\n+first\n\
                 --- a/link/f.txt\n+++ b/link/f.txt\n@@ -1 +1 @@\n-old\n+second\n";

    let error = fixture.fail("apply_patch", json!({ "patch": patch })).await;

    assert!(error.contains("more than once"), "{error}");
    assert_eq!(fixture.read("dir/f.txt"), "old\n");
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_rejects_a_hard_link_alias() {
    let fixture = Fixture::new();

    fixture.write("dir/f.txt", "old\n");
    std::fs::hard_link(fixture.path("dir/f.txt"), fixture.path("dir/hard.txt")).unwrap();

    let patch = "--- a/dir/f.txt\n+++ b/dir/f.txt\n@@ -1 +1 @@\n-old\n+first\n\
                 --- a/dir/hard.txt\n+++ b/dir/hard.txt\n@@ -1 +1 @@\n-old\n+second\n";

    let error = fixture.fail("apply_patch", json!({ "patch": patch })).await;

    assert!(error.contains("more than once"), "{error}");
    assert_eq!(fixture.read("dir/f.txt"), "old\n");
    assert_eq!(fixture.read("dir/hard.txt"), "old\n");
}

#[tokio::test]
async fn apply_patch_rejects_collisions_before_any_write() {
    let fixture = Fixture::new();

    fixture.write("one.txt", "one\n");
    fixture.write("two.txt", "already here\n");

    let patch = "--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-one\n+ONE\n\
                 --- /dev/null\n+++ b/two.txt\n@@ -0,0 +1 @@\n+new\n";
    let error = fixture.fail("apply_patch", json!({ "patch": patch })).await;

    assert!(error.contains("already exists"), "{error}");
    assert_eq!(fixture.read("one.txt"), "one\n");
    assert_eq!(fixture.read("two.txt"), "already here\n");
}

#[tokio::test]
async fn apply_patch_rejects_garbage() {
    let fixture = Fixture::new();

    let error = fixture
        .fail("apply_patch", json!({ "patch": "this is not a patch" }))
        .await;

    assert!(
        error.contains("patch") || error.contains("change"),
        "{error}"
    );
}

#[tokio::test]
async fn apply_patch_rejects_deletions() {
    let fixture = Fixture::new();

    fixture.write("gone.txt", "bye\n");

    let patch = "--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";
    let error = fixture.fail("apply_patch", json!({ "patch": patch })).await;

    assert!(error.contains("deleting"), "{error}");
    assert_eq!(fixture.read("gone.txt"), "bye\n");
}

// ---------------------------------------------------------------------------
// bash and processes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_runs_in_the_launch_directory_and_does_not_persist_state() {
    let fixture = Fixture::new();

    let pwd = fixture.ok("bash", json!({ "command": "pwd" })).await;

    assert!(
        content(&pwd).contains(&fixture.dir.path().display().to_string()),
        "{}",
        content(&pwd)
    );

    fixture
        .ok(
            "bash",
            json!({ "command": "cd /tmp && export T8FUL_LEAK=1" }),
        )
        .await;

    let check = fixture
        .ok(
            "bash",
            json!({ "command": "echo \"${T8FUL_LEAK:-unset}\"; pwd" }),
        )
        .await;

    assert!(content(&check).contains("unset"));
    assert!(content(&check).contains(&fixture.dir.path().display().to_string()));
}

#[tokio::test]
async fn bash_inherits_path_and_reports_exit_codes() {
    let fixture = Fixture::new();

    let path = fixture.ok("bash", json!({ "command": "echo $PATH" })).await;

    assert!(content(&path).contains('/'));

    let failure = fixture
        .ok("bash", json!({ "command": "echo boom >&2; exit 7" }))
        .await;

    assert!(
        content(&failure).contains("[exit code 7"),
        "{}",
        content(&failure)
    );
    assert!(content(&failure).contains("boom"));
    assert_eq!(failure["metadata"]["exit_code"], json!(7));
}

#[tokio::test]
async fn bash_withholds_known_credentials() {
    let fixture = Fixture::new();

    let parent_key = std::env::var("OPENROUTER_API_KEY").ok();
    let command =
        "printf '%s|%s' \"${OPENROUTER_API_KEY:-missing}\" \"${T8FUL_TEST_SECRET:-missing}\"";
    let output = fixture.ok("bash", json!({ "command": command })).await;
    let rendered = content(&output);

    assert!(rendered.contains("missing|missing"), "{rendered}");

    if let Some(key) = parent_key.filter(|key| !key.is_empty()) {
        assert!(
            !rendered.contains(&key),
            "the provider key reached the shell"
        );
    }
}

#[tokio::test]
async fn bash_reports_truncation() {
    let fixture = Fixture::with_capacity(64);

    let output = fixture
        .ok(
            "bash",
            json!({ "command": "for i in $(seq 1 200); do echo line-$i; done" }),
        )
        .await;

    assert_eq!(output["truncated"], json!(true));
    assert!(
        content(&output).contains("[output truncated:"),
        "{}",
        content(&output)
    );
}

#[tokio::test]
async fn bash_times_out_and_terminates_the_command() {
    let fixture = Fixture::new();

    let output = fixture
        .ok("bash", json!({ "command": "sleep 30", "timeout_ms": 150 }))
        .await;

    assert_eq!(output["metadata"]["timed_out"], json!(true));
    assert!(content(&output).contains("[timed out after"));
}

#[tokio::test]
async fn cancelling_a_foreground_command_kills_its_process_group() {
    let fixture = Fixture::new();
    let pid_file = fixture.path("pid");

    let tool = fixture.registry.get("bash").expect("bash tool");
    let call = ToolCall::new(
        "call-1",
        "bash",
        json!({ "command": format!("echo $$ > {}; sleep 30", pid_file.display()) }),
    );
    let context = fixture.context.clone();

    let cancelled =
        tokio::time::timeout(Duration::from_millis(400), tool.execute(&call, &context)).await;

    assert!(cancelled.is_err(), "the command should still be running");

    // `kill_on_drop` must have terminated the process.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let pid = std::fs::read_to_string(&pid_file).expect("pid file");
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run kill")
        .success();

    assert!(!alive, "process {} survived cancellation", pid.trim());
}

#[tokio::test]
async fn background_processes_can_be_inspected_and_stopped() {
    let fixture = Fixture::new();
    let pid_file = fixture.path("bg-pid");

    let started = fixture
        .ok(
            "bash",
            json!({
                "command": format!("echo $$ > {}; sleep 30", pid_file.display()),
                "background": true
            }),
        )
        .await;

    let process_id = started["metadata"]["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let status = fixture
        .ok("process_status", json!({ "process_id": process_id }))
        .await;

    assert!(content(&status).contains("running"), "{}", content(&status));

    let listed = fixture.ok("process_list", json!({})).await;

    assert!(content(&listed).contains(&process_id));

    let output = fixture
        .ok("process_output", json!({ "process_id": process_id }))
        .await;

    assert_eq!(output["truncated"], json!(false));

    let stopped = fixture
        .ok("process_stop", json!({ "process_id": process_id }))
        .await;

    assert!(
        !content(&stopped).contains("running"),
        "{}",
        content(&stopped)
    );
    assert!(
        content(&stopped).contains("setsid"),
        "a clean stop must report the detached-descendant limitation: {}",
        content(&stopped)
    );

    let pid = std::fs::read_to_string(&pid_file).expect("pid file");
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run kill")
        .success();

    assert!(!alive, "background process survived stop");

    fixture.processes.shutdown().await;
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
async fn wait_for_pid_file(path: &Path) -> String {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path) {
            let pid = text.trim().to_owned();

            if !pid.is_empty() {
                return pid;
            }
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("pid file {} was never written", path.display());
}

#[cfg(target_os = "linux")]
async fn wait_until_dead(pid: &str) {
    for _ in 0..200 {
        if !pid_alive(pid) {
            return;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    panic!("process {pid} is still alive");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_foreground_cancellation_kills_descendants() {
    let fixture = Fixture::new();
    let leader_file = fixture.path("leader.pid");
    let child_file = fixture.path("child.pid");

    let tool = fixture.registry.get("bash").expect("bash tool");
    let call = ToolCall::new(
        "call-1",
        "bash",
        json!({
            "command": format!(
                "echo $$ > {}; sleep 30 & echo $! > {}; wait",
                leader_file.display(),
                child_file.display()
            )
        }),
    );
    let context = fixture.context.clone();

    let cancelled =
        tokio::time::timeout(Duration::from_millis(400), tool.execute(&call, &context)).await;

    assert!(cancelled.is_err(), "the command should still be running");

    let leader = wait_for_pid_file(&leader_file).await;
    let child = wait_for_pid_file(&child_file).await;

    wait_until_dead(&leader).await;
    wait_until_dead(&child).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_timeout_kills_a_child_that_holds_stdout() {
    let fixture = Fixture::new();
    let child_file = fixture.path("timeout-child.pid");

    let output = fixture
        .ok(
            "bash",
            json!({
                "command": format!(
                    "sleep 30 & echo $! > {}; wait",
                    child_file.display()
                ),
                "timeout_ms": 200
            }),
        )
        .await;

    assert_eq!(output["metadata"]["timed_out"], json!(true));
    assert!(
        content(&output).contains("[timed out after"),
        "{}",
        content(&output)
    );

    let child = wait_for_pid_file(&child_file).await;

    wait_until_dead(&child).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_normal_completion_kills_a_descendant_that_holds_stdout() {
    let fixture = Fixture::new();
    let child_file = fixture.path("normal-child.pid");

    let output = tokio::time::timeout(
        Duration::from_secs(3),
        fixture.ok(
            "bash",
            json!({
                "command": format!(
                    "sleep 30 & echo $! > {}; exit 0",
                    child_file.display()
                )
            }),
        ),
    )
    .await
    .expect("a descendant must not keep the call open");

    assert!(
        content(&output).contains("[exit code 0"),
        "{}",
        content(&output)
    );

    let child = wait_for_pid_file(&child_file).await;

    wait_until_dead(&child).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_background_cleanup_finishes_after_the_leader_exits() {
    let fixture = Fixture::new();
    let child_file = fixture.path("background-child.pid");

    let started = fixture
        .ok(
            "bash",
            json!({
                "command": format!(
                    "sleep 30 > /dev/null 2>&1 & echo $! > {}; echo leader done",
                    child_file.display()
                ),
                "background": true
            }),
        )
        .await;
    let process_id = started["metadata"]["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();
    let child = wait_for_pid_file(&child_file).await;

    let mut state = String::new();

    for _ in 0..200 {
        let status = fixture
            .ok("process_status", json!({ "process_id": process_id }))
            .await;

        state = content(&status).to_owned();

        if state.contains("exited") {
            break;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(state.contains("exited"), "{state}");

    wait_until_dead(&child).await;

    let stopped = fixture
        .ok("process_stop", json!({ "process_id": process_id }))
        .await;

    assert!(
        !content(&stopped).contains("running"),
        "{}",
        content(&stopped)
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_term_resistant_children_are_killed() {
    let fixture = Fixture::new();
    let child_file = fixture.path("stubborn.pid");

    let started = std::time::Instant::now();
    let output = fixture
        .ok(
            "bash",
            json!({
                "command": format!(
                    "bash -c 'trap \"\" TERM; while :; do sleep 0.1; done' & \
                     echo $! > {}; wait",
                    child_file.display()
                ),
                "timeout_ms": 150
            }),
        )
        .await;

    assert_eq!(output["metadata"]["timed_out"], json!(true));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cleanup took {:?}",
        started.elapsed()
    );

    let child = wait_for_pid_file(&child_file).await;

    wait_until_dead(&child).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_cleanup_does_not_signal_an_unrelated_process_group() {
    let fixture = Fixture::new();
    let mut unrelated = std::process::Command::new("bash")
        .args(["-c", "set -m; exec sleep 5"])
        .spawn()
        .expect("start unrelated process group");
    let unrelated_pid = unrelated.id();

    tokio::time::sleep(Duration::from_millis(100)).await;

    fixture.ok("bash", json!({ "command": "true" })).await;

    assert!(
        pid_alive(&unrelated_pid.to_string()),
        "cleanup signalled an unrelated process group"
    );

    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn process_wait_reports_an_exit_with_its_output_tail() {
    let fixture = Fixture::new();
    let started = fixture
        .ok(
            "bash",
            json!({ "command": "sleep 0.1; printf 'tail-marker'", "background": true }),
        )
        .await;
    let process_id = started["metadata"]["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();

    let waited = fixture
        .ok(
            "process_wait",
            json!({ "process_id": process_id, "timeout_s": 20 }),
        )
        .await;
    let text = content(&waited);

    assert!(text.contains("exited"), "{text}");
    assert!(text.contains("exit code 0"), "{text}");
    assert!(text.contains("tail-marker"), "{text}");
    assert!(
        !text.contains("setsid"),
        "a natural exit is not a stop: {text}"
    );
    assert_eq!(waited["metadata"]["timed_out"], json!(false));
    assert!(waited["metadata"]["waited_ms"].is_number(), "{waited}");

    fixture.processes.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_process_wait_timeout_reports_the_process_still_running() {
    let fixture = Fixture::new();
    let started = fixture
        .ok("bash", json!({ "command": "sleep 30", "background": true }))
        .await;
    let process_id = started["metadata"]["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();

    let waited = fixture
        .ok(
            "process_wait",
            json!({ "process_id": process_id, "timeout_s": 1 }),
        )
        .await;
    let text = content(&waited);

    assert!(text.contains("still running after 1 s"), "{text}");
    assert!(text.contains("process_wait again"), "{text}");
    assert_eq!(waited["metadata"]["timed_out"], json!(true));

    let stopped = fixture
        .ok("process_stop", json!({ "process_id": process_id }))
        .await;

    assert!(!content(&stopped).contains("running"), "{stopped}");

    fixture.processes.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_stop_is_idempotent() {
    let fixture = Fixture::new();

    let started = fixture
        .ok("bash", json!({ "command": "sleep 30", "background": true }))
        .await;
    let process_id = started["metadata"]["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();

    let first = fixture
        .ok("process_stop", json!({ "process_id": process_id }))
        .await;
    let second = fixture
        .ok("process_stop", json!({ "process_id": process_id }))
        .await;

    assert!(!content(&first).contains("running"), "{}", content(&first));
    assert!(
        !content(&second).contains("running"),
        "{}",
        content(&second)
    );
}

#[tokio::test]
async fn unknown_process_ids_are_reported() {
    let fixture = Fixture::new();

    for tool in [
        "process_status",
        "process_output",
        "process_stop",
        "process_wait",
    ] {
        let error = fixture
            .fail(tool, json!({ "process_id": "process-999" }))
            .await;

        assert!(error.contains("unknown process id"), "{tool}: {error}");
    }
}

#[tokio::test]
async fn invalid_arguments_are_rejected() {
    let fixture = Fixture::new();

    let error = fixture.fail("read_file", json!({ "path": 42 })).await;

    assert!(error.contains("invalid arguments"), "{error}");
}

#[tokio::test]
async fn presenting_a_plan_reports_that_it_neither_switches_modes_nor_approves_it() {
    let fixture = Fixture::new();
    let output = fixture
        .ok("present_plan", json!({ "plan": "# Plan\n\nDo the thing." }))
        .await;
    let content = output["content"].as_str().expect("content");

    assert!(content.contains("stored for review"), "{content}");
    assert!(content.contains("did not change the mode"), "{content}");
    assert!(content.contains("did not approve the plan"), "{content}");
    assert!(
        content.contains("next message is the instruction"),
        "{content}"
    );
    assert_eq!(output["metadata"], json!({ "present_plan": true }));

    let error = fixture.fail("present_plan", json!({ "plan": "   " })).await;

    assert!(error.contains("must not be empty"), "{error}");
}

// Keeps the unused import warning away for the status helper used in agent tests.
#[allow(dead_code)]
fn _status_is_public(status: ToolStatus) -> ToolStatus {
    status
}

#[allow(dead_code)]
fn _path_helper(path: &Path) -> bool {
    path.exists()
}

#[tokio::test]
async fn plan_reads_are_not_confined_to_the_mount_roots() {
    let mut fixture = Fixture::new();
    let outside = tempfile::tempdir().expect("outside");
    let outside_file = outside.path().join("secret.txt");

    std::fs::write(&outside_file, "secret").unwrap();
    fixture.context.mode = t8ful_core::policy::Mode::Plan;
    fixture.context.read_scope = Some(t8ful_core::readscope::ReadScope::new([fixture
        .dir
        .path()
        .to_path_buf()]));
    let tool = fixture.registry.get("read_file").expect("read tool");

    let call = ToolCall::new(
        "call-outside",
        "read_file",
        json!({ "path": outside_file.display().to_string() }),
    );
    let output = tool
        .execute(&call, &fixture.context)
        .await
        .expect("outside read is allowed");

    assert!(output.content.contains("secret"));

    #[cfg(unix)]
    {
        let link = fixture.dir.path().join("escape.txt");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        let call = ToolCall::new("call-link", "read_file", json!({ "path": "escape.txt" }));
        let output = tool
            .execute(&call, &fixture.context)
            .await
            .expect("symlink read is allowed");

        assert!(output.content.contains("secret"));
    }

    std::fs::write(fixture.dir.path().join("inside.txt"), "inside").unwrap();

    let call = ToolCall::new("call-inside", "read_file", json!({ "path": "inside.txt" }));
    let output = tool.execute(&call, &fixture.context).await.unwrap();

    assert!(output.content.contains("inside"));
}

#[test]
fn bash_schema_has_no_risk_or_read_only_paths() {
    let fixture = Fixture::new();
    let tool = fixture.registry.get("bash").expect("bash tool");
    let properties = &tool.spec().parameters["properties"];

    assert!(properties.get("risk").is_none());
    assert!(properties.get("read_only_paths").is_none());
}

/// Runs the command for real, without isolation. Only used to check what the
/// tool reports; the boundary itself is tested against Bubblewrap in
/// `crates/t8ful-sandbox/tests/bwrap.rs`.
struct FakeSandbox;

#[async_trait::async_trait]
impl t8ful_core::sandbox::Sandbox for FakeSandbox {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn spawn(
        &self,
        command: t8ful_core::sandbox::SandboxedCommand,
    ) -> Result<t8ful_core::sandbox::SandboxedProcess, t8ful_core::sandbox::SandboxError> {
        let mut process = tokio::process::Command::new(&command.program);

        process
            .args(&command.args)
            .current_dir(&command.cwd)
            .env_clear()
            .envs(&command.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // The real Bubblewrap backend starts the command as its own process
        // group leader, which is what plan-mode cleanup relies on.
        #[cfg(unix)]
        process.process_group(0);

        let child = process
            .spawn()
            .map_err(|error| t8ful_core::sandbox::SandboxError::Launch(error.to_string()))?;

        Ok(t8ful_core::sandbox::SandboxedProcess { child })
    }
}

#[tokio::test]
async fn plan_shell_reports_that_the_command_was_sandboxed() {
    let mut fixture = Fixture::new();
    fixture.context.mode = t8ful_core::policy::Mode::Plan;
    fixture.context.sandbox = Some(Arc::new(FakeSandbox));
    fixture.context.read_scope = Some(t8ful_core::readscope::ReadScope::new([fixture
        .dir
        .path()
        .to_path_buf()]));
    let tool = fixture.registry.get("bash").expect("bash tool");
    let call = ToolCall::new("call-sandboxed", "bash", json!({ "command": "echo hello" }));
    let output = tool
        .execute(&call, &fixture.context)
        .await
        .expect("sandboxed run");

    assert!(output.content.contains("hello"), "{}", output.content);
    assert!(output.content.contains("sandbox"), "{}", output.content);
    assert!(
        output.content.contains("host is readable and read-only"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("private /tmp"),
        "{}",
        output.content
    );
    assert!(output.content.contains("discarded"), "{}", output.content);
}

#[tokio::test]
async fn plan_shell_streams_progress_before_the_command_exits() {
    let mut fixture = Fixture::new();
    fixture.context.mode = t8ful_core::policy::Mode::Plan;
    fixture.context.sandbox = Some(Arc::new(FakeSandbox));
    fixture.context.read_scope = Some(t8ful_core::readscope::ReadScope::new([fixture
        .dir
        .path()
        .to_path_buf()]));
    let gate = fixture.path("plan-release");
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = Arc::clone(&seen);

    fixture.context.progress = Some(t8ful_core::tool::ProgressReporter::new(move |bytes| {
        sink.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_str(&String::from_utf8_lossy(bytes));
    }));

    let tool = fixture.registry.get("bash").expect("bash tool").clone();
    let context = fixture.context.clone();
    let command = format!(
        "printf 'sandbox-live\\n'; while [ ! -e '{}' ]; do sleep 0.02; done; printf 'sandbox-done\\n'",
        gate.display()
    );
    let call = ToolCall::new("call-progress", "bash", json!({ "command": command }));
    let handle = tokio::spawn(async move { tool.execute(&call, &context).await });

    let mut observed = String::new();

    for _ in 0..250 {
        observed = seen
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();

        if observed.contains("sandbox-live") {
            break;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(observed.contains("sandbox-live"), "{observed}");
    assert!(
        !handle.is_finished(),
        "the command exited before the gate was released: {observed}"
    );

    std::fs::write(&gate, "go").expect("release the command");

    let output = handle.await.expect("task joins").expect("sandboxed run");

    assert!(
        output.content.contains("sandbox-done"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn coding_shell_streams_progress_before_the_command_exits() {
    let mut fixture = Fixture::new();
    let gate = fixture.path("coding-release");
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = Arc::clone(&seen);

    fixture.context.progress = Some(t8ful_core::tool::ProgressReporter::new(move |bytes| {
        sink.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_str(&String::from_utf8_lossy(bytes));
    }));

    let tool = fixture.registry.get("bash").expect("bash tool").clone();
    let context = fixture.context.clone();
    let command = format!(
        "printf 'live-output\\n'; while [ ! -e '{}' ]; do sleep 0.02; done; printf 'done\\n'",
        gate.display()
    );
    let call = ToolCall::new("call-progress", "bash", json!({ "command": command }));
    let handle = tokio::spawn(async move { tool.execute(&call, &context).await });

    let mut observed = String::new();

    for _ in 0..250 {
        observed = seen
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();

        if observed.contains("live-output") {
            break;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(observed.contains("live-output"), "{observed}");
    assert!(
        !handle.is_finished(),
        "the command exited before the gate was released: {observed}"
    );

    std::fs::write(&gate, "go").expect("release the command");

    let output = handle.await.expect("task joins").expect("foreground run");

    assert!(output.content.contains("done"), "{}", output.content);
}

#[tokio::test]
async fn plan_shell_timeout_stops_the_whole_process_group() {
    let mut fixture = Fixture::new();
    fixture.context.mode = t8ful_core::policy::Mode::Plan;
    fixture.context.sandbox = Some(Arc::new(FakeSandbox));
    fixture.context.read_scope = Some(t8ful_core::readscope::ReadScope::new([fixture
        .dir
        .path()
        .to_path_buf()]));
    let tool = fixture.registry.get("bash").expect("bash tool");
    let call = ToolCall::new(
        "call-timeout",
        "bash",
        json!({ "command": "printf 'started\\n'; sleep 5", "timeout_ms": 200 }),
    );
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        tool.execute(&call, &fixture.context),
    )
    .await
    .expect("a plan-mode timeout must not wait for the command")
    .expect("sandboxed run");

    assert_eq!(output.metadata.as_ref().unwrap()["timed_out"], json!(true));
    assert!(
        output.content.contains("[timed out after"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("started"),
        "output read before the timeout must survive: {}",
        output.content
    );
    // The group died with the command, so both pipes reached EOF.
    assert_eq!(
        output.metadata.as_ref().unwrap()["output_incomplete"],
        json!(false)
    );
}

#[tokio::test]
async fn plan_shell_bounds_the_drain_when_a_descendant_holds_the_pipe() {
    let mut fixture = Fixture::new();
    fixture.context.mode = t8ful_core::policy::Mode::Plan;
    fixture.context.sandbox = Some(Arc::new(FakeSandbox));
    fixture.context.read_scope = Some(t8ful_core::readscope::ReadScope::new([fixture
        .dir
        .path()
        .to_path_buf()]));
    let tool = fixture.registry.get("bash").expect("bash tool");
    let call = ToolCall::new(
        "call-lingering",
        "bash",
        json!({ "command": "sleep 2 & printf 'kept\\n'" }),
    );
    let output = tokio::time::timeout(
        Duration::from_secs(1),
        tool.execute(&call, &fixture.context),
    )
    .await
    .expect("the tool must not wait for a descendant holding the output pipe")
    .expect("sandboxed run");

    assert!(output.content.contains("kept"), "{}", output.content);
    assert_eq!(
        output.metadata.as_ref().unwrap()["output_incomplete"],
        json!(true)
    );
    assert!(
        output.content.contains("output may be incomplete"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn plan_shell_explains_a_missing_command() {
    let mut fixture = Fixture::new();
    fixture.context.mode = t8ful_core::policy::Mode::Plan;
    fixture.context.sandbox = Some(Arc::new(FakeSandbox));
    fixture.context.read_scope = Some(t8ful_core::readscope::ReadScope::new([fixture
        .dir
        .path()
        .to_path_buf()]));
    let tool = fixture.registry.get("bash").expect("bash tool");
    let call = ToolCall::new(
        "call-missing",
        "bash",
        json!({ "command": "t8ful-definitely-not-on-path" }),
    );
    let output = tool
        .execute(&call, &fixture.context)
        .await
        .expect("sandboxed run");

    assert!(
        output
            .content
            .contains("[plan mode: the command is not on PATH]"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn plan_shell_cwd_must_stay_inside_the_mounts() {
    let mut fixture = Fixture::new();
    let outside = tempfile::tempdir().expect("outside");

    fixture.context.mode = t8ful_core::policy::Mode::Plan;
    fixture.context.sandbox = Some(Arc::new(FakeSandbox));
    fixture.context.read_scope = Some(t8ful_core::readscope::ReadScope::new([fixture
        .dir
        .path()
        .to_path_buf()]));
    let tool = fixture.registry.get("bash").expect("bash tool");
    let call = ToolCall::new(
        "call-bash",
        "bash",
        json!({
            "command": "pwd",
            "cwd": outside.path().display().to_string()
        }),
    );
    let error = tool
        .execute(&call, &fixture.context)
        .await
        .expect_err("cwd outside the mounts");

    assert!(
        error
            .to_string()
            .contains("outside the plan-mode read boundary"),
        "{error}"
    );
}

#[tokio::test]
async fn plan_shell_without_a_sandbox_fails_closed() {
    let mut fixture = Fixture::new();
    fixture.context.mode = t8ful_core::policy::Mode::Plan;
    fixture.context.sandbox = None;
    let tool = fixture.registry.get("bash").expect("bash tool");
    let call = ToolCall::new(
        "call-bash",
        "bash",
        json!({ "command": "echo escaped > host-write.txt" }),
    );
    let error = tool
        .execute(&call, &fixture.context)
        .await
        .expect_err("sandbox missing");

    assert!(error.to_string().contains("sandbox"), "{error}");
    assert!(!fixture.dir.path().join("host-write.txt").exists());
}

#[tokio::test]
async fn questions_report_input_required_without_a_router() {
    let fixture = Fixture::new();
    let tool = fixture
        .registry
        .get("ask_user_question")
        .expect("question tool");
    let call = ToolCall::new(
        "call-question",
        "ask_user_question",
        serde_json::json!({
            "questions": [{
                "header": "Scope",
                "question": "Which file?",
                "options": [
                    { "label": "a.rs", "description": "first" },
                    { "label": "b.rs", "description": "second" }
                ]
            }]
        }),
    );
    let error = tool
        .execute(&call, &fixture.context)
        .await
        .expect_err("no UI");

    assert!(
        matches!(error, t8ful_core::ToolError::InputRequired(_)),
        "{error}"
    );
}

struct CaptureRouter {
    request: std::sync::Mutex<Option<t8ful_core::QuestionRequest>>,
    answers: Vec<t8ful_core::QuestionAnswer>,
}

#[async_trait::async_trait]
impl t8ful_core::QuestionRouter for CaptureRouter {
    async fn ask(&self, request: t8ful_core::QuestionRequest) -> t8ful_core::QuestionOutcome {
        *self.request.lock().expect("request lock") = Some(request);

        t8ful_core::QuestionOutcome::Answered(self.answers.clone())
    }
}

fn capturing_router(selected: Vec<usize>, custom: Option<&str>) -> Arc<CaptureRouter> {
    Arc::new(CaptureRouter {
        request: std::sync::Mutex::new(None),
        answers: vec![t8ful_core::QuestionAnswer {
            header: "Scope".to_owned(),
            question: "Which files?".to_owned(),
            selected,
            custom: custom.map(str::to_owned),
        }],
    })
}

#[tokio::test]
async fn questions_capture_multi_select_and_report_every_choice() {
    let mut fixture = Fixture::new();
    let router = capturing_router(vec![0, 1], Some("and benches"));

    fixture.context.questions = Some(router.clone());

    let output = fixture
        .ok(
            "ask_user_question",
            json!({
                "questions": [{
                    "header": "Scope",
                    "question": "Which files?",
                    "multiSelect": true,
                    "options": [
                        { "label": "a.rs", "description": "first" },
                        { "label": "b.rs", "description": "second" }
                    ]
                }]
            }),
        )
        .await;

    let request = router
        .request
        .lock()
        .expect("request lock")
        .clone()
        .expect("request");

    assert!(request.questions[0].multi_select);
    assert_eq!(output["content"], "Scope: a.rs, b.rs; wrote: and benches");
}

#[tokio::test]
async fn question_previews_are_carried_for_single_select() {
    let mut fixture = Fixture::new();
    let router = capturing_router(vec![1], None);

    fixture.context.questions = Some(router.clone());

    let output = fixture
        .ok(
            "ask_user_question",
            json!({
                "questions": [{
                    "header": "Scope",
                    "question": "Which files?",
                    "options": [
                        { "label": "a.rs", "preview": "# first" },
                        { "label": "b.rs" }
                    ]
                }]
            }),
        )
        .await;

    let request = router
        .request
        .lock()
        .expect("request lock")
        .clone()
        .expect("request");

    assert_eq!(
        request.questions[0].options[0].preview.as_deref(),
        Some("# first")
    );
    assert_eq!(output["content"], "Scope: b.rs");
}

#[tokio::test]
async fn a_custom_only_answer_keeps_the_custom_suffix() {
    let mut fixture = Fixture::new();

    fixture.context.questions = Some(capturing_router(Vec::new(), Some("hand written")));

    let output = fixture
        .ok(
            "ask_user_question",
            json!({
                "questions": [{
                    "header": "Scope",
                    "question": "Which files?",
                    "options": [
                        { "label": "a.rs" },
                        { "label": "b.rs" }
                    ]
                }]
            }),
        )
        .await;

    assert_eq!(output["content"], "Scope: hand written (custom)");
}

#[tokio::test]
async fn previews_with_multi_select_are_rejected() {
    let fixture = Fixture::new();
    let error = fixture
        .fail(
            "ask_user_question",
            json!({
                "questions": [{
                    "header": "Scope",
                    "question": "Which files?",
                    "multiSelect": true,
                    "options": [
                        { "label": "a.rs", "preview": "# first" },
                        { "label": "b.rs" }
                    ]
                }]
            }),
        )
        .await;

    assert!(error.contains("single-select"), "{error}");
}

#[tokio::test]
async fn five_questions_are_rejected() {
    let fixture = Fixture::new();
    let question = |header: &str| {
        json!({
            "header": header,
            "question": "Which file?",
            "options": [
                { "label": "a.rs" },
                { "label": "b.rs" }
            ]
        })
    };
    let error = fixture
        .fail(
            "ask_user_question",
            json!({
                "questions": [
                    question("One"),
                    question("Two"),
                    question("Three"),
                    question("Four"),
                    question("Five")
                ]
            }),
        )
        .await;

    assert!(error.contains("one to four"), "{error}");
}
