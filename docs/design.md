# t8ful Design

Authoritative behavior specification for t8ful 1.0.0: interfaces, policies, rationale, and explicit limits.

- User guide: [README.md](../README.md)
- NixOS & Home Manager modules: [nix-modules.md](nix-modules.md)
- Repository conventions: [AGENTS.md](../AGENTS.md)

Sections D01–D03 cover scope and architecture, D04–D07 interfaces and coding mode, D08–D10 plan mode and MCP, D11–D14 persistence and delegation, and D15–D16 quality and guarantees.

---

## D01 — Scope and platforms

- **Interfaces:** A responsive full-screen TUI and a headless CLI, built as a Rust Cargo workspace.
- **Platforms:** Linux-first. Core traits stay portable for future Windows/macOS support; Linux-specific sandbox code stays behind a platform boundary.
- **Provider:** OpenRouter is the only initial provider, behind a provider interface designed to support others later.
- **No plugin system:** External tools integrate over MCP; skills are instructions and supporting files, not code loaded into t8ful.
- **Performance vs. closure size:** Responsiveness and usefulness take priority over minimizing the Nix closure. Avoid needless memory or binary bloat, and measure before optimizing.

---

## D02 — Project root and configuration

### Root discovery

- Inside a Git repository, the project root is the Git worktree root; otherwise it is the launch working directory. Worktrees and nested repos resolve to their own root.
- Broken Git invocations are reported as errors rather than silently treated as "not a repo."
- Both the launch directory and reported root are canonicalized: the launch directory must stay inside the root, and `/` or a `core.worktree` redirect lacking the launch directory and a `.git` entry is refused.
- Shell commands still start in the launch working directory, not the project root.
- The project root is used for `.t8ful/config.toml`, `.t8ful/skills`, agent profiles, and the root `AGENTS.md`. Nested or global `AGENTS.md` files are not auto-loaded. `AGENTS.md` is read only if its canonical target is a regular file inside the canonical project root (an in-project symlink works; an escaping or non-regular target is ignored with a warning).

### Configuration layers and credentials

Config files use TOML. Precedence (highest first):

1. CLI flags
2. Environment variables (`OPENROUTER_API_KEY`, `T8FUL_MODEL`, `T8FUL_MODE`, `T8FUL_BASE_URL`, …)
3. Explicit `--config <path>`
4. Project (`<project-root>/.t8ful/config.toml`)
5. User (`$XDG_CONFIG_HOME/t8ful/config.toml`, default `~/.config/t8ful`)
6. System (`/etc/xdg/t8ful/config.toml`)
7. Built-in defaults

API keys live in `OPENROUTER_API_KEY` or `auth.json` (D05), and MCP tokens in environment variables or an external secret manager — never in Nix-generated config.

### Authority-granting settings and project trust

Only user config, environment variables, `--config`, CLI flags, or a **trusted project** may set:

- `provider.base_url`
- `skills.paths`
- `ui.notify_command`
- `[mcp.servers.*]`
- `agent.mode = "coding"`

System config can never grant these (it is root-owned, with no interactive user to confirm). An untrusted project can at most tighten to `plan`; any restricted key it sets is ignored with a warning naming the setting, file, and remedy (never echoing the value). Other project fields (shell timeouts/deny lists, prompt addenda, child roles/limits, worktree/target paths) cannot widen tools, redirect credentials, or leave plan mode. `FileConfig::requires_trust` is the single predicate for this check and destructures `FileConfig` exhaustively so new fields must be classified.

Trust decisions are stored in `$XDG_STATE_HOME/t8ful/trust.json` (owner-only, atomic write), keyed by canonical project root and bound to a SHA-256 hash of the project's config file:

- Editing `.t8ful/config.toml` invalidates the hash and prompts again (preventing a `git pull` from silently adding an MCP server under old trust).
- Trust never extends by prefix or to subdirectories, though symlinks sharing a canonical root share one decision.
- The TUI prompts only when a project actually requests restricted keys, offering: *trust and remember*, *continue without the project layer*, or *exit*.
- `t8ful run` never prompts: it warns, applies the restricted layer, and continues. `--trust-project` grants trust for one run without persisting it.
- `t8ful trust list` and `t8ful trust revoke` inspect and remove saved decisions.

### Removed approval settings

Coding mode is autonomous and has no approval gate. `[approval]` produces the standard unknown-section warning, `T8FUL_APPROVAL_MODE` is ignored, and `--allow-dangerous` / `--autonomous` are rejected as unknown arguments.

---

## D03 — Workspace and interfaces

