# Install the CLI

The `okto` CLI runs on your **Linux host** (the machine that will run lair). It
is published per-architecture (`x86_64`, `aarch64`) as a GitHub release asset.

## One-line installer

```sh
curl -fsSL https://raw.githubusercontent.com/georgebradford0/okto/main/scripts/get-cli.sh | sh
```

The helper script downloads the right binary for your architecture, installs it
onto your `PATH`, and sets up shell completions. Once it finishes, verify:

```sh
okto version
```

!!! note "Linux only"
    The CLI builds exist only for `linux/x86_64` and `linux/aarch64`. There are
    no macOS or Windows builds — the host that runs lair must be Linux.

## Shell completions

`okto init` installs completions automatically. To (re)generate them yourself,
print the script for your shell to stdout and source it where your shell expects:

```sh
okto completions bash    # also: zsh, fish, elvish, powershell
```

Default locations the CLI writes to:

| Shell | Path |
|-------|------|
| bash  | `~/.okto/oktorc` (sourced from `~/.bashrc`) |
| zsh   | `~/.zfunc/_okto` |
| fish  | `~/.config/fish/completions/okto.fish` |

## Updating

```sh
okto update
```

This fetches the latest CLI release for your platform, replaces the running
binary in place, and refreshes completions.

!!! tip
    `okto update` upgrades the **CLI** only. To upgrade the **lair runtime
    image**, use [`okto lair update`](managing-lair.md#updating-the-lair-image).

## Uninstalling

Remove the binary and completion files:

```sh
okto uninstall          # prompts for confirmation
okto uninstall -y       # skip the prompt
```

To also tear down the lair container and all agent data, run
[`okto destroy`](getting-started.md#tearing-down) first.
