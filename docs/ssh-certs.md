# SSH certificate authority for outbound agent SSH

This doc explains how child agents get SSH access to remote infrastructure
(Prime Intellect GPU pods, operator-controlled VMs, etc.) without lair
ever sharing its own SSH key with them. As of `lair 0.13.0` / `cli 0.5.0`.

> **Companion docs:** [agent-isolation.md](agent-isolation.md) describes why
> children can't read lair's private key in the first place; this doc covers
> the mechanism that gives them outbound SSH anyway.

## Why this exists

Children need SSH for the workflow "clone a repo, run scripts in it that
shell out to `ssh` to drive training on a GPU pod." Three options were
considered:

1. **Share lair's SSH key with every child.** Simple. Breaks the isolation
   model — a misbehaving child now acts as lair against any host that
   trusts lair's pubkey, including the agent VMs lair itself provisioned.
2. **Proxy SSH through lair.** Cleanest from a trust standpoint, but
   scripts inside a cloned repo expect a working `ssh` binary, not a tool
   call. Awkward for `rsync`, port-forwards, anything interactive.
3. **SSH certificate authority — lair signs short-lived certs for each
   child.** What we do.

The CA approach gives the operator's mental model the right shape: **one**
pubkey (the CA) gets authorized on remote infrastructure, and that's
forever. Every child gets its own short-lived signed cert. The principal
stamped into the cert is the child's name, so remote sshd logs identify
which child made the connection.

## Files on disk

All paths inside the lair container; on the host, swap `/data/lair` for
`~/.octo/lair`.

| Path | Owner | Purpose |
|---|---|---|
| `/data/lair/ssh_id_ed25519{,.pub}` | lair | Operator-facing SSH key, used by `register_remote_agent` to SSH into agent VMs. Unchanged from before this feature. |
| `/data/lair/ssh_ca_ed25519` | lair | **CA private key.** Signs child certs. Never leaves lair. Generated once on first boot. |
| `/data/lair/ssh_ca_ed25519.pub` | lair | **CA public key.** What the operator authorizes on remote hosts. Print with `octo ssh ca-pubkey`. |
| `/data/lair/ssh_revoked.json` | lair | Revocation list (per-child names). Read by issuance + refresh paths. |
| `/data/agents/<name>/.ssh/id_ed25519{,.pub}` | per-agent uid | Child's own keypair. Generated on agent boot. Never seen by lair. |
| `/data/agents/<name>/.ssh/id_ed25519-cert.pub` | per-agent uid | Short-lived signed cert. OpenSSH client auto-discovers it. |

## Operator setup (one-time, per remote host)

After `octo init` has minted the CA keypair:

```bash
# On your workstation
octo ssh ca-pubkey                       # prints the one-line CA pubkey

# On the remote host (Prime Intellect pod, GPU VM, etc.)
echo "<paste output here>" | sudo tee /etc/ssh/lair_ca.pub > /dev/null
echo "TrustedUserCAKeys /etc/ssh/lair_ca.pub" | sudo tee -a /etc/ssh/sshd_config
sudo systemctl reload sshd
```

That's the entire operator-side burden. After this, every child agent's
short-lived cert is accepted automatically. You never have to touch the
remote host again for key churn — children come and go without the
remote `authorized_keys` ever changing.

If you also want per-principal authorization (only specific children get
in, identified by name), add `AuthorizedPrincipalsFile /etc/ssh/principals/%u`
to `sshd_config` and put one principal per line in the file. Default is
"any cert signed by the CA is accepted," which is what most workflows want.

## The issuance flow

When a child agent boots:

1. `bootstrap_ssh_identity` in [lair/src/agent.rs](../lair/src/agent.rs)
   calls `ensure_keypair_at(~/.ssh/id_ed25519, .pub)` — idempotent, so a
   restart reuses the existing keypair.
2. If the child has `OCTO_AGENT_TOKEN` set (i.e. it was spawned by another
   agent, not directly by the operator), it POSTs its pubkey to lair's
   `/ssh/cert` endpoint, gated by the `X-Octo-Agent-Token` middleware. The
   token identifies the calling child to lair — the principal stamped into
   the cert is the calling child's name, not anything in the request body.
3. Lair calls `octo_core::sign_user_cert`, which shells out to
   `ssh-keygen -s ssh_ca_ed25519 -I <name>-<unix-ts> -n <name> -V +1h
   <child.pub>`. The result is the OpenSSH user-cert text.
4. Child writes the cert to `~/.ssh/id_ed25519-cert.pub` (atomic rename).

After this, plain `ssh user@host` from inside the agent container "just
works" — OpenSSH auto-discovers the cert sitting next to the matching
private key. Scripts in a cloned repo that call `ssh` need no env vars,
flags, or wrappers.

For **operator-spawned** top-level agents (which don't get
`OCTO_AGENT_TOKEN`), the initial cert request is skipped. The refresher
catches them within `TTL/2` (default 30min) — they boot with a working
keypair but no cert until the next refresh tick.

## Refresh

Lair runs a background task ([`ssh_cert_refresher` in lair.rs](../lair/src/lair.rs))
on a fixed cadence. On each tick it iterates the agent registry and, for
every `Running` child with `id_ed25519.pub` on disk and not on the
revocation list, re-signs the cert and atomic-renames it into place.

**Conditions for refresh** (all must hold):
- Child is `Running` (not `Pending`, `Stopped`, or absent).
- `~/.ssh/id_ed25519.pub` exists in the child's home.
- Child's name is not in `ssh_revoked.json`.
- The CA private key exists.

