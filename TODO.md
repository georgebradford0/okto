# TODO

- [ ] Change to leave text input but disable send button in chat and replace with stop button surrounded by activity indictaor
- [ ] check if api keys used for starting up lair are visible in deployment data, move to secrets
- [ ] Setup background tasks
- [ ] More robust pod readiness waiting — child pods have no readiness probe so `wait_for_deployment_ready` returns as soon as the process starts, not when the server is actually listening. Add a `/health` readiness probe to child deployments (same as lair) so reload and create reliably wait until the pod is ready to accept connections.
- [ ] Client-key allowlist + first-connection ack UI — `noise_handshake` already captures the client static key from snow's `get_remote_static()` and logs it, but it's never checked. Persist a `known_clients.json` on lair, gate new client pubkeys behind a first-connection ack flow in the mobile UI (approve / reject), and reject handshakes from unknown keys after the first run. Replaces today's QR-only TOFU model where anyone with the QR can connect indefinitely.
- [ ] Children generate their own Noise keypair on first boot — today `lair/create_pod` injects the parent's hex-encoded keypair into each child via the `NOISE_PRIVATE_KEY` env var, so leaking one child's pod env compromises lair and every sibling. Have child servers run `load_or_generate_keypair` against their own `/data/noise_key.bin`, register their pubkey back to lair via a small HTTP endpoint, and let lair store per-child pubkeys in `pubkey_registry.json` instead of broadcasting its own.

# POSSIBLY TODO
- [ ] Setup push notifications on mobile to let user know when something is finished.