| Crate | Responsibility |
| --- | --- |
| `t8ful-core` | Conversation/tool types, provider and tool traits, policy gate, agent loop, context budgeting, task graph, subagent orchestration, worktree interface, typed events. |
| `t8ful-openrouter` | OpenRouter HTTP/model catalog, SSE streaming and fragmented tool-call assembly, usage and error mapping. |
| `t8ful-tools` | Built-in file, edit, patch, shell, managed-process, recall, task, and subagent tools. |
| `t8ful-mcp` | MCP server lifecycle (`rmcp`), stdio & Streamable HTTP transports, `mcp__server__tool` naming, plan-mode trust, core `Tool` dispatch. |
| `t8ful-sandbox` | Linux Bubblewrap planning sandbox behind the core sandbox interface. |
| `t8ful-tui` | Full-screen dashboard, incremental renderer, input editor, clipboard, terminal lifecycle. |
| `t8ful` | CLI, config/trust resolution, skills, session/usage persistence, headless renderer, composition. |

`t8ful-core` accepts normalized requests and emits typed events (text, reasoning, tool activity, questions, tasks, processes, usage, completion, errors) consumed by the TUI, headless renderer, and session store. Provider and tool crates never depend on `t8ful-tui`. Protocol details stay in the provider adapter; permissions are enforced centrally before dispatch.

---

## D04 — User interfaces and input

### Dashboard and startup

- **Layout:** Full-screen alternate-screen dashboard showing the transcript, composer, and (when wide enough) tasks, subagents, processes, model, and usage, with a compact fallback at narrow widths.
- **Diagnostics:** While the TUI owns the terminal, nothing writes to stderr. `--log`/`T8FUL_LOG` writes to `$XDG_STATE_HOME/t8ful/logs/t8ful.log` (private, created on first record); user warnings appear in the transcript.
- **Non-blocking startup:** The first frame renders before model-catalog discovery and MCP server startup finish. Both run in parallel with deadlines; submitted prompts and notice-driven wake turns queue until they settle so the first request never sees partial limits or a partial tool list. Catalog failure/timeout falls back to configured limits; a failed MCP server logs one transcript warning and the run continues without its tools. Headless runs await startup inline.
- **Lifecycle:** Seed the startup banner (version, key hints, slogan) on new sessions only, never on resume. Always restore terminal state (including disabling mouse capture) on normal exit, error, cancellation, or panic.

### Streaming, tool cards, and incremental rendering

- Stream model output and tool output without blocking input or redraws. A tool card appears on dispatch, streams bounded live output, and is replaced by the final result.
- **Incremental rendering:** Cache wrapped line ranges by entry revision, live-block generation, and width; redraw only when state changes, keeping idle CPU near zero. Live markdown and reasoning render block by block (completed blocks stay cached; unterminated fenced code renders unhighlighted until its closing fence arrives). Growing tool bodies wrap only newly arrived lines.
- **Tool cards:** `bash` headers show the full wrapped command; other tools elide the path or first line to 60 characters. Collapsed cards show their last 4 body lines plus a count of earlier lines revealed by `Ctrl-O`; preview bodies show their first 5. Any card whose first content line reports a nonzero exit, timeout, signal, or cancellation shows the error glyph and never collapses (including after resume).

### Scrolling and clipboard

- `PgUp`/`PgDn` scroll by a full viewport (offset clamped to the real maximum), and mouse-wheel scrolling works when mouse capture is on. While scrolled up, new lines below do not shift the viewport; offset `0` follows the tail.
- **In-app selection:** Dragging highlights transcript cells in reverse video and copies on release (`wl-copy`, then `xclip`, then OSC 52; OSC 52 is preferred over SSH). Double-click selects a word; `Esc` clears selection before cancelling anything. Copies run asynchronously with a deadline, complete when the helper exits (not when pipes close), report only the latest selection, and write OSC 52 only from the event loop. `[ui] mouse = false` disables mouse capture as an escape hatch.

### Presented plans and mode switches

- A presented plan renders as one never-collapsed block: amber `▏` gutter, `▏Plan` header with right-aligned `· presented`, horizontal rule, markdown body, and a dim footer noting it is a proposal.
- A live `present_plan` in plan mode opens a UI-local hand-off panel in the composer slot:
  - `Implement here` — switches to coding mode and submits `Implement the plan you presented.`
  - `Implement in a new session` — starts a fresh coding session seeded with the plan.
  - `Stay in plan mode` — dismisses the panel.
  It uses the same inline row grammar as questions, never enters the question router or blocks a turn, is dismissed by typing/steering/queueing, and is never shown on resume.
- User-initiated mode switches draw a colored `plan → coding` (or reverse) divider; startup and resume syncs draw none.

### Footer, spinner, and notifications

