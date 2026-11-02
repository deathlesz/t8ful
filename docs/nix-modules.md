# NixOS and Home Manager modules

t8ful provides two Nix modules under the same `programs.t8ful` namespace:

- `nixosModules.t8ful` — system-wide defaults under `/etc/xdg/t8ful`
- `homeModules.t8ful` — per-user config under `$XDG_CONFIG_HOME/t8ful` (usually `~/.config/t8ful`)

Both install the `t8ful` package, generate non-secret `config.toml` files, and link pinned skills. Neither ever writes credentials.

| Flake output | Purpose |
| --- | --- |
| `packages.<system>.t8ful` | The `t8ful` binary |
| `nixosModules.t8ful` | System layer (`/etc/xdg/t8ful`) |
| `homeModules.t8ful` | User layer (`$XDG_CONFIG_HOME/t8ful`) |
| `checks.<system>.t8ful-modules` | Evaluation check for both modules |

## NixOS

```nix
{
  inputs.t8ful.url = "github:deathlesz/t8ful";

  outputs = {nixpkgs, t8ful, ...}: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      modules = [
        t8ful.nixosModules.t8ful
        {
          programs.t8ful = {
            enable = true;
            settings.provider.model = "anthropic/claude-sonnet-4";
          };
        }
      ];
    };
  };
}
```

This adds `t8ful` to `environment.systemPackages`, writes `/etc/xdg/t8ful/config.toml`, and links pinned skills at `/etc/xdg/t8ful/skills/<name>`.

t8ful reads the system layer when `XDG_CONFIG_DIRS` is unset or includes `/etc/xdg`, using only the first existing system config directory. If another directory shadows `/etc/xdg`, adjust `XDG_CONFIG_DIRS` or configure the user layer instead.

## Home Manager

```nix
{
  inputs.t8ful.url = "github:deathlesz/t8ful";

  outputs = {nixpkgs, home-manager, t8ful, ...}: {
    homeConfigurations.me = home-manager.lib.homeManagerConfiguration {
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      modules = [
        t8ful.homeModules.t8ful
        {
          programs.t8ful = {
            enable = true;
            settings = {
              provider.model = "anthropic/claude-sonnet-4";
              ui.theme = "ansi";
            };
          };
        }
      ];
    };
  };
}
```

This adds `t8ful` to `home.packages`, writes `$XDG_CONFIG_HOME/t8ful/config.toml`, and symlinks each pinned skill individually under `$XDG_CONFIG_HOME/t8ful/skills/<name>`. Individual links keep the directory writable so skills installed with `t8ful skills install` coexist with declarative ones.

Both modules work well together: set host-wide defaults in NixOS and override per user in Home Manager.

## Options

| Option | Type | Notes |
| --- | --- | --- |
| `programs.t8ful.enable` | bool (`false`) | Install the package and write config. |
| `programs.t8ful.package` | package | Defaults to `packages.<system>.t8ful`. |
| `programs.t8ful.skills` | attrs of path (`{}`) | Pinned skill sources, linked as `<skills-root>/<name>`. |
| `programs.t8ful.settings` | attrs (`{}`) | Non-secret settings; unset options are omitted from TOML. |

### `programs.t8ful.settings`

Options mirror `config.toml`. Unset options are omitted so they never clobber lower config layers. Numeric limits start at `1` (except `shell.default_timeout_ms`, where `0` disables the timeout).

