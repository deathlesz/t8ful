{
  description = "t8ful — a fast, personal Rust coding agent";

  outputs = {flake-parts, ...} @ inputs:
    flake-parts.lib.mkFlake {inherit inputs;} ({self, ...}: {
      flake = {
        nixosModules.t8ful = import ./nix/modules/nixos.nix {self = self;};
        homeModules.t8ful = import ./nix/modules/home-manager.nix {self = self;};
      };

      perSystem = {system, ...}: let
        pkgs = import inputs.nixpkgs {
          inherit system;

          overlays = [inputs.rust-overlay.overlays.default];
        };

        rust = pkgs.rust-bin.stable.latest.minimal.override {
          extensions = ["clippy" "rust-analyzer"];
        };

        # separate to support unstable features
        rustfmt = pkgs.rust-bin.nightly.latest.rustfmt;

        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

        # Runtime tools the binary shells out to: bash for the shell tool, git
        # for root discovery, coreutils for ordinary commands run through the
        # wrapped environment, and libnotify for desktop notifications.
        runtimeTools = [pkgs.bash pkgs.git pkgs.coreutils pkgs.bubblewrap pkgs.libnotify];

        t8ful = pkgs.rustPlatform.buildRustPackage {
          pname = "t8ful";
          inherit version;

          # Only build inputs belong in the store: the workspace manifests,
          # the lock file, and Rust sources. Untracked credentials such as a
          # root `.env` are also ignored by Git, but the explicit filter keeps
          # unrelated working-tree files out even when this is not a checkout.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type: let
              name = baseNameOf path;
              relative = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
              isDirectory = type == "directory";
              # Test fixtures are inputs too: `t8ful-mcp` spawns a shell-script
              # MCP server, and a build that silently omitted it would fail the
              # hermetic suite inside the sandbox rather than here.
              isBuildInput =
                name
                == "Cargo.toml"
                || name == "Cargo.lock"
                || pkgs.lib.hasSuffix ".rs" relative
                || pkgs.lib.hasPrefix "crates/t8ful-mcp/tests/support/" relative;
            in
              if isDirectory
              then !(builtins.elem name [".git" ".direnv" "target" "result"])
              else isBuildInput;
          };

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [pkgs.makeWrapper];
          nativeCheckInputs = runtimeTools ++ [pkgs.python3];

          postInstall = ''
            wrapProgram $out/bin/t8ful \
              --suffix PATH : ${pkgs.lib.makeBinPath runtimeTools}
          '';

          meta = {
            description = "A fast, personal Rust coding agent for the terminal";
            homepage = "https://github.com/deathlesz/t8ful";
            license = with pkgs.lib.licenses; [mit asl20];
            mainProgram = "t8ful";
          };
        };

        # Evaluate both modules and assert what they produce. Only text
        # derivations are involved, so this stays cheap and hermetic: no
        # activation, no system rebuild, no network.
        t8fulModulesCheck = let
          inherit (pkgs) lib;

          isLinux = pkgs.stdenv.hostPlatform.isLinux;

          # A pinned skill source, the shape a flake input or fetchFromGitHub
          # checkout has. The modules only ever link to store paths.
          skill = pkgs.writeTextDir "SKILL.md" ''
            ---
            name = "demo"
            description = "Demo skill used by the t8ful module evaluation check."
            ---

            # Demo skill

            Used by the t8ful module evaluation check.
          '';

          homeBase = {
            home.username = "t8ful-check";
            home.homeDirectory = "/home/t8ful-check";
            home.stateVersion = "24.11";
          };

          homeModule = self.homeModules.t8ful;

          mkHome = modules:
            inputs.home-manager.lib.homeManagerConfiguration {
              inherit pkgs;
              modules = [homeModule homeBase] ++ modules;
            };

          homeEnabled = mkHome [
            {
              programs.t8ful = {
                enable = true;
                settings = {
                  provider.model = "test/model";
                  agent = {
                    reasoning_effort = "high";
                    context_limit = 64000;
                  };
                  ui = {
                    theme = "ansi";
                    mouse = false;
                    show_reasoning = true;
                  };
                  agents = {
                    max_concurrent = 3;
                    wake_on_finish = false;
                    roles.researcher = {
                      instructions = "Only read.";
                      tools = ["read_file" "bash"];
                      isolation = "shared";
                    };
                  };
                  mcp = {
                    startup_timeout_ms = 2500;
                    servers.filesystem = {
                      command = ["npx" "-y" "@modelcontextprotocol/server-filesystem" "/work"];
                      pass_env = ["HOME"];
                      plan = "read-only";
                    };
                    servers.linear = {
                      url = "https://mcp.linear.app/mcp";
                      bearer_token_env = "LINEAR_TOKEN";
                    };
                  };
                };
                skills.demo = skill;
              };
            }
          ];

          homeDisabled = mkHome [{programs.t8ful.enable = false;}];

          # A writable skill directory next to the module-managed link: the
          # module must not claim the whole skills root.
          homeWritable = mkHome [
            {
              programs.t8ful = {
                enable = true;
                skills.demo = skill;
              };
              home.file.".config/t8ful/skills/writable/SKILL.md".text = ''
                A skill the user installs by hand.
              '';
            }
          ];

          homeTraversal = mkHome [
            {
              programs.t8ful = {
                enable = true;
                skills."../escape" = skill;
              };
            }
          ];

          homeConfig = homeEnabled.config;
          homeFileTargets = map (entry: entry.target) (lib.attrValues homeWritable.config.home.file);
          duplicateTargets = lib.length homeFileTargets != lib.length (lib.unique homeFileTargets);

          nixos = lib.optionalAttrs isLinux (let
            mkSystem = modules:
              inputs.nixpkgs.lib.nixosSystem {
                modules =
                  [
                    self.nixosModules.t8ful
                    {
                      nixpkgs.hostPlatform = system;
                      system.stateVersion = "24.11";
                    }
                  ]
                  ++ modules;
              };

            enabled = mkSystem [
              {
                programs.t8ful = {
                  enable = true;
                  settings = {
                    provider.model = "test/model";
                    agent.reasoning_effort = "medium";
                  };
                  skills.demo = skill;
                };
              }
            ];

            disabled = mkSystem [{programs.t8ful.enable = false;}];

            traversal = mkSystem [
              {
                programs.t8ful = {
                  enable = true;
                  skills."../escape" = skill;
                };
              }
            ];
          in {
            inherit enabled disabled traversal;
          });

          nixosConfig = nixos.enabled.config;

          failedAssertion = config: lib.any (entry: entry.assertion == false) config.assertions;

          # Inspect the declared options directly: the system scope must not
          # offer the settings t8ful strips from system configuration.
          settingsFor = scope:
            import ./nix/modules/settings.nix {
              inherit lib;
              inherit scope;
            };

          optionNames = scope: path: let
            top = (settingsFor scope).options.programs.t8ful.settings.type.getSubOptions [];
            found = lib.attrByPath path null top;
          in
            if found == null
            then ["<missing>"]
            else builtins.attrNames found;

          # Home Manager enforces assertions while the configuration is
          # evaluated, so the rejected name shows up as a failed evaluation.
          homeTraversalRejected = !(builtins.tryEval homeTraversal.config.home.username).success;

          failures = lib.filter (message: message != null) (
            [
              (
                if lib.elem t8ful homeConfig.home.packages
                then null
                else "Home Manager: the t8ful package is missing from home.packages"
              )

              (
                if homeConfig.xdg.configFile ? "t8ful/config.toml"
                then null
                else "Home Manager: enabled configuration does not write t8ful/config.toml"
              )

              (
                if homeConfig.xdg.configFile ? "t8ful/skills/demo"
                then null
                else "Home Manager: enabled configuration does not link t8ful/skills/demo"
              )

              (
                if lib.elem "base_url" (optionNames "user" ["provider"])
                then null
                else "Home Manager: the user scope does not offer provider.base_url"
              )

              (
                if lib.elem "paths" (optionNames "user" ["skills"])
                then null
                else "Home Manager: the user scope does not offer skills.paths"
              )

              (
                if lib.elem "reasoning_effort" (optionNames "user" ["agent"])
                then null
                else "Home Manager: the user scope does not offer agent.reasoning_effort"
              )

              (
                if lib.elem "context_limit" (optionNames "user" ["agent"])
                then null
                else "Home Manager: the user scope does not offer agent.context_limit"
              )

              (
                if lib.elem "reasoning_effort" (optionNames "system" ["agent"])
                then null
                else "NixOS: the system scope does not offer agent.reasoning_effort"
              )

              (
                if lib.elem "context_limit" (optionNames "system" ["agent"])
                then null
                else "NixOS: the system scope does not offer agent.context_limit"
              )

              (
                if lib.elem "approval" (optionNames "user" [])
                then "Home Manager: the user scope still offers a removed approval section"
                else null
              )

              (
                if lib.elem "approval" (optionNames "system" [])
                then "NixOS: the system scope still offers a removed approval section"
                else null
              )

              (
                if lib.elem "base_url" (optionNames "system" ["provider"])
                then "NixOS: the system scope offers provider.base_url, which t8ful strips from system configuration"
                else null
              )

              (
                if lib.elem "skills" (optionNames "system" [])
                then "NixOS: the system scope offers skills.paths, which t8ful strips from system configuration"
                else null
              )

              (
                if "${homeConfig.xdg.configFile."t8ful/skills/demo".source}" == "${skill}"
                then null
                else "Home Manager: t8ful/skills/demo does not point at the pinned skill source"
              )

              (
                if homeDisabled.config.xdg.configFile ? "t8ful/config.toml"
                then "Home Manager: disabled configuration still writes t8ful/config.toml"
                else null
              )

              (
                if lib.elem t8ful homeDisabled.config.home.packages
                then "Home Manager: disabled configuration still installs t8ful"
                else null
              )

              (
                if homeWritable.config.xdg.configFile ? "t8ful/skills/demo"
                then null
                else "Home Manager: the module skill link is missing when a writable skill is also installed"
              )

              (
                if homeWritable.config.home.file ? ".config/t8ful/skills/writable/SKILL.md"
                then null
                else "Home Manager: the user-installed writable skill is missing"
              )

              (
                if duplicateTargets
                then "Home Manager: two entries target the same file"
                else null
              )

              (
                if homeTraversalRejected
                then null
                else "Home Manager: a skill name with a path separator did not fail an assertion"
              )
            ]
            ++ lib.optionals isLinux [
              (
                if lib.elem t8ful nixosConfig.environment.systemPackages
                then null
                else "NixOS: the t8ful package is missing from environment.systemPackages"
              )

              (
                if nixosConfig.environment.etc ? "xdg/t8ful/config.toml"
                then null
                else "NixOS: enabled configuration does not write /etc/xdg/t8ful/config.toml"
              )

              (
                if nixosConfig.environment.etc ? "xdg/t8ful/skills/demo"
                then null
                else "NixOS: enabled configuration does not link /etc/xdg/t8ful/skills/demo"
              )

              (
                if "${nixosConfig.environment.etc."xdg/t8ful/skills/demo".source}" == "${skill}"
                then null
                else "NixOS: /etc/xdg/t8ful/skills/demo does not point at the pinned skill source"
              )

              (
                if nixos.disabled.config.environment.etc ? "xdg/t8ful/config.toml"
                then "NixOS: disabled configuration still writes /etc/xdg/t8ful/config.toml"
                else null
              )

              (
                if lib.elem t8ful nixos.disabled.config.environment.systemPackages
                then "NixOS: disabled configuration still installs t8ful"
                else null
              )

              (
                if failedAssertion nixos.traversal.config
                then null
                else "NixOS: a skill name with a path separator did not fail an assertion"
              )
            ]
          );

          env =
            {
              homeConfig = homeEnabled.config.xdg.configFile."t8ful/config.toml".source;
            }
            // lib.optionalAttrs isLinux {
              nixosToml = nixosConfig.environment.etc."xdg/t8ful/config.toml".source;
            };
        in
          if failures != []
          then throw "t8ful module check failed:\n${lib.concatStringsSep "\n" failures}"
          else
            pkgs.runCommand "t8ful-modules-check" env ''
              set -eu

              # The user layer reads the settings it is given and writes nothing
              # for the keys that stay unset.
              grep -F '[provider]' "$homeConfig"
              grep -F 'model = "test/model"' "$homeConfig"
              grep -F 'reasoning_effort = "high"' "$homeConfig"
              grep -F 'context_limit = 64000' "$homeConfig"
              grep -F '[ui]' "$homeConfig"
              grep -F 'theme = "ansi"' "$homeConfig"
              grep -F 'mouse = false' "$homeConfig"
              grep -F 'show_reasoning = true' "$homeConfig"
              grep -F '[agents]' "$homeConfig"
              grep -F 'max_concurrent = 3' "$homeConfig"
              grep -F 'wake_on_finish = false' "$homeConfig"
              grep -F '[agents.roles.researcher]' "$homeConfig"
              grep -F 'isolation = "shared"' "$homeConfig"

              # MCP servers are written with variable names, never values.
              grep -F '[mcp]' "$homeConfig"
              grep -F 'startup_timeout_ms = 2500' "$homeConfig"
              grep -F '[mcp.servers.filesystem]' "$homeConfig"
              grep -F 'pass_env = [' "$homeConfig"
              grep -F 'plan = "read-only"' "$homeConfig"
              grep -F '[mcp.servers.linear]' "$homeConfig"
              grep -F 'bearer_token_env = "LINEAR_TOKEN"' "$homeConfig"

              for forbidden in base_url catalog_timeout_ms system_prompt_extra '[skills]' approval; do
                if grep -F "$forbidden" "$homeConfig"; then
                  echo "user configuration unexpectedly contains $forbidden" >&2
                  exit 1
                fi
              done

              ${lib.optionalString isLinux ''
                # The system layer can only write settings t8ful accepts from
                # system configuration.
                grep -F 'model = "test/model"' "$nixosToml"
                grep -F 'reasoning_effort = "medium"' "$nixosToml"

                # t8ful refuses MCP server definitions from system
                # configuration, so the module must not offer the option there.
                for forbidden in base_url 'skills.paths' approval '[mcp.servers' ; do
                  if grep -F "$forbidden" "$nixosToml"; then
                    echo "system configuration unexpectedly contains $forbidden" >&2
                    exit 1
                  fi
                done
              ''}

              touch $out
            '';
      in {
        _module.args.pkgs = pkgs;

        packages = {
          inherit t8ful;
          default = t8ful;
        };

        apps.default = {
          type = "app";
          program = "${t8ful}/bin/t8ful";
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs =
            (with pkgs; [
              python314
              # Bubblewrap is the packaged runtime backend and the only one the
              # sandbox crate targets. nsjail stays available in the shell for
              # experiments with an alternative backend, but nothing uses it.
              bubblewrap
              nsjail
              # `cargo run` should find the same notifier the package wraps.
              libnotify
            ])
            ++ [
              rust
              rustfmt
            ];
        };

        checks.t8ful-modules = t8fulModulesCheck;
      };

      systems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];
    });

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-parts.url = "github:hercules-ci/flake-parts";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
}