- **Footer (2 rows):**
  - Row 1: launch path + Git branch on the left; `<model> · <reasoning> · <mode>` on the right (omitting reasoning when at `default`).
  - Row 2: `↑<input> ↓<output> CH<hit>% $<cost> <used>%/<window>` on the left (omitting zero/unknown parts, never showing `(auto)`, with cache hit rate and context pressure from the latest request and context reset to unknown after compaction); `? help` on the right.
- **Busy line:** Spinner and elapsed time only. Verbs come from `claude-code-spinner-verbs` (minus `Clauding`), rotate every 20 seconds, and are seeded by session ID + turn number for deterministic replay. Transient state never flashes in the footer; durable feedback goes into the transcript.
- **Notifications:** Terminal bell plus one fire-and-forget desktop notification (`notify-send` on `PATH` or `[ui] notify_command` from user/`--config` only — never project/system) passing one of four fixed labels as the final argument: `Turn finished`, `Turn failed.`, `Plan is ready.`, or `Question waiting.`. At most one per turn (plan hand-off replaces turn end; questions notify independently; mid-turn `Error` goes to the transcript only; no error text or end reason is sent). `[ui] notifications = false` disables both (config-only).

### System prompt composition

Sections are separated by blank lines, with volatile mode sections last:

1. Preamble (filtered tool list and editing rules)
2. `<addendum>` (optional, from `agent.system_prompt_extra`)
3. `<skills>` (optional index of installed skill names + descriptions, instructing the model to load full text via `skill`)
4. `<project_context>` (when root `AGENTS.md` exists)
5. `<cwd>`
6. `<environment>` (mode-specific)
7. `<plan_mode>` (in plan mode)
8. `<mode_change>` (first request after a user-initiated mode switch only; never on startup/resume)

Preamble editing rules direct tool choice by intent: `read_file` to inspect, `edit_file` for atomic exact-text replacements (all matched against the original file, unique unless `replace_all`, no overlaps), `apply_patch` for multi-hunk/multi-file edits, and `write_file` for new files or deliberate rewrites — never `sed -i`, Python/Perl one-liners, or shell redirection. Background commands produce automatic completion notices (`[shell] notify_on_exit` controls idle wake).

### Commands, pickers, `@` mentions, and queues

