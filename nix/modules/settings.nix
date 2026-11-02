# Typed `programs.t8ful.settings` for both module systems.
#
# The option names mirror the `config.toml` schema t8ful reads. Every leaf is
# nullable and defaults to unset, so the modules only write what was asked for
# and never override another configuration layer with a default.
#
# `scope = "system"` omits the settings t8ful refuses to read from system
# configuration: only user configuration, the environment, or an explicitly
# selected file may select the provider endpoint or widen the skill roots.
{
  lib,
  scope,
}: let
  inherit (lib) mkOption optionalAttrs types;

  isSystem = scope == "system";

  optional = type: description:
    mkOption {
      type = types.nullOr type;
      default = null;
      description = description;
    };

  # The `[mcp]` options. `servers` is merged in with `//` inside this set
  # rather than at the top level, because a top-level `//` would replace the
  # whole subtree instead of adding one option to it.
  mcp = {
    startup_timeout_ms = optional types.ints.positive ''
      Per-server deadline for starting an MCP server and listing its tools, in
      milliseconds. Servers start concurrently, so this bounds the whole startup
      rather than each server in turn.
    '';

    servers = mkOption {
      type = types.attrsOf (types.submodule {
        options = {
          command = optional (types.listOf types.str) ''
            Program and arguments for a local server, for example
            `["npx" "-y" "@modelcontextprotocol/server-filesystem" "/work"]`.
            Mutually exclusive with `url`.
          '';
          cwd = optional types.str ''
            Working directory for a local server, relative to the project root
            unless absolute.
          '';
          pass_env = mkOption {
            type = types.listOf types.str;
            default = [];
            description = ''
              Environment variables a local server receives, given as variable
              *names* only. Never put a value here: the rendered file lives in
              the world-readable Nix store. t8ful reads the value from its own
              environment when it starts the server.
            '';
          };
          url = optional types.str ''
            Streamable HTTP endpoint for a remote server. Mutually exclusive
            with `command`.
          '';
          bearer_token_env = optional types.str ''
            Name of the environment variable holding the bearer token for a
            remote server.
          '';
          headers = mkOption {
            type = types.attrsOf types.str;
            default = {};
            description = ''
              Extra request headers for a remote server, mapped from header name
              to the *name* of the environment variable holding its value.
            '';
          };
          plan = optional (types.enum ["read-only" "mutating"]) ''
            Whether this server's tools may run in plan mode. `mutating` (the
            default) denies them there with an explanation naming the setting.
          '';
        };
      });
      default = {};
      description = ''
        MCP servers. Only names of environment variables belong here; a
        credential value would be copied into the Nix store. t8ful refuses
        server definitions from system configuration, so this option is not
        written in the system scope.
      '';
      example = lib.literalExpression ''
        {
          filesystem = {
            command = ["npx" "-y" "@modelcontextprotocol/server-filesystem" "/work"];
            pass_env = ["HOME"];
            plan = "read-only";
          };
          linear = {
            url = "https://mcp.linear.app/mcp";
            bearer_token_env = "LINEAR_TOKEN";
          };
        }
      '';
    };
  };

  # A system layer may not define servers, so the option is withheld there.
  scopedMcp =
    if isSystem
    then {inherit (mcp) startup_timeout_ms;}
    else mcp;
