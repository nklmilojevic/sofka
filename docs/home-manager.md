# Home Manager

The Sofka flake exports `homeManagerModules.sofka` and
`homeManagerModules.default`. Both names select the same module. No overlay is
required. The module supports Linux and macOS.

Add Sofka to your flake inputs:

```nix
inputs.sofka.url = "github:nklmilojevic/sofka";
```

CI builds every commit on `main` and every release for Linux and macOS and
pushes the result to the `nkl-sofka` Cachix cache. Add it to your Nix settings
to substitute Sofka instead of compiling it:

```nix
nix.settings = {
  extra-substituters = [ "https://nkl-sofka.cachix.org" ];
  extra-trusted-public-keys = [ "nkl-sofka.cachix.org-1:hLg9frFNJynrxe7SSBb/p6pbawlpZmG10bw+wLsTufw=" ];
};
```

The cache only matches builds against the Sofka flake's own nixpkgs. Do not set
`inputs.sofka.inputs.nixpkgs.follows`. With it, the package builds against your
nixpkgs and compiles locally. Outside Home Manager, use
`inputs.sofka.packages.${system}.sofka`, not the overlay. The overlay always
builds against your nixpkgs.

Add the module to your Home Manager configuration. Here, `inputs` is the input
set from your flake outputs function. You can pass it to Home Manager through
`extraSpecialArgs = { inherit inputs; };`.

```nix
{ inputs, ... }:
{
  imports = [ inputs.sofka.homeManagerModules.default ];

  programs.sofka = {
    enable = true;
    settings = {
      default_namespace = "default";
      default_resource = "deployments";
      readonly = true;
      favorite_namespaces = [ "kube-system" "monitoring" ];
    };
    aliases.dep = "deployments";
    keys.table.sort_age = "A";
    keys.logs.log_marker = "m";
    skin = {
      name = "gruvbox-dark";
      colors.red = "#fb4934";
    };
    views."*".sort = "AGE:desc";
    plugins = [
      {
        name = "Get pods";
        palette = "get-pods";
        command = "kubectl";
        args = [ "get" "pods" "--context" "$CONTEXT" "-n" "$NAMESPACE" ];
        target = "context";
        mutating = false;
        output = "popup";
      }
    ];
  };
}
```

## Options

All options are under `programs.sofka`.

| Option       | Default                      | Purpose                                                          |
| ------------ | ---------------------------- | ---------------------------------------------------------------- |
| `enable`     | `false`                      | Install Sofka and manage the selected files.                     |
| `package`    | Package from the Sofka flake | Select a package, or use `null` to skip installation.            |
| `settings`   | `{ }`                        | Set any field in the complete Sofka TOML configuration.          |
| `aliases`    | `{ }`                        | Merge into `settings.aliases`.                                   |
| `keys`       | `{ }`                        | Merge into `settings.keys`.                                      |
| `plugins`    | `[ ]`                        | Merge into `settings.plugins`.                                   |
| `views`      | `{ }`                        | Merge into `settings.views`.                                     |
| `skin`       | `{ }`                        | Merge into `settings.skin`.                                      |
| `configFile` | `null`                       | Use an existing TOML or YAML file instead of generated settings. |
| `clusters`   | `{ }`                        | Manage cluster and context override files.                       |

`settings` accepts the full [configuration](configuration.md), including
providers, logging, guardrails, bookmarks, and new settings added to Sofka.
The module checks TOML value types. Sofka checks application settings when it
loads the file.

Dedicated options and `settings` use normal Nix module merging: tables merge by
key, lists combine in module order, and conflicting scalar values produce an
error. Use `lib.mkBefore` or `lib.mkAfter` on lists in `settings` to control
their order. Use `lib.mkForce` on a field in `settings` to replace other
definitions, including values from dedicated options.

```nix
{ lib, ... }:
{
  programs.sofka.settings.skin.name = lib.mkForce "nord";
}
```

The module writes generated aliases, keys, plugins, views, and skin settings
to one TOML file. Existing YAML files support the same settings.
The module uses the native names `keys` and `skin`; it does not generate k9s
hotkey or skin files.

## Existing files and package selection

To manage an existing complete TOML file:

```nix
programs.sofka = {
  enable = true;
  configFile = ./sofka.toml;
};
```

For an existing YAML file, use `configFile = ./sofka.yaml;` or
`configFile = ./sofka.yml;`. The module uses the source suffix to select
`config.yaml` or `config.yml` at that level. Generated `settings` still use
TOML. Remove any separately managed config file at the same level to avoid
a file conflict.

`configFile` cannot be combined with nonempty `settings` or dedicated options
for the same file. It can be combined with cluster and context overrides.

To manage configuration without installing a package, set
`programs.sofka.package = null;`. To select another package, set
`programs.sofka.package = pkgs.sofka;` if your package set provides it.

With only `enable = true`, the module installs Sofka and leaves the main config
file unmanaged. When the module is disabled, it installs no package and manages
no files.

## Cluster and context overrides

Each cluster and context accepts `settings` or a TOML or YAML `configFile`. Clusters also
accept `contexts`:

```nix
programs.sofka.clusters = {
  prod-cluster = {
    settings.readonly = true;
    contexts.prod-admin.settings.skin.name = "catppuccin-latte";
  };
  staging = {
    configFile = ./staging.toml;
    contexts.staging-admin.configFile = ./staging-admin.toml;
  };
};
```

This creates files under `sofka/clusters/<cluster>/config.toml` and
`sofka/clusters/<cluster>/<context>/config.toml`. An existing YAML source uses
`config.yaml` or `config.yml` instead. Empty settings create no file.
A context file does not require a cluster file.

Attribute names must be directory names after
[Sofka's name conversion](configuration.md#per-cluster-and-per-context-overrides).
Replace each character outside `A-Z`, `a-z`, `0-9`, `.`, `_`, and `-` with `-`.
For example, `arn:aws:eks:eu-west-1:123:cluster/prod` becomes
`arn-aws-eks-eu-west-1-123-cluster-prod`. The module rejects unsafe names, empty
names, and names made only of dots. For an empty kubeconfig name, Sofka's
conversion produces `-`; for a name made only of dots, replace each dot with
`-`.

At runtime, Sofka applies the main config, then the cluster override, then the
context override. This is separate from Nix module merging: Sofka merges tables
by key and replaces other values, including arrays.

## Apply changes

Apply your Home Manager configuration, then enter `:reload` in Sofka. Settings
that require a restart retain that requirement.

Home Manager links files under `$XDG_CONFIG_HOME/sofka`, normally
`~/.config/sofka`, on both Linux and macOS. A custom `xdg.configHome` changes
this location. Managed files are read-only; edit the Nix settings or source
TOML or YAML file and apply Home Manager again. Sofka keeps session state separate from
these files.

The module checks are available through
`nix build .#checks.<system>.home-manager`. The specification is in
[issue #482](https://github.com/nklmilojevic/sofka/issues/482).
