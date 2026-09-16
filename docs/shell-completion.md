# Shell completion

Run `sofka completion <shell>` to print a completion script. Supported shells
are `bash`, `zsh`, `fish`, `elvish`, and `powershell`.

The scripts complete CLI options, subcommands, and fixed option values. They
use the same definitions as `sofka --help`. They do not query Kubernetes for
resource names, namespaces, or contexts. Script generation does not load the
sofka configuration or connect to a cluster.

Use the instructions for your shell below. Make sure `sofka` is on `PATH` when
the shell starts. These commands generate the script at shell startup, so
completion stays in sync after a sofka update.

## Bash

Add this line to `~/.bashrc`:

```bash
source <(sofka completion bash)
```

## Zsh

Add these lines to `~/.zshrc`. If your shell setup already runs `compinit`, add
only the `source` line after that setup.

```zsh
autoload -Uz compinit
compinit
source <(sofka completion zsh)
```

## Fish

Add this line to `~/.config/fish/config.fish`:

```fish
sofka completion fish | source
```

## Elvish

Add this line to your Elvish `rc.elv` file:

```elvish
eval (sofka completion elvish | slurp)
```

## PowerShell

Add this line to your PowerShell profile (`$PROFILE`):

```powershell
sofka completion powershell | Out-String | Invoke-Expression
```

Restart your shell after you change its configuration. You can also run the
commands directly to enable completion in the current session.

`completion` is a CLI subcommand. To open a Kubernetes resource with that name,
use `sofka --resource completion`.