in {
  options.programs.t8ful.settings = mkOption {
    type = types.submodule {
      options =
        {
          provider =
            {
              kind = optional (types.enum ["openrouter"]) ''
                Provider implementation. t8ful currently supports `openrouter` only.
              '';
              model = optional types.str ''
                Default model identifier, for example `anthropic/claude-sonnet-4`.
                A model can also come from `--model` or `T8FUL_MODEL`. When neither
                is set, the terminal interface starts on its built-in default model
                and a headless run refuses to start.
              '';
              catalog_timeout_ms = optional types.ints.positive ''
                Deadline for the model catalog request, in milliseconds.
              '';
            }
            // optionalAttrs (!isSystem) {
              base_url = optional types.str ''
                Provider endpoint. t8ful ignores this from system configuration, so
                the option is only offered in the user scope.
              '';
            };

          agent = {
            max_steps = optional (types.ints.between 1 100000) ''
              Maximum number of tool steps in a single turn. Unlimited when unset.
            '';
            max_output_tokens = optional (types.ints.between 1 4294967295) ''
              Maximum output tokens requested from the provider.
            '';
            context_limit = optional types.ints.positive ''
              Explicit context window for the active model. Overrides catalog metadata.
            '';
            context_limit_fallback = optional types.ints.positive ''
              Context limit assumed when the provider does not report one.
            '';
            reasoning_effort = optional (types.enum ["default" "off" "low" "medium" "high"]) ''
              Reasoning effort sent to the provider. `default` sends no reasoning field.
            '';
            mode = optional (types.enum ["coding" "plan"]) ''
              Interaction mode. `plan` denies file mutation and runs shell commands in a sandbox.
            '';
            system_prompt_extra = optional types.lines ''
              Extra text appended to the built-in system prompt.
            '';
          };

          agents = {
            enabled = optional types.bool ''
              Whether the agent may start child agents. Enabled by default.
            '';
            max_concurrent = optional types.ints.positive ''
              Child agents allowed to run at the same time.
            '';
            max_spawns_per_turn = optional types.ints.positive ''
              Total child agents one root turn may start.
            '';
            worktree_root = optional types.str ''
              Directory isolated agents' worktrees are created under. Defaults to
              the state directory, deliberately outside the project.
            '';
            shared_target_dir = optional types.str ''
              A `CARGO_TARGET_DIR` exported to child agents, so parallel builders
              share one warm cache instead of each paying a full cold build.
            '';
            wake_on_finish = optional types.bool ''
              Start a turn on an idle agent to deliver a finished child's report.
              This spends money with nobody typing, so it can be turned off; the
              report is then delivered with the next message.
            '';
            roles = mkOption {
              type = types.attrsOf (types.submodule {
                options = {
                  instructions = optional types.lines ''
                    Extra instructions prepended to the child's system prompt.
                  '';
                  tools = mkOption {
                    type = types.listOf types.str;
                    default = [];
                    description = ''
                      Tools the role may use. Empty means every tool the parent has.
                      A named list is intersected with the parent's tools, so a role
                      can never gain a tool the parent lacks.
                    '';
                  };
                  model = optional types.str ''
                    Model for this role. Unset inherits the parent's model.
                  '';
                  isolation = optional (types.enum ["shared" "worktree"]) ''
                    Whether the role edits the parent's tree or gets its own Git
                    worktree based on HEAD.
                  '';
                };
              });
              default = {};
              description = ''
                Reusable child-agent profiles by name. `general` always exists.
              '';
            };
          };

          shell = {
            default_timeout_ms = optional types.ints.unsigned ''
              Default timeout for shell commands, in milliseconds. `0` allows a
              command to run without a timeout.
            '';
            max_output_bytes = optional types.ints.positive ''
              Maximum captured bytes of shell output.
            '';
            deny_env = mkOption {
              type = types.listOf types.str;
              default = [];
              description = ''
                Environment variables removed from every shell command t8ful runs.
              '';
            };
          };

          tools = {
            disabled = mkOption {
              type = types.listOf types.str;
              default = [];
              description = ''
                Tools withheld from the model. Unknown built-in names are a
                startup error; an `mcp__server__tool` name is resolved against
                the servers that actually started. Withholding a tool removes it
                from the provider's tool list and from every child agent's
                inherited tool set.
              '';
            };
          };

          mcp = scopedMcp;

          ui = {
            theme = optional (types.enum ["pi" "ansi"]) ''
              Terminal color theme.
            '';
            max_image_encoded_bytes = optional types.ints.positive ''
              Maximum size of an encoded image attachment, in bytes.
            '';
            mouse = optional types.bool ''
              Capture mouse events for wheel scrolling in the TUI.
            '';
            show_reasoning = optional types.bool ''
              Show provider reasoning as dim italic text.
            '';
          };
        }
        // optionalAttrs (!isSystem) {
          skills.paths = mkOption {
            type = types.listOf types.path;
            default = [];
            description = ''
              Extra trusted skill roots. t8ful ignores this from system
              configuration, so the option is only offered in the user scope.
            '';
          };
        };
    };
    default = {};
    description = ''
      Non-secret settings written to t8ful's `config.toml`. Unset options are not
      written at all, so lower-priority configuration layers keep their values.
      Credentials are never written: set `OPENROUTER_API_KEY` in the
      environment instead.
    '';
  };
}
