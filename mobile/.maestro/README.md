# Maestro e2e tests (iOS Simulator + Android emulator)

On-device end-to-end tests that drive the **real app** through its core flow —
launch → connect → send a message → see the agent reply — on a simulator or
emulator. They complement the Jest/RTL suite (`mobile/__tests__/`), which runs
the component tree in Node with every native module mocked; these run the actual
native binary on a real virtual device, exercising the Noise tunnel, the bundled
JS, and the rendered UI.

## How it hangs together

The app needs a **lair** to talk to. Instead of a real one (API spend, network,
Docker), we boot a tiny **mock lair**: the real `lair` binary, started under
`OKTO_DEV=1` so it serves the **fixed dev keypair**, backed by the same
in-process mock LLM the Rust e2e suite uses. Fully offline, deterministic, free.

Because the keypair is fixed, the connection string is a constant — the same one
baked into `App.tsx` as `DEV_CONN` and into `core` as `DEV_PUBKEY_BASE32`:

```
iOS Simulator    →  2:127.0.0.1:9000:34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ
Android emulator →  2:10.0.2.2:9000:34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ
```

(iOS Simulators share the Mac's network stack so `127.0.0.1` reaches the host;
the Android emulator reaches the host through its `10.0.2.2` alias. lair binds
its Noise proxy on `0.0.0.0:9000`, so both work.)

The flows connect via the **manual connect-string field** (`manual-connect-input`
+ `manual-connect-button`) rather than the camera, since Simulators have no
camera. This is also exactly the onboarding an App Store reviewer would do.

## Files

| Path | What |
|------|------|
| `tests/tests/maestro_serve.rs` | The mock lair — an `#[ignore]`d serve "test" that parks on `:9000`. |
| `mobile/.maestro/connect.yaml` | Reusable subflow: launch fresh → manual connect → assert chat screen. |
| `mobile/.maestro/smoke.yaml` | Full happy path: connect → send message → assert the scripted reply. |
| `mobile/scripts/maestro-e2e.sh` | Boots the mock lair, fans the flows out across all booted devices in parallel, tears down. |

testIDs the flows rely on (in `App.tsx`): `manual-connect-input`,
`manual-connect-button`, `chat-input`, `composer-send`, `composer-stop`.

## Prerequisites

1. **Maestro** — `curl -fsSL https://get.maestro.mobile.dev | bash` (see
   <https://maestro.mobile.dev>).
2. **Rust toolchain** — to build/run the mock lair (`cargo`). First run builds
   the `lair` binary; afterwards it's cached.
3. **The app installed on the target device(s):**
   - iOS Simulator: `npm run -w mobile ios` (or build onto a booted sim in Xcode).
   - Android emulator: `npm run -w mobile android` (onto a running emulator).
   - A **release** build is preferred so the app shows the manual connect screen.
     A debug (`__DEV__`) build on the iOS Simulator auto-connects to
     `127.0.0.1:9000` and skips manual entry — the flows tolerate that (they
     only assert the chat screen appears), but on an Android emulator the debug
     auto-connect targets the wrong host, so use a release build there.

## Run

The all-in-one runner (boots the mock lair, runs every booted device in parallel):

```sh
mobile/scripts/maestro-e2e.sh            # all booted iOS + Android devices
mobile/scripts/maestro-e2e.sh --ios      # iOS Simulators only
mobile/scripts/maestro-e2e.sh --android  # Android emulators only
```

Or drive it manually — start the mock lair in one terminal:

```sh
cargo test -p okto-tests --test maestro_serve serve -- --ignored --nocapture
```

…then run a flow in another (pick the HOST for your device type):

```sh
maestro test mobile/.maestro/smoke.yaml --env HOST=127.0.0.1   # iOS Simulator
maestro test mobile/.maestro/smoke.yaml --env HOST=10.0.2.2    # Android emulator
maestro studio                                                  # interactive authoring
```

## CI

The iOS release workflow (`.github/workflows/ios.yml`) runs `smoke.yaml` as a
gate across a `strategy.matrix` of iPhone simulators: its `build` job builds the
app once per device, boots that sim, installs the app, boots the mock lair, and
drives the flow with `HOST=127.0.0.1`. `distribute` (TestFlight) `needs: build`,
so a failing flow on any device blocks the upload. The Android emulator runs are
local-only.

## Adding flows

Give any element you need to select a stable `testID` in `App.tsx`, then select
it in the flow with `id: "<testID>"`. Prefer testIDs over visible-text matching —
the UI copy changes more often than the testIDs. Keep new flows parameterised by
`${HOST}` so they run unchanged on both platforms.
