# Noise & SSH keypairs

Every lair container holds **two** independent keypairs serving different
purposes. They are generated on first boot, persisted to the bind-mounted
`~/.okto/` directory so they survive container restarts, and never leave
the host except as **pubkeys** (one-way) when bootstrapping a remote
agent. This document explains what each key is for, where it lives, and
how cross-container trust is established.

---

## At a glance

| Keypair | Algorithm | Used for | On-disk path | Code |
|---|---|---|---|---|
| **Noise** | X25519 (Curve25519) | Encrypting all mobile↔lair and lair↔remote-agent traffic via the [Noise Protocol Framework](https://noiseprotocol.org/) XX handshake | `$OKTO_DATA_DIR/noise_key.bin` (= `~/.okto/lair/noise_key.bin`) | [`core/src/noise.rs`](../core/src/noise.rs) |
| **SSH** | Ed25519 | Operator backchannel — SSHing into remote VMs to bootstrap them, plus agent-driven `gh`/git/GPU-pod SSH | `$HOME/.ssh/id_ed25519{,.pub}` (= `~/.okto/.ssh/`) | [`lair/src/ssh.rs`](../lair/src/ssh.rs) |

Neither key uses a passphrase — both are protected by Unix file
permissions (0600, owned by lair's uid inside the container).

---

## Noise keypair — transport encryption

The Noise key is the **transport identity** lair advertises to mobile
clients and remote agents. The pattern is `Noise_XX_25519_ChaChaPoly_SHA256`:
a three-message handshake that does mutual authentication and derives a
fresh symmetric session key. Both sides prove possession of the static
private key by signing the handshake transcript with it — there is no
out-of-band CA or TLS PKI involved.

### Where it shows up

- **`okto init`** prints a QR code containing `2:<host>:<port>:<base32-pubkey>`. The pubkey is lair's Noise static; mobile pins it as the *expected* responder static and refuses any handshake that doesn't match. This is how DNS / TLS PKI is avoided entirely.
- **Remote agents** (those started with `AGENT_NOISE_PORT` set in cloud-init userdata) generate their *own* Noise keypair on first boot and publish the pubkey in `/var/lib/okto/lair/agent-info.json`. Lair SSHes in once during `register_remote_agent`, reads that file, and stores the remote's pubkey in its agent registry. From then on, lair opens an outbound Noise tunnel to the remote VM on demand and verifies the remote's pubkey against the registry on every handshake ([`lair/src/lair.rs::child_http_base`](../lair/src/lair.rs)).
- **Lair → remote-agent userdata** embeds `LAIR_PUBKEY=<base32>` so the remote agent's Noise responder will *only* complete handshakes initiated by this specific lair — symmetric pinning, fail-closed ([`lair/src/agent.rs:1117-1142`](../lair/src/agent.rs)).

### Encoding

Base32 (RFC 4648, no padding) for any human-visible context — QR codes,
logs, `agent-info.json`, env vars. The on-disk format is plain 64 bytes:
the first 32 are the private key, the next 32 are the public key.
`to_base32` / `from_base32` helpers live in [`core/src/noise.rs`](../core/src/noise.rs).

---

## SSH keypair — operator backchannel

The SSH key is the **container's external identity** for anything that
speaks SSH: bootstrapping a remote VM, `git clone git@github.com:…`,
`ssh root@<gpu-pod>`. It is *not* used to authenticate between okto
processes — that's the Noise key's job.

### Scope: per container, shared across local agents

Lair generates the keypair on startup at `$HOME/.ssh/id_ed25519` (=
`/data/.ssh/` inside the container, bind-mounted from `~/.okto/.ssh/`
on the host) and then **seeds every child agent's `~/.ssh/` from the
same keypair, chowned to the agent's uid**. So all local agents in one
container share one SSH identity.

This is deliberate: the operator only has to register one pubkey per
container on external services (GitHub, GPU providers, etc.) and every
agent inside that container inherits the trust. Print yours with:

```sh
okto ssh pubkey
```

### Bootstrap flow for remote agents

When the operator (via the mobile chat or CLI) asks lair to create a
remote agent on a cloud VM, lair:

1. Calls `mint_bootstrap_userdata` ([`lair/src/lair.rs:1820-1949`](../lair/src/lair.rs)) which emits a cloud-init script that appends lair's SSH pubkey to the VM's `/root/.ssh/authorized_keys` and embeds the Noise pubkey as `LAIR_PUBKEY`.
2. The operator passes that userdata to whatever provisioning MCP they're using (AWS, Hetzner, etc.).
3. Once the VM is up, lair SSHes in (using its own private key against the trust it just installed), waits for `agent-info.json` to appear, drops API-key env vars, optionally clones a repo, restarts the agent systemd unit, and records the remote's Noise pubkey in its registry.

After that, **SSH is bootstrap-only** — runtime chat traffic goes over
Noise, not SSH.

---

## Per-container scope (no cross-container key sharing)

Each lair container has its **own** Noise key and its **own** SSH key.
When you spin up a remote agent on EC2, that remote machine runs its
own lair container, which generates its own pair of keys at startup.
The original lair never copies its private keys anywhere; only pubkeys
flow outward (one-way, embedded in cloud-init).

Consequences:

- Pubkeys you've registered on GitHub / GPU providers for your *local* lair won't be honored by a *remote* lair. The remote container has a different identity and needs its own pubkey registered on those services if its agents need to e.g. `git push` from there.
- If you destroy and re-init a lair container, both keypairs regenerate. External services trusting the old pubkey need to be updated.

---

## Rotation & regeneration

To rotate either key: delete the file inside `~/.okto/` (the lair
container has it bind-mounted as `/data/.ssh/` or `/data/lair/noise_key.bin`),
then `okto reload`. Lair will notice the missing file at startup and
generate a fresh keypair ([`load_or_generate_keypair`](../core/src/noise.rs),
[`ensure_container_ssh_keypair`](../cli/src/init.rs)).

After rotating:

- **Noise**: every mobile client and remote agent that pinned the old pubkey will refuse to handshake. Re-pair mobile by re-scanning the QR (`okto qr`); re-issue userdata for any remote agents you want to keep.
- **SSH**: anywhere the old pubkey was registered (GitHub, GPU providers, your own servers) needs the new one — print it with `okto ssh pubkey`.
