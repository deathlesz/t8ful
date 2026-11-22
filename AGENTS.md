# AGENTS.md

## Overview

t8ful is a Linux-first Rust coding agent (version 1.1.1) with a full-screen TUI, a headless CLI, and two providers (OpenRouter and OpenCode Go) behind one merged catalog.

- [docs/design.md](docs/design.md) is the authoritative behavior specification.
- [README.md](README.md) is the user-facing introduction.
- [docs/nix-modules.md](docs/nix-modules.md) documents the NixOS and Home Manager modules.

The 1.1.1 feature set is complete (coding and plan modes, skills, MCP clients, Nix modules, session recall, per-tool toggles, task graph, subagents, and worktrees). Ongoing hardening and performance work must preserve the contracts in `docs/design.md`. Start new implementation work only when the user explicitly asks for it.

## Working rules

- **Preserve user work.** Never commit, stash, reset, merge, or revert anything without permission. Temporary Git repos created by tests and worktrees created for isolated child workers are fine.
- **Keep crate boundaries clean.** Core, provider, tools, sandbox, and UI stay separable. Provider and tool crates must never depend on `t8ful-tui`.
- **Dependencies.** Use the latest non-conflicting version, declare it in `[workspace.dependencies]`, and opt in per crate with `.workspace = true`.
- **Code style.** Write straightforward, idiomatic Rust with blank lines between logical code blocks. Propagate meaningful errors. `unsafe_code` is forbidden workspace-wide — raise it before relaxing the lint instead of adding a local `allow`.
- **Nix builds.** `result` is an ignored symlink into the Nix store; pass `--no-link` instead of rewiring it.

## Workspace layout

Rust edition 2024, resolver 3.

| Crate | Contents |
| --- | --- |
| `t8ful-core` | Conversation model, `Provider`/`Tool` traits, `Policy`, `AgentEvent`, agent loop, context budgeting, journal and usage records, question/sandbox interfaces, task graph (`task`), child-agent identity/limits/registry/notices (`subagent`), and worktree interface (`worktree`). |
| `t8ful-openai` | OpenAI-compatible transport shared by both providers: SSE decoder, fragmented tool-call assembly, HTTP/TLS client, prompt serialization, usage and error mapping, `test_support` loopback server. |
| `t8ful-openrouter` | OpenRouter catalog, attribution headers, and `reasoning` request shape. |
| `t8ful-opencode` | OpenCode Go catalog, endpoints, `x-opencode-session` header, and `reasoning_effort` request shape. |
| `t8ful-tools` | `read_file` (text and images), `write_file`, `edit_file`, `apply_patch`, `ask_user_question`, `present_plan`, `recall`, `bash`, managed-process tools, `spawn_agents`, `agent_wait`, `agent_status`, and `task_*` tools, plus the injected `AttachmentStore` image storage is written through. |
| `t8ful-mcp` | MCP server lifecycle over `rmcp` (stdio and Streamable HTTP), tool discovery, `mcp__server__tool` naming, per-server plan-mode trust, credential-free `ServerConfig`, bounded result rendering. |
| `t8ful-sandbox` | Bubblewrap planning sandbox: fd-pinned read-only mounts, private scratch, seccomp, descriptor closing. |
| `t8ful-tui` | Dashboard, render cache, slash-command table, markdown, clipboard, terminal lifecycle. |
| `t8ful` | CLI, config resolution, root/instruction discovery, skill discovery and Git install, session/usage storage, journal search (`recall`), compaction wiring, headless renderer, composition. |

**Runtime paths:**
- State lives under `$XDG_STATE_HOME/t8ful`: `sessions/<id>/journal.jsonl` (created on first record, not at startup), `usage/`, `logs/t8ful.log` (created on first record), `trust.json`, and `auth.json`.
- Config merges in order (highest precedence first): CLI flags, environment, `--config`, project `.t8ful/config.toml`, user `$XDG_CONFIG_HOME/t8ful/config.toml`, system `/etc/xdg/t8ful/config.toml`, defaults.
- Skills in [.agents/skills/](.agents/skills/) are for the Pi/Claude development agents. t8ful never reads that directory: it discovers skills under `<project-root>/.t8ful/skills`, the user and system skill roots, and any configured `skills.paths`.

## Development environment

Tools from `flake.nix` (stable Rust toolchain, nightly `rustfmt`, `bwrap`, `nsjail`) are already on `PATH` in the developer shell. Use them instead of installing separate toolchains.

- Need a tool that isn't on `PATH`? Run `nix shell nixpkgs#<package> -c <command>` (or wrap commands in `nix develop -c ...` if the dev shell isn't loaded; `.envrc` uses the flake).
- Bubblewrap's binary is `bwrap`, not `bubblewrap`; `Bubblewrap::discover()` looks for `bwrap` on `PATH`.
- Published repo: `github.com/deathlesz/t8ful`. Root `.env` is untracked, ignored by t8ful, and must never be tracked or used as a build input.

## Testing

