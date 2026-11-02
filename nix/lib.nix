# Helpers shared by the NixOS and Home Manager modules.
{lib}: let
  inherit (builtins) attrNames isAttrs isList length mapAttrs;

  isEmpty = value:
    value
    == null
    || (isList value && value == [])
    || (isAttrs value && length (attrNames value) == 0);

  # Drop unset leaves and the sections they would leave empty, so a module
  # never writes a key it was not given and never overrides a lower config
  # layer with an empty value.
  prune = value:
    if isAttrs value
    then lib.filterAttrs (_: child: !(isEmpty child)) (mapAttrs (_: prune) value)
    else value;

  # Skill names become single path components under the skills root, so they
  # must not contain a separator or resolve to a parent directory.
  isValidSkillName = name:
    builtins.match "[A-Za-z0-9._-]+" name
    != null
    && name != "."
    && name != "..";
in {
  inherit prune isValidSkillName;

  # Render the settings as one TOML file, or null when nothing is configured.
  # A null result means the caller must not create a config file at all.
  render = {
    pkgs,
    settings,
  }: let
    withPaths =
      settings
      // {
        skills =
          (settings.skills or {})
          // {
            paths = map toString ((settings.skills or {}).paths or []);
          };
      };

    pruned = prune withPaths;
  in
    if pruned == {}
    then null
    else (pkgs.formats.toml {}).generate "t8ful-config.toml" pruned;

  invalidSkillNames = names: builtins.filter (name: !(isValidSkillName name)) names;
}
