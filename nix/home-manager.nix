{ config, lib, pkgs, ... }:

let
  cfg = config.programs.sofka;
  toml = pkgs.formats.toml { };

  # Without the flake (a plain import of this module), package.nix still needs
  # craneLib; take crane from the revision flake.lock pins.
  craneLock = (lib.importJSON ../flake.lock).nodes.crane.locked;
  craneLib = import
    (builtins.fetchTarball {
      url = "https://github.com/${craneLock.owner}/${craneLock.repo}/archive/${craneLock.rev}.tar.gz";
      sha256 = craneLock.narHash;
    })
    { inherit pkgs; };

  fileOptions = {
    settings = lib.mkOption {
      type = toml.type;
      default = { };
      description = "Sofka settings to write as TOML. Tables merge by key and lists combine in module order.";
    };
    configFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Existing TOML or YAML file to use instead of generated settings at this level.";
    };
  };

  directoryType = lib.types.addCheck lib.types.str
    (name: builtins.match "[A-Za-z0-9._-]+" name != null && builtins.match "[.]+" name == null);

  namedFiles = options: lib.types.attrsOf (lib.types.submodule ({ name, ... }: {
    options = options // {
      directory = lib.mkOption {
        type = directoryType;
        default = name;
        readOnly = true;
        internal = true;
        description = "Directory name after Sofka's kubeconfig name conversion.";
      };
    };
  }));

  fileName = value:
    if value.configFile != null && lib.hasSuffix ".yaml" (toString value.configFile) then "config.yaml"
    else if value.configFile != null && lib.hasSuffix ".yml" (toString value.configFile) then "config.yml"
    else "config.toml";

  files = [{ path = "sofka/${fileName cfg}"; value = cfg; }]
    ++ lib.concatLists (lib.mapAttrsToList
    (_: cluster:
      [{ path = "sofka/clusters/${cluster.directory}/${fileName cluster}"; value = cluster; }]
        ++ lib.mapAttrsToList
        (_: context: {
          path = "sofka/clusters/${cluster.directory}/${context.directory}/${fileName context}";
          value = context;
        })
        cluster.contexts)
    cfg.clusters);

  sectionOption = section: type: default: lib.mkOption {
    inherit type default;
    description = "Settings to merge into programs.sofka.settings.${section}.";
  };
in
{
  options.programs.sofka = fileOptions // {
    enable = lib.mkEnableOption "Sofka";
    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = pkgs.callPackage ../package.nix { inherit craneLib; };
      defaultText = lib.literalExpression "the Sofka package from this flake";
      description = "Sofka package to install. Set null to manage configuration only.";
    };
    aliases = sectionOption "aliases" toml.type { };
    keys = sectionOption "keys" toml.type { };
    plugins = sectionOption "plugins" (lib.types.listOf toml.type) [ ];
    views = sectionOption "views" toml.type { };
    skin = sectionOption "skin" toml.type { };
    clusters = lib.mkOption {
      type = namedFiles (fileOptions // {
        contexts = lib.mkOption {
          type = namedFiles fileOptions;
          default = { };
          description = "Context overrides, keyed by directory name after Sofka's name conversion.";
        };
      });
      default = { };
      description = "Cluster overrides, keyed by directory name after Sofka's name conversion.";
    };
  };

  config = lib.mkIf cfg.enable {
    programs.sofka.settings = lib.mkMerge (map
      (section:
        lib.mkIf (cfg.${section} != (if section == "plugins" then [ ] else { })) {
          ${section} = cfg.${section};
        }) [ "aliases" "keys" "plugins" "views" "skin" ]);

    home.packages = lib.optional (cfg.package != null) cfg.package;

    assertions = map
      (file: {
        assertion = file.value.configFile == null || file.value.settings == { };
        message = "programs.sofka: ${file.path} cannot use both configFile and generated settings.";
      })
      files;

    xdg.configFile = builtins.listToAttrs (map
      (file: lib.nameValuePair file.path {
        source =
          if file.value.configFile != null then file.value.configFile
          else toml.generate "sofka-config.toml" file.value.settings;
      })
      (builtins.filter (file: file.value.configFile != null || file.value.settings != { }) files));
  };
}
