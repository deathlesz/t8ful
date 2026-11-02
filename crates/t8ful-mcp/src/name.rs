//! Mapping between a server's tool name and the name the model sees.
//!
//! MCP tool names are chosen by the server and may contain characters that an
//! OpenAI-compatible function name cannot carry, and two servers may offer the
//! same tool. Every model-visible name is therefore `mcp__<server>__<tool>`,
//! sanitised to `[A-Za-z0-9_-]`, and the hub keeps the reverse mapping so a
//! `tools/call` always sends the server's own spelling.

/// Separator between the namespace and the server name, and between the server
/// name and the tool name.
pub const SEPARATOR: &str = "__";

/// Prefix every MCP-derived tool name carries.
pub const NAMESPACE: &str = "mcp";

/// Longest model-visible tool name.
///
/// OpenAI-compatible function names are capped at 64 characters, and the
/// OpenRouter proxies in front of several providers enforce it.
pub const MAX_NAME_LEN: usize = 64;

/// Characters that may appear in a model-visible tool name.
fn is_allowed(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Replace every byte outside `[A-Za-z0-9_-]` with `_`.
///
/// Sanitising is byte-wise rather than character-wise so a multi-byte character
/// becomes one `_` per byte: the result is always ASCII and therefore always a
/// valid function name.
pub fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());

    for byte in raw.bytes() {
        out.push(if is_allowed(byte) {
            char::from(byte)
        } else {
            '_'
        });
    }

    out
}

/// Whether `name` is an MCP-derived tool name.
pub fn is_mcp_name(name: &str) -> bool {
    name.starts_with("mcp__") && name.len() > 4
}

/// A stable, short hex digest of the original name.
///
/// FNV-1a is enough here: the digest only has to distinguish two names that
/// collided after truncation, and it must not vary between runs or processes.
fn short_hash(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;

    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    format!("{:06x}", hash & 0x00ff_ffff)
}

/// Build the model-visible name for one server tool.
///
/// The tool part is truncated when the whole name would exceed
/// [`MAX_NAME_LEN`], and a short hash of the original tool name is appended so
/// two long names sharing a prefix stay distinct. A server name long enough to
/// leave no room is truncated too, because the cap is the provider's limit and
/// not a preference.
pub fn model_name(server: &str, tool: &str) -> String {
    let server = sanitize(server);
    let tool = sanitize(tool);
    let full = format!("{NAMESPACE}{SEPARATOR}{server}{SEPARATOR}{tool}");

    if full.len() <= MAX_NAME_LEN {
        return full;
    }

    let suffix = short_hash(&tool);
    // Reserve the namespace, the two separators, the joining underscore, and the
    // hash. Whatever is left is shared between the server and tool parts, with
    // the tool part taking precedence: the server name is what a user configured
    // and recognises, while the tool part is what tells two tools apart.
    let fixed = NAMESPACE.len() + SEPARATOR.len() * 2 + 1 + suffix.len();
    let budget = MAX_NAME_LEN.saturating_sub(fixed);
    // The server name is kept whole whenever it fits in a third of what is left,
    // so a normal server stays recognisable; only a pathological name is cut,
    // and even then the tool part keeps the larger share because that is what
    // tells two tools apart.
    let server_keep = server.len().min(budget / 3);
    let tool_budget = budget - server_keep;

    format!(
        "{NAMESPACE}{SEPARATOR}{}{SEPARATOR}{}_{suffix}",
        &server[..server_keep],
        &tool[..tool_budget.min(tool.len())],
    )
}

/// Split a model-visible name back into its server and tool components.
///
/// Returns `None` for a name that is not in the `mcp__` namespace. The result is
/// only the sanitised spelling: the hub's reverse map holds the original tool
/// name, which this function cannot reconstruct.
pub fn split_model_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once(SEPARATOR)?;

    if server.is_empty() || tool.is_empty() {
        return None;
    }

    Some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::{MAX_NAME_LEN, is_mcp_name, model_name, sanitize, split_model_name};

    #[test]
    fn names_are_namespaced_and_sanitised() {
        for (server, tool, expected) in [
            ("github", "create_issue", "mcp__github__create_issue"),
            ("my server", "list/files", "mcp__my_server__list_files"),
            ("fs", "read.file", "mcp__fs__read_file"),
            ("a-b_c", "x", "mcp__a-b_c__x"),
        ] {
            assert_eq!(model_name(server, tool), expected, "{server}/{tool}");
        }
    }

    #[test]
    fn sanitising_is_ascii_and_never_drops_a_byte() {
        assert_eq!(sanitize("héllo"), "h__llo");
        assert_eq!(sanitize("a b\tc"), "a_b_c");
        assert!(sanitize("日本語").is_ascii());
    }

    #[test]
    fn a_long_name_is_capped_and_still_distinct() {
        let prefix = "t".repeat(80);
        let left = model_name("server", &format!("{prefix}_alpha"));
        let right = model_name("server", &format!("{prefix}_beta"));

        assert_eq!(left.len(), MAX_NAME_LEN, "{left}");
        assert_eq!(right.len(), MAX_NAME_LEN, "{right}");
        assert_ne!(left, right, "the hash suffix must separate them");
        assert!(left.starts_with("mcp__server__"));
    }

    #[test]
    fn a_long_server_name_is_cut_to_leave_room_for_the_tool() {
        let name = model_name(&"s".repeat(70), "tool");

        assert!(name.len() <= MAX_NAME_LEN, "{name}");
        assert!(name.starts_with("mcp__"), "{name}");
        assert!(name.contains("tool"), "{name}");
    }

    #[test]
    fn the_hash_suffix_is_stable_across_calls() {
        let tool = "x".repeat(200);

        assert_eq!(model_name("s", &tool), model_name("s", &tool));
    }

    #[test]
    fn splitting_returns_both_components() {
        assert_eq!(
            split_model_name("mcp__github__create_issue"),
            Some(("github", "create_issue"))
        );
        assert_eq!(split_model_name("read_file"), None);
        assert_eq!(split_model_name("mcp__"), None);
        assert_eq!(split_model_name("mcp__server"), None);
    }

    #[test]
    fn the_namespace_test_matches_the_generated_prefix() {
        assert!(is_mcp_name(&model_name("s", "t")));
        assert!(!is_mcp_name("bash"));
    }
}