| Setting | Type | Notes |
| --- | --- | --- |
| `provider.kind` | `"openrouter"` | Only OpenRouter is supported. |
| `provider.model` | string | Default model (`--model` and `T8FUL_MODEL` override it). When unset, the TUI uses its built-in default and headless runs error. |
| `provider.base_url` | string | **User layer only** (ignored in system config). |
| `provider.catalog_timeout_ms` | positive int | Model catalog request deadline (ms). |
| `agent.max_steps` | int ≥ 1 | Tool steps per turn; unlimited when unset. |
| `agent.max_output_tokens` | int ≥ 1 | Output token cap requested from the provider. |
| `agent.context_limit` | positive int | Explicit context window; overrides catalog metadata. |
| `agent.context_limit_fallback` | positive int | Context window assumed when catalog metadata is missing. |
| `agent.reasoning_effort` | enum | `default`, `off`, `low`, `medium`, or `high`. |
| `agent.mode` | `"coding"` \| `"plan"` | `plan` denies writes and sandboxes shell commands. |
| `agent.system_prompt_extra` | lines | Appended to the built-in system prompt. |
| `shell.default_timeout_ms` | unsigned int | Default shell timeout (ms); `0` disables it. |
| `shell.max_output_bytes` | positive int | Captured shell output cap. |
| `shell.deny_env` | list of string (`[]`) | Env vars stripped from every tool shell. |
| `ui.theme` | `"pi"` \| `"ansi"` | Terminal theme. |
| `ui.max_image_encoded_bytes` | positive int | Encoded image attachment size cap. |
| `ui.mouse` | bool | Enable mouse capture for wheel scrolling and in-app selection. |
| `ui.show_reasoning` | bool | Render provider reasoning in dim italic text. |
| `skills.paths` | list of path (`[]`) | **User layer only**; extra trusted skill roots. |
| `tools.disabled` | list of string (`[]`) | Tools withheld from the model and all child agents. |
| `agents.enabled` | bool | Allow spawning child agents (on by default). |
| `agents.max_concurrent` | positive int | Max simultaneously running children. |
| `agents.max_spawns_per_turn` | positive int | Max children spawned in one root turn. |
| `agents.worktree_root` | string | Base directory for isolated worktrees (defaults to state dir). |
| `agents.shared_target_dir` | string | `CARGO_TARGET_DIR` exported to children to share one build cache. |
| `agents.wake_on_finish` | bool | Wake an idle agent when a child finishes. |
| `agents.roles.<name>.instructions` | lines | Extra instructions prepended to the role's prompt. |
| `agents.roles.<name>.tools` | list of string (`[]`) | Allowed tools; empty inherits all parent tools. |
| `agents.roles.<name>.model` | string | Role model; unset inherits the parent's model. |
| `agents.roles.<name>.isolation` | `"shared"` \| `"worktree"` | `worktree` gives the child its own `HEAD` checkout. |
| `mcp.startup_timeout_ms` | positive int | Concurrent MCP server startup deadline (ms). |
| `mcp.servers.<name>.command` | list of string | **User layer only.** Local stdio command; exclusive with `url`. |
| `mcp.servers.<name>.cwd` | string | Local server working directory (relative to project root unless absolute). |
| `mcp.servers.<name>.pass_env` | list of string (`[]`) | Env var *names* passed to a local server (never values). |
| `mcp.servers.<name>.url` | string | **User layer only.** Remote Streamable HTTP URL; exclusive with `command`. |
| `mcp.servers.<name>.bearer_token_env` | string | Env var *name* holding a remote bearer token. |
| `mcp.servers.<name>.headers` | attrs of string (`{}`) | Extra headers mapped to env var *names*. |
| `mcp.servers.<name>.plan` | `"read-only"` \| `"mutating"` | `read-only` allows the server's tools in plan mode. |

### System-layer limits

t8ful strips `provider.base_url`, `skills.paths`, and `[mcp.servers.*]` from system configuration because those settings redirect credentials, widen sandbox read mounts, or launch programs. The NixOS module therefore omits them (only `mcp.startup_timeout_ms` is available in system scope). Set them in Home Manager, `T8FUL_BASE_URL`, or an explicit `--config` file.

### Subagent role notes

`agents.roles.<name>.tools` is a convenience filter, not a security boundary. A role's list is intersected with the parent's tools (so a child can never gain tools the parent lacks), and `spawn_agents`, `agent_wait`, `agent_status`, `ask_user_question`, and `present_plan` are always excluded (naming them in a role triggers a warning and is ignored). A role with `bash` can still do anything `bash` can.

