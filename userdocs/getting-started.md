# Getting started

This walks you from a fresh host to chatting with an agent on your phone.

## 1. Bootstrap lair

```sh
okto init
```

`okto init` is the one-time setup command. It refuses to run if
`~/.okto/config.json` already exists. On first run it **prompts interactively**
for:

- **Anthropic API key** — press <kbd>Enter</kbd> to skip.
- **OpenAI API key** — press <kbd>Enter</kbd> to skip. *(At least one of the two keys is required.)*
- **API URL** — <kbd>Enter</kbd> for the Anthropic default, or the full
  chat-completions URL of an OpenAI-compatible endpoint
  (e.g. `https://api.deepinfra.com/v1/openai/chat/completions`).
- **Model** — <kbd>Enter</kbd> for the default (`claude-sonnet-4-6`).

It then:

1. Persists credentials to `~/.okto/config.json`.
2. Installs Docker if it isn't already present.
3. Generates a **Noise keypair** (transport identity) and an **Ed25519 SSH
   keypair** (operator backchannel for remote agents).
4. Writes the env file `~/.okto/lair-env` (consumed by `docker --env-file`).
5. `docker pull`s the lair image and `docker run`s the container, bind-mounting
   `~/.okto` to `/data` and publishing the Noise port.
6. Waits for the management API to report healthy, then **prints a QR code**.

### Useful `okto init` flags

| Flag | Purpose |
|------|---------|
| `-e, --env KEY=VALUE` | Extra env var for the lair container (repeatable). Inherited by every child agent. e.g. `-e GH_TOKEN=…` |
| `--noise-port <PORT>` | Host-side Noise port the QR advertises. Default **8443**. |
| `--http-port <PORT>` | Loopback management-API port. Default **8000**. |
| `--image <REF>` | Lair image reference. Defaults to `$OKTO_LAIR_IMAGE` or `ghcr.io/georgebradford0/lair:latest`. |
| `--mcp-config <PATH>` | Seed lair's [MCP servers](mcp.md) from an `mcp.json` file. |
| `--system-prompt-append <TEXT or @PATH>` | Append site-specific guidance to lair's system prompt. `@path` reads a file. See [Customization](customization.md#system-prompt). |
| `--disable-push` | Turn [push notifications](notifications.md) off end-to-end. |
| `--ready-timeout <SECS>` | How long to wait for health after `docker run`. Default **180**. Bump it if your [`bootstrap.sh`](customization.md#bootstrapsh) does heavy work. |

!!! example "Init with a GitHub token and house-style prompt"
    ```sh
    okto init \
      -e GH_TOKEN=ghp_xxx \
      --system-prompt-append @./lair-prompt.md
    ```

## 2. Pair your phone

When `init` finishes it prints a QR code. If you need it again later:

```sh
okto qr
```

The QR encodes `2:<host>:<port>:<noise-pubkey>`. Open the mobile app, tap the
icon, and scan it. The host is auto-detected from your public IP; override it
with `--host`, or set `PUBLIC_HOST` via [`okto env`](managing-lair.md#environment-variables-okto-env).

```sh
okto qr --host my.box.example.com
```

On iOS the app asks for push-notification permission — see
[Push notifications](notifications.md).

## 3. Chat

Once paired, you're talking to lair (the parent agent). From the chat you can
ask it to write code, run commands, and **create child agents** (local or
remote) that then appear in the sidebar. See [Agents](agents.md).

## Tearing down

Stop lair, remove every managed agent, and wipe lair's host data
(`~/.okto/lair`, `~/.okto/agents`, `~/.okto/lair-env`, the launch record) —
leaving your `config.json` in place:

```sh
okto destroy        # prompts; type "yes"
okto destroy -y     # skip the prompt
```