**Interval:** `min(TTL/2, 15min)`, floored at 60s. With the default 1h
TTL that's 30min. The 15-minute cap means very long TTLs still refresh
often enough that lair restarts don't leave children mid-window without a
fresh cert.

**Out-of-band triggers** that don't wait for the tick:
- First-time pubkey publication (the child's boot-time POST mints
  immediately).
- A child re-POSTing to `/ssh/cert` for any reason (always honored unless
  revoked).

**The non-trigger:** the refresher does *not* inspect the existing cert's
expiry timestamp. Re-signing on a fixed cadence is simpler, an
`ssh-keygen` invocation is sub-50ms, and the math works out the same for
the metric that actually matters (worst-case revocation latency, see
below).

## Revocation

Revocation is per-agent-name, stored in `ssh_revoked.json`. Operator
commands:

```bash
octo ssh revoke <agent-name>      # lair refuses to mint or refresh certs for this name
octo ssh unrevoke <agent-name>    # take it off the list; next refresh tick mints again
octo ssh list-revoked             # print the current list
```

**What revocation actually does** (this is important):

| Surface | Effect |
|---|---|
| `/ssh/cert` endpoint | Returns 403 for any further cert request from this name. |
| Refresh task | Skips this child on every tick; the existing cert file is left in place but ages out by TTL. |
| Remote sshd hosts (Prime, etc.) | **Nothing.** OpenSSH has no online cert check. The remote keeps accepting the existing cert until its `valid_until` timestamp passes. |

So worst-case revocation latency against external infrastructure equals
**TTL**, not refresh interval. If a child was issued a fresh full-TTL cert
the instant before you revoked it, that cert remains valid until it
expires — there's no mechanism to invalidate it remotely. The lever to
pull for tighter guarantees is `OCTO_SSH_CERT_TTL_SECS` (default 3600),
not the refresh cadence.

**KRL push to lair-owned VMs:** for the agent VMs that lair itself
provisioned via `mint_bootstrap_userdata`, lair could in principle push
an OpenSSH KRL file (`/etc/ssh/lair_revoked.krl` + `RevokedKeys` line)
and do effective revocation in seconds. That plumbing is **not yet
implemented** — it's tracked as a follow-up. For now, both lair-owned
and external hosts rely on TTL-based expiration.

## Env knobs

| Env var | Default | Meaning |
|---|---|---|
| `OCTO_SSH_CERT_TTL_SECS` | `3600` (1h) | How long each minted cert is valid. Floor is 60s. This is the security parameter for external-host revocation. |

Tune TTL down for tighter blast radius (e.g. `300` = 5min for short-lived
GPU jobs) at the cost of more ssh-keygen invocations and more risk that a
lair outage longer than the TTL leaves children unable to renew.

## What children inherit (and what they don't)

By default a child agent ships with:
- Its own per-agent uid (10100–10199, see `agent_proc::uid_for_port`).
- Its own `OCTO_DATA_DIR` (`/data/agents/<name>/data/`).
- Its own `HOME` (`/data/agents/<name>/`).
- Its own SSH keypair at `~/.ssh/id_ed25519` (generated on first boot).
- A short-lived signed cert at `~/.ssh/id_ed25519-cert.pub` (refreshed
  every `TTL/2`).

A child does **not** see:
- Lair's `ssh_id_ed25519` (the operator's identity for SSHing into agent
  VMs). The child's uid can't read `/data/lair/` at all.
- Lair's `ssh_ca_ed25519` (the CA private key). Same.
- `LAIR_MGMT_TOKEN` (stripped from the child's spawn env).
- Other children's `OCTO_AGENT_TOKEN` (each child runs as its own uid, so
  `/proc/<sibling-pid>/environ` is unreadable).

The signing surface is the only path through which a child gains SSH
capability, and it's gated by the per-child token. A compromised child
can use its current cert for up to TTL, but it cannot mint certs for any
other principal, read the CA key, or issue itself longer-lived credentials.

## Code map

- `core/src/ssh.rs` — CA bootstrap (`ensure_ssh_ca_keypair`), cert signing
  (`sign_user_cert`), revocation store (`revoke`/`unrevoke`/`is_revoked`).
- `lair/src/lair.rs`:
  - `ssh_mint_cert` — `POST /ssh/cert` handler.
  - `ssh_ca_pubkey_handler` — `GET /ssh/ca-pubkey`.
  - `ssh_revoke_handler` / `ssh_unrevoke_handler` / `ssh_list_revoked_handler`.
  - `ssh_cert_refresher` — background tick.
  - CA bootstrap call (alongside the existing `ensure_ssh_keypair`) in `run()`.
- `lair/src/agent.rs::bootstrap_ssh_identity` — child-side keypair gen +
  initial cert POST.
- `cli/src/ssh.rs` — `octo ssh {ca-pubkey,revoke,unrevoke,list-revoked}`.
- `cli/src/init.rs` — calls `ensure_ssh_ca_keypair` during `octo init`.

## Open follow-ups

- **KRL push to lair-owned remote agent VMs.** Modify
  `mint_bootstrap_userdata` to drop `/etc/ssh/lair_revoked.krl` + a
  `RevokedKeys` line in sshd_config, and have lair push KRL updates over
  SSH on revocation events. Brings revocation latency for those hosts
  from "up to TTL" down to "seconds."
- **Operator-spawned children get certs immediately.** Currently they
  wait up to `TTL/2` for the refresher. Could be tightened by either
  minting tokens for all children (loosens the spawn-capability model)
  or by lair triggering a one-shot mint as part of `create_agent`
  finalization.
- **Per-principal authz examples.** Document `AuthorizedPrincipalsFile`
  patterns once we have a real workflow that needs it.