The test suite is hermetic: a loopback HTTP/SSE server mocks OpenRouter, `portable-pty` drives the real TUI, temp directories replace XDG state/config, and the PTY harness shadows `notify-send` with a recording no-op. Never make tests depend on the network, a real API key, or the user's XDG directories.

Test harnesses strip `T8FUL_MODEL` and `T8FUL_MODE` from child processes; the command below also strips them for in-process config tests:

```bash
env -u T8FUL_MODEL -u T8FUL_MODE T8FUL_SANDBOX_TESTS=required \
  cargo test --profile fast --workspace
```

- `T8FUL_SANDBOX_TESTS=required` makes host Bubblewrap tests mandatory (without it they skip when namespaces aren't available, which is expected inside a Nix build sandbox).
- Use `--profile fast` for the full suite — it optimizes dependencies while keeping workspace rebuilds cheap.
- **Test quality:**
  - Every test must be able to fail on an observable behavior or safety property. Skip tests that merely restate constants, defaults, static tables, or cosmetic output.
  - Prefer table-driven tests over per-key/per-color/per-string variants. Keep keystroke and rendering tests in `t8ful-tui` unit tests; reserve PTY tests for cross-component wiring.
  - Assert on output, not internal bookkeeping (counter assertions are allowed only for a documented performance contract, at most one per contract).
  - Flaky tests are bugs: fix the race or delete the test. Never `#[ignore]` it; the suite must pass with default parallelism.
  - Protect the security and data-integrity tier (sandbox isolation, policy gate, journal/resume, no-replay, provider parsing, atomic edits, worktree isolation) — add tests there first and never delete them to trim test counts.
  - Add deterministic regression tests for bug fixes.

## Verification

Run from the repo root after changes:

```bash
# every time
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
env -u T8FUL_MODEL -u T8FUL_MODE T8FUL_SANDBOX_TESTS=required \
  cargo test --profile fast --workspace
cargo build --release --workspace
cargo bench --no-run
# only when Nix changed
nix flake check 'path:.'
nix build 'path:.#checks.x86_64-linux.t8ful-modules' --no-link
nix build 'path:.#t8ful' --no-link
```

- Flakes read source through Git, so plain `nix build` and `nix flake check` ignore untracked files. Use `path:.` instead of staging files to work around it.
- Explicitly report any check you didn't run or couldn't pass. Live OpenRouter requests require explicit user authorization (they cost money) and are never automated.

## Safety and architectural invariants

- **Project trust.** Project config is restricted until trusted. Decisions are saved per canonical project root in `$XDG_STATE_HOME/t8ful/trust.json` and bound to the SHA-256 of the project config file (editing re-prompts). Never auto-trust, never trust system config, never extend trust by prefix or subdirectory, and never persist `--trust-project`. A trusted project may set the provider endpoint and MCP servers (intentional, and the prompt says so). Extend `FileConfig::requires_trust` as the single predicate — never duplicate it.
- **Plan mode.** Enforce restrictions in one place — the core policy gate and the sandbox — so subagents and local MCP servers inherit them. Fail closed: if the sandbox cannot start, return an error instead of running unsandboxed.
- **Autonomous coding mode.** Coding mode has no approval prompts, dangerous-command classifiers, run grants, or permission-widening callbacks. For example, a dirty working tree refuses an isolated worker with an error rather than asking. Trusted remote MCP tools can still have side effects t8ful cannot enforce.
- **No replay on doubt.** Never automatically retry a possibly side-effecting tool after a failure, stream error, or resume. The journal records dispatch intents and results so indeterminate outcomes are reported, never replayed.
- **Subagents.** Single-level, non-blocking, never merged. `spawn_agents` returns immediately; finished child reports land on the parent's notice queue (separate from steering, which waits for the current step's tool calls before the next model request). Reports are delivered once, at an idle-wake turn or immediately before the next prompt; `agent_wait` consumes notices for the reports it returns. Isolated workers' worktrees and branches are reported and preserved — never merged, applied, or committed. A child's tools are its role's list intersected with the parent's, minus `spawn_agents`, `agent_wait`, `agent_status`, `ask_user_question`, and `present_plan`; a role with `bash` is still not a read-only boundary.
- **MCP credentials.** Config holds environment variable *names* only, never values. `t8ful-mcp` reads them at spawn time, `ServerConfig` holds no secrets (`Debug` prints names only), every configured name joins the shell deny list, and local plan-mode servers do not receive them. Reject any new `[mcp.servers.*]` field that could hold a secret value.
- **API keys and secrets.** Keep credentials out of Nix-generated files, logs, and journals. `OPENROUTER_API_KEY` and `OPENCODE_GO_API_KEY` each win over the matching entry in `$XDG_STATE_HOME/t8ful/auth.json` (owner-only, written atomically by `/login <provider>`). That file is the only non-env credential store, must never become a build input, and must never be printed, logged, or journaled. Stripping known credential vars (`OPENROUTER_API_KEY`, `OPENCODE_GO_API_KEY`) from shell commands is accidental-leak reduction, not a security boundary.
