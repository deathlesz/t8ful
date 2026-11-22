# t8ful

A fast, Linux-first Rust coding agent for the terminal.

t8ful gives you a full-screen TUI for interactive work and a headless CLI for scripts. It talks to [OpenRouter](https://openrouter.ai), stays out of your way in coding mode, and fails closed in a Bubblewrap sandbox when you switch to plan mode.

## Highlights

- **TUI and CLI, one engine.** Full-screen dashboard with live streaming, tool cards, `@` file mentions, markdown/syntax highlighting, and terminal image previews — or `t8ful run` for streamed text and versioned JSONL.
- **Autonomous coding mode.** Edits files, runs commands, and manages background processes without approval prompts.
- **Sandboxed plan mode.** Exploration runs in [Bubblewrap](https://github.com/containers/bubblewrap): the host is mounted read-only with a disposable `/tmp` and no network. If the sandbox cannot start, plan mode errors instead of falling back.
- **Reliable file and shell tools.** Exact-match atomic multi-edits, unified patch application, bounded output, managed background processes, credential filtering for shell commands, and `read_file` over a screenshot returns the picture itself.
- **Durable sessions.** Append-only JSONL journals under `$XDG_STATE_HOME/t8ful`, session picker and resume, automatic context compaction, token/cost tracking, and `recall` to search past sessions.
- **Skills and MCP.** `SKILL.md` directories (local, Git-installed, or Nix-pinned) and local stdio or remote Streamable HTTP MCP servers (`mcp__server__tool`), referencing credentials by environment variable name only.
- **Delegation.** A session task graph, non-blocking child agents (`spawn_agents`) with separate contexts and journals, and optional isolated Git worktrees.
- **First-class Nix support.** Flake package plus NixOS and Home Manager modules.

## Requirements

- **Linux** (the plan-mode sandbox is Linux-specific).
- **Rust 1.88+** to build from source, or **Nix**.
- **An API key** for OpenRouter (`OPENROUTER_API_KEY`), OpenCode Go (`OPENCODE_GO_API_KEY`), or both. Either can also be saved with `/login` (stored owner-only in `$XDG_STATE_HOME/t8ful/auth.json`). Both catalogs are listed together, and the model you pick decides which provider serves the request.
- **`bwrap`** on `PATH` for plan mode (bundled by the Nix package/modules; install `bubblewrap` on other distros).

## Install

Run directly or install with Nix:

```bash
nix run github:deathlesz/t8ful
nix profile install github:deathlesz/t8ful
```

For declarative setup via `nixosModules.t8ful` or `homeModules.t8ful`, see [docs/nix-modules.md](docs/nix-modules.md).

Install from source with Cargo:

```bash
cargo install --git https://github.com/deathlesz/t8ful t8ful
```

Or build from a local checkout:

```bash
git clone https://github.com/deathlesz/t8ful
cd t8ful
cargo build --release
./target/release/t8ful
```

## Quick start

```bash
export OPENROUTER_API_KEY=sk-or-...      # and/or:
export OPENCODE_GO_API_KEY=oc_sk-...
t8ful
```

In the TUI:

- `Enter` sends a prompt; `Ctrl+Q` queues a follow-up while the agent is busy.
- `Tab` (or `/plan`, `/code`, `--mode`) switches between coding and plan mode.
- `?` on an empty composer opens keybindings; `/help` lists slash commands.
- `/model` switches models from both providers; each row shows its source, e.g. `kimi-k3 [opencode-go]`, and `/model opencode` narrows the list to OpenCode Go. `/attach` attaches images, `/sessions` and `/resume` switch conversations, `/new` starts fresh, and `/agents` / `/tasks` inspect delegation.
- `/login openrouter`, `/login opencode-go`, `/logout openrouter`, and `/logout opencode-go` manage the saved keys one provider at a time.

Headless CLI:

```bash
t8ful run "summarize the failures in tests/fixtures"     # streamed text
t8ful run --format json "fix the build" > events.jsonl    # versioned JSONL
t8ful run - < prompt.md                                   # prompt from stdin
t8ful models                                              # both catalogs, with a provider column
t8ful sessions                                            # saved sessions
t8ful usage                                               # token and cost totals
t8ful skills list
t8ful trust list
```

## Modes and safety

**Coding mode is autonomous.** There are no approval prompts, command classifiers, or permission grants. Tools run with your user privileges and no sandbox. You control the blast radius through the mode you pick, the environment you launch t8ful in, and the project you open.

**Plan mode blocks writes and network access.** File tools cannot modify the project, and shell commands run in Bubblewrap with a read-only host, a disposable `/tmp`, and no network. Direct file *reads* are not restricted in either mode — the model can read anything your user can read. Plan mode protects against unintended mutations, not secret exfiltration.

A few limits are structural and worth knowing up front:

- **Remote MCP servers** trusted for plan mode cannot be sandboxed by t8ful; you accept whatever their remote tools do.
- **Shared-workspace subagents** can overwrite each other's changes; use `worktree` isolation when parallel writers need separation.
- **Shell credential filtering** strips known API/MCP env vars to reduce accidental leaks, not to provide isolation.
- **Compaction and usage** include estimates; unknown values are reported as unknown rather than guessed.

See [docs/design.md](docs/design.md) (D16) for the complete list of enforced guarantees and explicit non-guarantees.

## Configuration

Settings merge in this order (highest precedence first):

| Layer | Source |
| --- | --- |
| CLI | flags (`--model`, `--mode`, `--max-steps`, …) |
| Environment | `OPENROUTER_API_KEY`, `OPENCODE_GO_API_KEY`, `T8FUL_MODEL`, `T8FUL_MODE`, `T8FUL_BASE_URL`, … |
| Explicit file | `--config <path>` |
| Project | `<project-root>/.t8ful/config.toml` |
| User | `$XDG_CONFIG_HOME/t8ful/config.toml` (`~/.config/t8ful` by default) |
| System | `/etc/xdg/t8ful/config.toml` |

**Project trust:** A project config cannot set `provider.base_url`, `skills.paths`, `ui.notify_command`, `[mcp.servers.*]`, or `agent.mode = "coding"` until you trust that project. In the TUI, t8ful prompts only when a project actually requests one of those settings; `t8ful run` warns and ignores them instead of blocking, and `--trust-project` grants trust for one run without saving it. Saved decisions are bound to the project root and a SHA-256 hash of its config file (so edits re-prompt) and can be inspected or revoked with `t8ful trust list` and `t8ful trust revoke`. System config can never grant authority.

**Credentials:** MCP settings (`pass_env`, `bearer_token_env`, `headers`) hold environment variable *names*, never values.

**State:** Sessions, usage records, trust decisions, and logs live under `$XDG_STATE_HOME/t8ful` (`~/.local/state/t8ful` by default).

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
env -u T8FUL_MODEL -u T8FUL_MODE T8FUL_SANDBOX_TESTS=required \
  cargo test --profile fast --workspace
cargo build --release --workspace
```

The test suite is hermetic: it uses a loopback HTTP/SSE mock for OpenRouter, a PTY for TUI integration tests, and temporary directories instead of your XDG paths. `T8FUL_SANDBOX_TESTS=required` makes the host Bubblewrap tests mandatory rather than skipping them if namespaces are unavailable.

For full behavior specs and repo conventions, see [docs/design.md](docs/design.md) and [AGENTS.md](AGENTS.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