- **Slash commands:** One command table backs both `/help` and completion above the composer (arg hints + fixed values like `/reasoning` levels for the active model or `/login openrouter`). Filters by the first token, shows up to 6 rows, and supports arrow selection, `Tab` completion, `Enter` to run the highlighted row, and `Esc` to dismiss until the next edit. Exact command names and buffers with arguments submit literally.
- **Shortcuts & commands:** `Tab` toggles coding/plan mode when no popup, picker, overlay, or question is active; `?` on an empty composer or `/help` opens help; `/model` and `Alt-E` / `/reasoning` open the model and reasoning pickers; `/sessions`, `/resume`, `/new`, `/attach`, and `/agents`/`/tasks` cover the common session and delegation flows; `/process [list|view|stop] [id]` lists processes with state, prints one process's collected output, or stops it (`kill` is not an alias; the notice above the composer is running-only); `/login [provider]` opens a masked prompt showing the active credential source (`env`, `auth.json`, or missing), saves to `auth.json`, and swaps the provider + reloads the catalog without restarting (rejected while a turn is running); `/logout` clears it.
- **Reasoning levels:** Vocabulary is `default`, `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. Offered levels come from the model's catalog `supported_efforts`, so `xhigh`/`max` appear only for models that accept them and a mandatory-reasoning model offers no `off`; config can remap any level or mark it unsupported. Switching models republishes the set and clamps the current level.
- **`@` file mentions:** Completed from a background gitignore-aware index of the project root (never blocks the first frame). Same popup navigation as slash commands; trailing `/` drills into directories; `Tab` inserts the path; `Enter` accepts a partial path or submits an exact one. Mentions remain plain text and never auto-read files.
- **Steering and follow-up queues:** `Ctrl-Q` queues follow-ups. Up to 3 pending lines render above the composer (`Steering:` then `Follow-up:`) plus a hidden count, held-queue marker, and `↳ alt+up to edit pending messages`. `Alt-Up` pulls pending steering and queued follow-ups back into the composer ahead of typed text and clears the driver queue. Steering delivers at safe boundaries (after a text-only response or after all tool calls in the current assistant message finish) without cancelling in-flight calls; only cancellation or mode switches interrupt a batch. User messages appear in the transcript once, when journaled. Cancelled, failed, or resumed turns hold the queue until `Alt-Up`; successful turns drain it in order.

### Structured questions

Shown as a full-width inline panel in the composer slot while keeping the transcript visible:

- Displays prompt, numbered rows (`→ N. Label  – description` with hanging indents), a `Type something.` row, and a dim key hint.
- Two or more questions add a header tab strip (active tab bracketed) plus a `Review` tab; a single question submits on `Enter`.
- Keys: `↑`/`↓` move, `Enter` selects, `1`–`4` select directly, `t` opens inline custom entry, `←`/`Shift-Tab` goes back with saved answers, `→`/`Tab` advances once answered, `Esc` leaves custom entry → goes back → cancels.
- `multiSelect` renders `[✓]` checkboxes (`Enter`/`Space` toggle, `Done` row submits once non-empty, custom text kept alongside checks).
- Single-select options may include a markdown `preview`; at ≥72 columns the panel splits into options + preview (custom entry expands to full width).
- Answers carry selected indexes plus optional text and route through the question router exactly once.

### Shared text editor

One Emacs-style editor powers the composer, model/session filters, login prompt, and question custom entry: `Ctrl-A`/`E` (line start/end — hence reasoning picker is `Alt-E`), `Ctrl-B`/`F` (char), `Ctrl-P`/`N` (line), `Ctrl-K`/`U` (kill to end/start), `Ctrl-W` (kill word), `Ctrl-Y` (yank), `Ctrl-D` (delete; quits only on empty composer), `Ctrl-H` (backspace), `Alt-B`/`F` (word), plus Home/End/arrows/Delete/Backspace.

### Images

- Accepts file and Linux clipboard images (PNG, JPEG, WebP, GIF) up to a configurable 20 MiB encoded limit, copied into session storage for resume. Reports unsupported models or missing clipboard backends clearly.
- Pending attachments sit in a compact tray above the composer (ordinal, format, dimensions, size). On `UserMessage` confirmation they move into the transcript with a caption and terminal preview capped at 100×24 cells.
- Protocol preference: Kitty, Sixel, or iTerm2 (auto-detected; under tmux only when `TERM` starts with `tmux` or `TERM_PROGRAM=tmux` for passthrough), falling back to halfblocks. `T8FUL_IMAGE_PROTOCOL` (`kitty`, `sixel`, `iterm2`, `halfblocks`) overrides detection.
- Lazy decoding on ≤2 workers with a 64 MiB per-image decoded cap and 64 MiB preview cache. Missing, corrupt, or oversized files keep their caption and show a preview-unavailable line. Resume restores metadata in place and never requeues submitted images.

### Headless CLI

`t8ful run` outputs streamed text or versioned JSONL on stdout, keeping diagnostics separate. An unanswered structured question exits nonzero with `input_required`. Headless runs never pause for approval.

---

## D05 — OpenRouter provider

- **Credentials:** `OPENROUTER_API_KEY` always wins; otherwise t8ful reads `auth.json` in the state directory (owner-only, atomic write; malformed file is a startup error naming the path). Startup, `t8ful run`, and `t8ful models` share this resolution. Without credentials, the TUI starts unauthenticated (`not connected; run /login`, no catalog request) while headless runs fail early. Idle `/login` or `/logout` swaps the provider and compaction client, resets the catalog to pending, and cancels any in-flight catalog fetch.
- **Extensibility:** A new provider implements the core provider interface without changing agent-loop or UI behavior.
- **Models:** Headless runs require a configured model ID. The TUI defaults to `deepseek/deepseek-v4.1-flash-1017` when unset (OpenRouter exposes no default-model endpoint); `/model` switches at turn boundaries, and subagent roles can pick their own model.
- **Streaming and validation:** Reassembles incremental text and fragmented tool-call IDs, names, and JSON arguments; validates complete calls before dispatch. Maps auth, rate-limit, transport, unsupported-model, and malformed-stream errors clearly. Never auto-replays executed tools after cancellation or stream failure. A text-only reply consisting solely of fabricated tool markup (`<tool_result>`, `<tool_call>`, `<function_call>`) is displayed with a warning and never treated as a tool result.
- **Reasoning effort:** `default` omits the reasoning field; `off` sends `reasoning: { enabled: false }`; `minimal`, `low`, `medium`, `high`, `xhigh`, and `max` send `reasoning: { effort: <level> }`. Catalog `supported_parameters` / `supported_efforts` determine support; switching to a model without reasoning resets non-default effort to `default` with a warning, and the picker dims unsupported options.

---

## D06 — Built-in tools and processes

| Capability | Behavior |
| --- | --- |
| `read_file` | Bounded file reads; reports missing paths and truncation. |
| `write_file` | Create or deliberately overwrite files (autonomous in coding mode, inside or outside project root). |
| `edit_file` | Atomic multi-block exact-text replacement against original content; fails unchanged on missing, ambiguous, or overlapping matches. |
| `apply_patch` | Validated unified patch application; reports conflicts without silent overwrites. |
| `bash` & processes | Fresh Bash per command with bounded output, background handoff, `process_wait`, `process_output`, `process_stop`. |
| Tasks & subagents | `task_*`, `spawn_agents`, `agent_wait`, `agent_status` (D13–D14). |
| `recall` | Read-only, on-demand search across prior sessions of the project (or all projects); no index or model call. |

There is no dedicated file-search or web tool — use `rg`/`grep` via `bash` or an MCP server. Revisit whether both `edit_file` and `apply_patch` earn their place by evaluating them on representative coding tasks.

### Shell and managed processes

- Each command runs in a fresh Bash process inheriting the launch cwd and environment (subject to D07 filtering); `cd` and `export` do not persist across calls. Output truncation is always reported.
- Running background processes stream live output to their Bash card at ≤20 updates/s (display-only: never journaled, never added to model context, never wakes the agent).
- A background process that exits on its own injects one notice into the root conversation (ID, command, state, exit code/signal, elapsed time, bytes, truncation flag, last 2 KiB of output, and pointer to `process_output`). With `[shell] notify_on_exit = true` (default) it wakes an idle agent; otherwise it delivers before the next prompt.
- `process_wait` blocks for a bounded number of whole seconds (default 120, `0` for no deadline, max 1 day) and returns state + last 2 KiB. An exit observed via `process_wait` consumes its notice (no wake); a timed-out wait claims nothing, so the notice still arrives later.
- `process_stop`, `/process stop`, and shutdown produce no notice. On exit, t8ful terminates and reaps managed process groups. Descendants that call `setsid` leave the group and can survive — `process_stop` and stopped status state this explicitly (coding mode makes no PID namespace or cgroup claim).

### Disabling tools

`[tools] disabled = ["recall", "bash"]` removes tools from the registry (absent from the provider's tool list, `<tools>` prompt, and child registries). Built-in and conditional tool names are validated at startup (typos are errors; conditional names are accepted even when their feature is off). A role naming a disabled tool triggers a warning.

---

## D07 — Autonomy and credential policy

- **Coding mode is autonomous.** All tool effects — writes anywhere, destructive shell or Git commands, package changes — run with the user's privileges and no sandbox, approval prompt, classifier, or grant. The user controls blast radius through mode, launch environment, and project choice.
- **Plan mode is the boundary.** It denies mutating tool effects and sandboxes shell commands (D08). Direct reads stay unrestricted in both modes. Nothing (model, instructions, skills, subagents, config, or MCP) can disable plan-mode restrictions.
- **Credentials:** `auth.json` is the only non-env API key store; it is never a Nix build input and never enters logs, journals, transcripts, or shell environments. Never intentionally log secret configuration or authorization headers. General shell commands strip `OPENROUTER_API_KEY` and all configured MCP credential env vars. In coding mode, MCP servers receive only their own configured vars; in plan mode, local MCP servers receive none. Env filtering is accidental-leak reduction, **not** a security boundary for unsandboxed coding commands.
- **Shared workspaces:** Concurrent agents in a shared workspace can overwrite each other's files or shell state. Attribution and tasks reduce confusion but do not prevent conflicts.

---

## D08 — Plan mode and local isolation

Plan mode is an explicit user mode for exploration, structured questions, and presenting a plan. Local file tools, shell commands, and local MCP servers cannot write project files or host files; saving a plan to disk requires switching to coding mode. Necessary host exceptions are session/usage storage, OpenRouter requests, and explicitly trusted remote MCP servers (whose remote side effects cannot be locally sandboxed).

- **Authoritative mode:** The current request's `<plan_mode>` block governs that request; earlier environment blocks in the transcript are historical. `present_plan` submits a proposal for review and does not switch modes or authorize implementation.
- **Direct reads:** Unconfined in both modes — the model can read anything the t8ful process can read. Plan mode protects against writes and network access, not read confidentiality.
- **Sandbox posture ("read the whole host, write nothing"):**
  - Host `/` and every nested submount are individually bound read-only (a recursive bind does not make nested mounts read-only).
  - Only a private, disposable `/tmp` and `/var/tmp` are writable; read-scope roots are re-bound read-only afterwards so projects under `/tmp` remain readable.
  - Environment is reduced to `PATH`, `HOME`, `LANG`, `LC_*`, `TZ`, plus non-denied extras, passed via `execve` (never `--setenv`, keeping values out of `/proc/<pid>/cmdline`). `SSH_AUTH_SOCK`, `DBUS_SESSION_BUS_ADDRESS`, `WAYLAND_DISPLAY`, `DISPLAY`, `NIX_REMOTE`, and `OPENROUTER_API_KEY` are always dropped.
  - Network namespace is unshared, and a seccomp filter denies `socket`, `socketpair`, `connect`, `bind`, `listen`, `accept`, `accept4`, and `io_uring_setup`.
  - Writes to `~`, `/etc`, `/nix/store`, `/var`, or `/run` fail with `EROFS`, and remounting fails.
- **Tradeoff:** Because the host is readable, sandboxed commands can read SSH keys, cloud credentials, `auth.json`, and `/nix/store`. Never put secrets in the Nix store.
- **Fail closed:** Planning shells run without extra prompts; if the sandbox cannot start or lacks required protections, plan mode fails with an explanation and never falls back to an unsandboxed shell. Local MCP servers and subagents inherit these restrictions. Prepare missing Nix packages outside plan mode (`nix shell` needs network/store access).

---

## D09 — Linux sandbox selection

Bubblewrap (`bwrap`) is the sole sandbox backend in `t8ful-sandbox`, keeping D08 enforced in one place behind a platform boundary:

- Runs unprivileged (no root or setuid helper).
- `--unshare-all` isolates user, mount, PID, IPC, UTS, and network namespaces; seccomp blocks socket creation.
- `--ro-bind-fd` binds `/` and each submount read-only with descriptor pinning (eliminating path-swap TOCTOU windows).
- Creates disposable tmpfs `/tmp` and `/var/tmp` and re-binds read-scope roots read-only.
- Passes the reduced environment via `execve`, never `--setenv`.
- Bundled as `bwrap` in the Nix dev shell and package; `Bubblewrap::discover()` resolves `bwrap` on `PATH` and nothing else.

`nsjail` remains a plausible future backend, not a parallel implementation. If `bwrap` is missing or cannot start, plan mode fails closed.

---

## D10 — MCP integration and plan-mode risk

t8ful supports local stdio and remote Streamable HTTP MCP **tools** via `rmcp` 3.11 (`client`, `transport-async-rw`, `transport-streamable-http-client`).

- **Protocol & TLS:** Uses `Auto` lifecycle — probes `server/discover` for stateless 2026-07-28 and falls back to `initialize` (up to 2025-11-25) on the same connection. Remote servers use `reqwest` with bundled `webpki-roots` (matching the provider client for deterministic TLS in minimal containers and Nix sandboxes). `AsyncRwTransport` plugs directly into `SandboxedProcess` stdio.
- **Configuration:** `[mcp.servers.<name>]` from user config, `--config`, or a trusted project (D02), specifying either `command` (with optional `cwd` relative to project root) or `url` — both or neither is a startup error. Credentials are env var **names** only (`pass_env`, `bearer_token_env`, `headers`), read at spawn time; a missing var fails that server naming the variable. Local children receive only `PATH`, `HOME`, `TMPDIR`, plus `pass_env`. All configured credential var names join the shell deny list.
- **Naming:** Exposed to the model as `mcp__<server>__<tool>`, byte-wise sanitized to `[A-Za-z0-9_-]` and capped at 64 characters (truncating the tool part + short hash; secondary collisions get a numeric suffix and warning). `tools/call` reverse-maps back to the server's original name. `[tools] disabled` accepts `mcp__*` dynamically (an unmatched name warns rather than failing).
- **Lifecycle:** Registered in the shared `ToolRegistry` (so disabling, child snapshots, and role intersection work uniformly) and listed in a dynamic `<mcp>` block under `<tools>`. Servers start concurrently under one `mcp.startup_timeout_ms` deadline (default 12 s); failures or timeouts produce one transcript warning and the run continues. All servers shut down on drop or session switch.
- **Plan mode:**
  - Local servers run inside the Bubblewrap sandbox with `pass_env` withheld. Launchers like `npx` usually fail without network/writable paths; the error explains this and suggests configuring `url`. Without a sandbox, local servers do not start in plan mode (no fallback).
  - Remote servers run in plan mode only if configured with `plan = "read-only"` (`ToolEffect::ReadOnly`); otherwise they report `Mutating` and the policy gate denies calls via `Tool::denial_reason` naming the setting. The registry and prompt stay identical across modes. There is no per-call approval gate — trusting a remote server for plan mode accepts its remote side-effect risk.
- **Results & errors:** Walks `content` in order (text verbatim, image/audio as size+type placeholders, resource links as URIs, `structuredContent` as JSON only when no content block produced text). `isError` marks tool failure; output truncates to `shell.max_output_bytes`. Arguments are validated as a JSON object (deeper checks are the server's job). Cancelled calls are reported indeterminate and never replayed. Servers requiring interactive OAuth fail naming `bearer_token_env`.
- **Out of scope for 1.0.0:** MCP resources, prompts, sampling, elicitation, tasks, legacy HTTP/SSE transport, and interactive OAuth.

---

## D11 — Sessions, context, and usage

- **Turn limits:** Unbounded unless `agent.max_steps` or `--max-steps` is configured.
- **Sensitivity:** Saved prompts, outputs, attachments, and recall excerpts can contain sensitive data; the journals are the user's to delete.
- **Journals:** Versioned append-only JSONL under `$XDG_STATE_HOME/t8ful` (default `~/.local/state/t8ful`) with private permissions. A session ID exists from startup, but `journal.jsonl` is created on the first record so untouched sessions leave nothing behind. Every record carries a Unix timestamp (seconds). Attachments are stored separately with random same-directory names (never overwriting existing files). Original history is preserved across compaction and retained until deleted by the user, with a 128 MiB load cap (actionable error if exceeded).
- **Listing:** `/sessions`, `/resume`, and `t8ful sessions` show only sessions containing at least one user prompt (excluding mode-only, attachment-only, or skill-only journals), ordered newest-first by latest message age (falling back to file mtime, then header).
- **Durability & recovery:** Writes are serialized and guarded against concurrent writers. Incomplete trailing records recover without replaying actions; malformed earlier records are reported rather than silently dropped; crashes leaving a tool outcome unknown are reported as ambiguous and never replayed.
- **`recall` tool:** Searches prior journals on demand — user prompts and assistant text only (never tool calls/results, skill wrappers, or the current session). Default scope is the current canonical project root; `scope = "all"` searches every project. No index and no LLM call inside the tool: returns ranked excerpts (session ID, age, mode, title, role). Budget: newest-first scan of up to 200 journals, 2 MiB per journal, 32 MiB total (reports how many older sessions were skipped; skips malformed lines). Journaling a `recall` call records only the query and returned excerpts.
- **Compaction:** Resolves model limits from the OpenRouter catalog (including vendor-prefixed IDs), overridden by `agent.context_limit`. Uses the latest provider usage as baseline and estimates only subsequent messages. Triggers when estimated tokens + 16,384 reserve ≥ limit, keeping ~20,000 recent tokens verbatim and cutting at user/assistant boundaries (both budgets scale proportionally on small windows so compaction never disables itself; split turns summarize an ordered prefix). Compaction and context-limit errors are reported rather than silently dropping history.
- **Usage:** Tracks input/output tokens and cost per request, session, model, and subagent (including compaction), plus cross-session totals. Deduplicates repeated records for the same provider generation while keeping distinct billable attempts. Missing values are marked unknown/partial, never zero.

---

## D12 — Skills and installation

- Skills are directories containing `SKILL.md` (name, description, instructions) plus optional supporting files.
- **Discovery precedence:** project (`.t8ful/skills`) > user (`$XDG_CONFIG_HOME/t8ful/skills`) > NixOS system (`/etc/xdg/t8ful/skills`) > configured `skills.paths` in order. Name collisions produce a warning rather than being hidden silently.
- **Loading:** Short descriptions appear in `<skills>`; full instructions load on demand via `skill` or `/skill <name> [prompt]`. Explicit activation renders as a `skill` tool card (rebuilt on resume), and `/skill <name> <prompt>` submits the prompt in the same turn after the card. Skills cannot weaken plan-mode or sandbox policy; supporting scripts run through normal tool policy.
- **Installation:** The Git installer accepts an optional revision (resolving and recording the commit if omitted) and subdirectory. Installs and updates are explicit, never execute repo scripts, reject path traversal, and refuse to overwrite unmanaged or Nix-managed skills. Nix modules also accept pinned store paths.

---

## D13 — Tasks and subagents

- **Task graph:** Tracks `pending`, `in_progress`, `completed`, and `cancelled` tasks with dependencies and optional owner/agent IDs. Blocked state is derived from incomplete (or missing) dependencies; cycles are rejected. Spawning a child creates a task automatically, and standalone tasks are supported. Rebuilt from the journal on resume.
- **Roles and tools:** Global, project, or ad hoc roles define instructions, allowed tools, optional model, and default isolation. Empty `tools` inherits all parent tools minus excluded ones; a named list is intersected with the parent's tools (a child never gains tools the parent lacks). Children are always single-level: `spawn_agents`, `agent_wait`, `agent_status`, `ask_user_question`, and `present_plan` are always excluded, and children have no question router. Children run in separate contexts and inherit the parent's plan mode and sandbox. A role with `bash` is not a read-only boundary.
- **Non-blocking execution & notices:** `spawn_agents` returns immediately. When a child stops, its report goes onto the parent's notice queue (separate from steering, which waits for the current step's tool calls). Reports deliver exactly once at a turn boundary: waking an idle parent when `agents.wake_on_finish` is enabled (configurable because it spends money unattended), or immediately before the next prompt otherwise. `agent_wait` consumes notices for the reports it returns. Switching sessions cancels and forgets all children and drains agent and process notices so nothing leaks into the new session.
- **Notice framing:** Notices are delivered as user-role messages (no synthetic tool-call ID). Free-text report fields and process output tails are escaped so they cannot forge closing tags, agent summaries truncate at 8,000 bytes with a note, and resume renders notices as informational rows rather than user prompts. The user-role envelope is a documented limitation, not an authorization input.
- **Limits & accounting:** Defaults are 4 concurrent children and 16 spawns per root turn (both configurable). `agents.max_depth` is parsed for backwards compatibility and ignored with a warning. Each child writes its own journal, with usage attributed in the shared ledger and its report.

---

## D14 — Shared workspaces and worktrees

- Subagents share the workspace by default (preserving warm build caches; concurrent writes can conflict).
- Isolated workers (`isolation = "worktree"`) get a Git worktree at `HEAD`; without a usable repo and commit, spawn returns an actionable error rather than falling back to shared mode.
- **Clean-tree requirement:** Staged, unstaged, or untracked changes refuse the spawn with an error naming the affected paths (no prompt, since coding mode is autonomous, and silently starting from `HEAD` would drop the user's changes). Ignored files never block, though their absence from the worktree is noted. t8ful never stashes, resets, commits, or alters parent work.
- **No auto-merge:** Isolated worktrees and branches are reported and preserved after the child stops; integrating them is the user's Git operation. `agents.shared_target_dir` lets parallel Rust builders share one `CARGO_TARGET_DIR`.

---

## D15 — Nix, performance, and quality

- **Nix:** Exposes a `t8ful` package (`nix build`/`nix run`) plus NixOS and Home Manager modules compatible with writable user skills. Do not assume Nix on future platforms; in coding mode on Nix systems, `nix shell nixpkgs#<package> -c <command>` can supply missing tools.
- **Performance:** Measure optimized builds on target hardware: warm TUI startup, input-to-render p50/p95 under streaming, idle/peak memory, and binary size. Keep Nix evaluation and network/model latency separate. Establish baselines before setting numeric budgets, and investigate regressions rather than cutting features for closure size.
- **Micro-benchmarks:** `divan` benches (`cargo bench`) cover a 20 KB streamed markdown delta, a 256 KB appended tool body, a mention keypress over a large tree, and an 8 KB prompt. They are a change signal, not a CI gate.
- **Testing:** Deterministic unit and integration tests using fake provider streams, loopback/stdio MCP servers, temp Git repos, fake clipboards, and controlled processes. No network or paid OpenRouter calls; live smoke tests require explicit authorization.

