# Home Manager module for t8ful.
#
# Writes the user layer: `$XDG_CONFIG_HOME/t8ful/config.toml` and one link per
# pinned skill under `$XDG_CONFIG_HOME/t8ful/skills`. Credentials are never
# written; t8ful reads `OPENROUTER_API_KEY` and `OPENCODE_GO_API_KEY` from
# the environment.
{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.t8ful;

  t8fulLib = import ../lib.nix {inherit lib;};

  rendered = t8fulLib.render {
    inherit pkgs;
    settings = cfg.settings;
  };

  invalidSkills = t8fulLib.invalidSkillNames (builtins.attrNames cfg.skills);
in {
  imports = [
    (import ./settings.nix {
      inherit lib;
      scope = "user";
    })
  ];

  options.programs.t8ful = {
    enable = lib.mkEnableOption "t8ful, a fast personal coding agent";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.t8ful;
      defaultText = lib.literalExpression "self.packages.\${pkgs.stdenv.hostPlatform.system}.t8ful";
      description = "The t8ful package to install.";
    };

    skills = lib.mkOption {
      type = lib.types.attrsOf lib.types.path;
      default = {};
      description = ''
        Pinned skill sources, linked one by one into
        `$XDG_CONFIG_HOME/t8ful/skills`. Linking each skill separately leaves the
        rest of that directory writable, so skills installed or edited by t8ful
        keep working alongside these.
      '';
      example = lib.literalExpression ''
        {
          commit = pkgs.fetchFromGitHub {
            owner = "example";
            repo = "t8ful-skill-commit";
            rev = "0123456789abcdef0123456789abcdef01234567";
            hash = "sha256-...";
          };
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = invalidSkills == [];
        message = ''
          programs.t8ful.skills contains names that are not single path
          components: ${lib.concatStringsSep ", " invalidSkills}. Use only
          letters, digits, `.`, `_` and `-`, and avoid `.` and `..`.
        '';
      }
    ];

    home.packages = [cfg.package];

    xdg.configFile = lib.mkMerge [
      (lib.mkIf (rendered != null) {
        "t8ful/config.toml".source = rendered;
      })

      (lib.mapAttrs' (name: source:
        lib.nameValuePair "t8ful/skills/${name}" {
          inherit source;
          recursive = true;
        })
      cfg.skills)
    ];
  };
}
