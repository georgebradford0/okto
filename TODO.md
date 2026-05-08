# TODO

- [ ] Check endpoints used for both lair and agents such as /stream, /branches, etc and see why they are necessary
- [ ] Check if api keys used for starting up lair are visible in deployment data and if so address this
- [ ] Client-key allowlist + first-connection ack UI — `noise_handshake` already captures the client static key from snow's `get_remote_static()` and logs it, but it's never checked. Persist a `known_clients.json` on lair, gate new client pubkeys behind a first-connection ack flow in the mobile UI (approve / reject), and reject handshakes from unknown keys after the first run. Replaces today's QR-only TOFU model where anyone with the QR can connect indefinitely.
- [ ] Children generate their own Noise keypair on first boot — today `lair/create_pod` injects the parent's hex-encoded keypair into each child via the `NOISE_PRIVATE_KEY` env var, so leaking one child's pod env compromises lair and every sibling. Have child servers run `load_or_generate_keypair` against their own `/data/noise_key.bin`, register their pubkey back to lair via a small HTTP endpoint, and let lair store per-child pubkeys in `pubkey_registry.json` instead of broadcasting its own.
