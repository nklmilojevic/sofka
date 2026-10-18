{ pkgs, home-manager, module, package }:

let
  inherit (pkgs) lib;
  evaluateWith = module: extra: (home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      module
      {
        home.username = "sofka-test";
        home.homeDirectory = if pkgs.stdenv.hostPlatform.isDarwin then "/Users/sofka-test" else "/home/sofka-test";
        home.stateVersion = "26.05";
      }
      extra
    ];
  }).config;
  evaluate = evaluateWith module;
  standalone = evaluateWith ../home-manager.nix { programs.sofka.enable = true; };
  enabled = settings: evaluate { programs.sofka = { enable = true; package = null; } // settings; };
  fails = settings: !(builtins.tryEval (builtins.deepSeq (enabled settings).xdg.configFile true)).success;
  disabled = evaluate { programs.sofka.settings.readonly = true; };
  default = evaluate { programs.sofka.enable = true; };
  empty = enabled { };
  replacement = pkgs.writeShellScriptBin "sofka" "exit 0";
  overridden = enabled { package = replacement; };
  full = evaluate {
    xdg.configHome = "/custom/sofka-config";
    programs.sofka = {
      enable = true;
      package = null;
      settings = {
        readonly = true;
        favorite_namespaces = [ "kube-system" "monitoring" ];
        aliases.po = "pods";
        keys.table.sort_age = "A";
        providers.metrics.url = "https://metrics.example.test";
        plugins = lib.mkBefore [{ name = "first"; command = "true"; }];
      };
      aliases.dep = "deployments";
      keys.logs.log_marker = "m";
      keys.table.favorite_namespace_2 = [ ];
      plugins = [{ name = "second"; command = "kubectl"; args = [ "get" "pods" ]; }];
      views."*".sort = "AGE:desc";
      skin = { name = "gruvbox-dark"; colors.red = "#fb4934"; };
      clusters.prod = {
        settings.readonly = true;
        contexts.admin.settings.skin.name = "catppuccin-latte";
      };
      clusters.arn-aws-eks-eu-west-1-123-cluster-prod.contexts.team.settings.default_namespace = "team";
    };
  };
  raw = enabled {
    configFile = ./config.toml;
    clusters.prod = {
      configFile = ./config.toml;
      contexts.admin.configFile = ./config.toml;
    };
  };
  yamlSource = builtins.toFile "sofka-config.yaml" "readonly: true\ndefault_namespace: from-yaml\n";
  ymlSource = builtins.toFile "sofka-config.yml" "readonly: false\n";
  yaml = enabled {
    configFile = yamlSource;
    clusters.prod = {
      configFile = ymlSource;
      contexts.admin.configFile = yamlSource;
      contexts.other.settings.readonly = true;
    };
  };
  forced = enabled {
    settings.skin.name = lib.mkForce "nord";
    skin.name = "gruvbox-dark";
  };
  fixtures = {
    main = {
      source = full.xdg.configFile."sofka/config.toml".source;
      expected = {
        readonly = true;
        favorite_namespaces = [ "kube-system" "monitoring" ];
        aliases = { po = "pods"; dep = "deployments"; };
        keys = { table = { sort_age = "A"; favorite_namespace_2 = [ ]; }; logs.log_marker = "m"; };
        providers.metrics.url = "https://metrics.example.test";
        plugins = [{ name = "first"; command = "true"; } { name = "second"; command = "kubectl"; args = [ "get" "pods" ]; }];
        views."*".sort = "AGE:desc";
        skin = { name = "gruvbox-dark"; colors.red = "#fb4934"; };
      };
    };
    cluster = { source = full.xdg.configFile."sofka/clusters/prod/config.toml".source; expected.readonly = true; };
    context = { source = full.xdg.configFile."sofka/clusters/prod/admin/config.toml".source; expected.skin.name = "catppuccin-latte"; };
    contextOnly = { source = full.xdg.configFile."sofka/clusters/arn-aws-eks-eu-west-1-123-cluster-prod/team/config.toml".source; expected.default_namespace = "team"; };
    forced = { source = forced.xdg.configFile."sofka/config.toml".source; expected.skin.name = "nord"; };
    raw = {
      source = raw.xdg.configFile."sofka/config.toml".source;
      expected = { readonly = true; default_namespace = "from-file"; skin.name = "nord"; };
    };
  };
  noSofkaFiles = cfg: builtins.all (name: !(lib.hasPrefix "sofka/" name)) (builtins.attrNames cfg.xdg.configFile);
in
assert noSofkaFiles disabled;
assert !(builtins.elem package disabled.home.packages);
assert noSofkaFiles default;
assert builtins.elem package default.home.packages;
assert standalone.programs.sofka.package.pname == package.pname;
assert builtins.elem standalone.programs.sofka.package standalone.home.packages;
assert builtins.isString standalone.programs.sofka.package.drvPath;
assert noSofkaFiles empty;
assert !(builtins.elem package empty.home.packages);
assert builtins.elem replacement overridden.home.packages;
assert !(builtins.elem package overridden.home.packages);
assert full.xdg.configFile."sofka/config.toml".target == "/custom/sofka-config/sofka/config.toml";
assert !(full.xdg.configFile ? "sofka/clusters/arn-aws-eks-eu-west-1-123-cluster-prod/config.toml");
assert builtins.all (name: raw.xdg.configFile.${name}.source == ./config.toml) [
  "sofka/config.toml"
  "sofka/clusters/prod/config.toml"
  "sofka/clusters/prod/admin/config.toml"
];
assert yaml.xdg.configFile."sofka/config.yaml".source == yamlSource;
assert yaml.xdg.configFile."sofka/clusters/prod/config.yml".source == ymlSource;
assert yaml.xdg.configFile."sofka/clusters/prod/admin/config.yaml".source == yamlSource;
assert yaml.xdg.configFile ? "sofka/clusters/prod/other/config.toml";
assert !(yaml.xdg.configFile ? "sofka/config.toml");
assert !(yaml.xdg.configFile ? "sofka/clusters/prod/config.toml");
assert fails { configFile = yamlSource; settings.readonly = true; };
assert fails { configFile = ./config.toml; settings.readonly = true; };
assert fails { configFile = ./config.toml; aliases.dep = "deployments"; };
assert fails { clusters.prod = { configFile = ./config.toml; settings.readonly = true; }; };
assert fails { clusters.prod.contexts.admin = { configFile = ./config.toml; settings.readonly = true; }; };
assert fails { settings.skin.name = "nord"; skin.name = "gruvbox-dark"; };
assert builtins.all (name: fails { clusters.${name}.settings.readonly = true; }) [ "" "." ".." "..." "../prod" "arn:aws:eks" ];
assert fails { clusters.prod.contexts."../admin".settings.readonly = true; };
pkgs.runCommand "sofka-home-manager-check"
{
  nativeBuildInputs = [ pkgs.python3 ];
  manifest = pkgs.writeText "sofka-home-manager-fixtures.json" (builtins.toJSON fixtures);
} ''
  python - "$manifest" <<'PY'
  import json
  import sys
  import tomllib

  with open(sys.argv[1]) as manifest:
      fixtures = json.load(manifest)
  for name, fixture in fixtures.items():
      with open(fixture["source"], "rb") as source:
          actual = tomllib.load(source)
      assert actual == fixture["expected"], (name, actual, fixture["expected"])
  PY
  touch "$out"
''