---

## D16 — Guarantees and limits

Any change that weakens an enforced property, or quietly claims a non-guarantee, is a defect.

### Enforced

- **Project authority requires trust:** `provider.base_url`, `skills.paths`, `ui.notify_command`, `[mcp.servers.*]`, and `agent.mode = "coding"` from a project file apply only after user trust, bound to the config file's SHA-256 hash.
- **Plan mode cannot mutate local state:** File tools, the policy gate, and Bubblewrap block project and host writes, network access, and host sockets.
- **Plan mode fails closed:** If the sandbox cannot start, planning shells and local plan-mode MCP servers error rather than running unsandboxed.
- **No replay on doubt:** Journals record dispatch intents and results; indeterminate tool outcomes are reported, never retried automatically.
- **Children cannot escalate:** Child tools are role ∩ parent minus delegation and user-interaction tools, inheriting the parent's mode and sandbox.
- **Child work is never merged:** Isolated worktrees and branches are reported and preserved for the user to integrate.
- **Credentials stay out of config:** MCP credentials are env var names only; OpenRouter keys come from `OPENROUTER_API_KEY` or owner-only `auth.json` and are never printed, logged, journaled, or placed in Nix build inputs.
- **Restricted layers cannot widen policy:** System config never grants authority, and an untrusted project can at most tighten to plan mode.

### Not guaranteed

- **Coding mode is not a security boundary:** Tools run with your privileges and no sandbox, approval gate, or command classifier.
- **Direct reads are not confined in plan mode:** The model can read any file the process can read, inside or outside the project.
- **Plan-mode shells can read the host:** The sandbox mirrors host reads by design; never place secrets in the Nix store.
- **Remote MCP side effects cannot be enforced:** A remote server trusted for plan mode can do whatever its remote API allows.
- **Credential filtering is not isolation:** Stripping known env vars from shells only reduces accidental leaks.
- **Shared-workspace subagents can conflict:** Concurrent writers can overwrite each other's files and shell state.
- **Sandbox isolation is not kernel-exploit immunity:** Nor does it protect data already sent to OpenRouter.
- **Estimates are estimates:** Context budgets and usage totals can be partial; unknown values are reported as unknown rather than invented.