Pair write-capable roles with `isolation = "worktree"` to give the child its own Git worktree at `HEAD`. A dirty working tree causes the spawn to fail rather than silently dropping uncommitted changes. t8ful never merges or commits child work — the report gives you the worktree path and branch to integrate yourself.

## Credentials

Because the Nix store is world-readable, never put API keys or tokens in `programs.t8ful.settings`, `home.sessionVariables`, or any Nix-managed file. Provide `OPENROUTER_API_KEY` in the runtime environment (shell profile, `direnv`, or a runtime secret manager) or save it interactively with `/login`.

## Project trust

Trusting a project's `.t8ful/config.toml` is an interactive runtime decision stored in `$XDG_STATE_HOME/t8ful/trust.json`, bound to the config file's SHA-256 hash. It is deliberately not configurable through Nix. Manage saved decisions with `t8ful trust list` and `t8ful trust revoke`.

## MCP servers

Configure MCP servers with environment variable *names* only:

```nix
programs.t8ful.settings.mcp.servers = {
  filesystem = {
    command = ["npx" "-y" "@modelcontextprotocol/server-filesystem" "/work"];
    pass_env = ["HOME"];
    plan = "read-only";
  };
  linear = {
    url = "https://mcp.linear.app/mcp";
    bearer_token_env = "LINEAR_TOKEN";
  };
};
```

Export `LINEAR_TOKEN` in the shell that launches t8ful. If a referenced variable is missing at startup, that server fails with an error naming the variable. Every configured credential variable is also added to the shell deny list and withheld from local plan-mode servers.

Discovered tools appear as `mcp__<server>__<tool>`. In plan mode, a server's tools only run if `plan = "read-only"` is set; otherwise calls are denied with a message naming the setting. Local servers run inside the plan-mode Bubblewrap sandbox (no network, no writable paths), so launchers like `npx` typically fail there — use a remote `url` for servers you need while planning.

## Pinned skills

`programs.t8ful.skills` accepts any store path:

```nix
programs.t8ful.skills = {
  commit = pkgs.fetchFromGitHub {
    owner = "example";
    repo = "t8ful-skill-commit";
    rev = "0123456789abcdef0123456789abcdef01234567";
    hash = "sha256-...";
  };

  review = pkgs.fetchgit {
    url = "https://example.com/t8ful-skill-review.git";
    rev = "fedcba9876543210fedcba9876543210fedcba98";
    hash = "sha256-...";
  };
};
```

Skill names must be a single path segment using letters, digits, `.`, `_`, or `-` (and not `.` or `..`); invalid names fail an assertion.

Skill directories are trusted read roots for the plan-mode sandbox (`skills.paths` adds more). `t8ful skills list` shows both declarative links and installed skills, and `t8ful skills install` / `update` refuse to overwrite Nix-managed links.

## Configuration precedence

Highest priority first:

1. CLI flags (`--model`, `--mode`, `--max-steps`, …)
2. Environment variables (`T8FUL_MODEL`, `T8FUL_BASE_URL`, `T8FUL_MODE`, …)
3. Explicit `--config <path>`
4. Project `<project-root>/.t8ful/config.toml`
5. User `$XDG_CONFIG_HOME/t8ful/config.toml` (Home Manager)
6. System `/etc/xdg/t8ful/config.toml` (NixOS)
7. Built-in defaults

## Evaluation check

`nix flake check` runs `checks.<system>.t8ful-modules`, which evaluates both modules without activation, network, API keys, or a system rebuild, and verifies that:

- enabling installs `t8ful` and disabling does not;
- only configured settings appear in the generated `config.toml`;
- the system module never exposes `provider.base_url` or `skills.paths`;
- pinned skills are linked individually without disturbing writable neighbor skills;
- skill names containing a path separator fail an assertion.
