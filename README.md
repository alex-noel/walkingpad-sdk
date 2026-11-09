# WalkingPad SDK

An independent Rust library and command-line tool for walking-pad discovery, telemetry, and optional bounded speed adjustments. The first hardware target is a KingSmith X21 on macOS with Apple Silicon. Device compatibility is established from GATT services and captured packets, not the model name alone.

This version provides BLE discovery, GATT inspection, notification capture, a standard FTMS decoder, field freshness, offline replay/simulation, and an authenticated local WebSocket bridge for live BLE or simulated data. A KingSmith text driver recognizes the X21’s 32-bit UUID alias and selects the v1/v5 encoding from its handshake. The bridge is read-only by default; an explicit flag enables relative speed adjustments on an already-running belt.

## Build and verify

Install Rust using [rustup](https://rust-lang.org/tools/install/). The repository pins the tested toolchain in `rust-toolchain.toml` and dependencies in `Cargo.lock`. If Rust was just installed, load its shell path with `. "$HOME/.cargo/env"`.

```sh
cargo build --locked --bin walkingpad
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
```

On Linux, install `libdbus-1-dev` and `pkg-config` (or distribution equivalents). Runtime Bluetooth access requires BlueZ. Windows requires a BLE-capable adapter. macOS requires Bluetooth permission for the launching application or packaged SDK tool. The CLI embeds its Bluetooth usage description; a signed local bundle can also be built with `sh scripts/package-macos.sh`.

## Start without hardware

```sh
cargo run --locked --bin walkingpad -- simulate --speed-kmh 3
cargo run --locked --bin walkingpad -- decode 04042c01b00400a005
cargo run --locked --bin walkingpad -- replay fixtures/ftms-synthetic.jsonl
```

Simulation emits a fresh speed, a stale interval, a reported stop, and a disconnect. Replay runs immediately against recorded elapsed times rather than wall time. The supplied fixture is synthetic and is not evidence of X21 compatibility.

## Stream input to a game

Run this in one terminal:

```sh
mkdir -p captures
cargo run --locked --bin walkingpad -- bridge --simulate --demo-faults --seconds 60 --connection-file captures/simulator.json
```

Connect from a second terminal:

```sh
cargo run --locked --bin walkingpad -- subscribe --connection-file captures/simulator.json
```

The bridge sends a heartbeat snapshot every 250 ms; live notifications can also publish immediately. With `--demo-faults`, device reports pause at 8–12 seconds, the belt reports stopped at 12–15 seconds, then walking resumes. The game should use `usable_speed_mps`: null means input is unavailable, zero means a valid stop, and a positive value is forward speed in metres per second.

The connection file contains a randomly selected localhost WebSocket URL and a per-session token. On Unix it is owner-readable only and is removed on normal shutdown. Consumers connect to the URL using an `Authorization: Bearer TOKEN` header. The token is never printed. The server binds only to `127.0.0.1`, keeps only the latest snapshot for slow clients, and sends a disconnected snapshot before closing. The sample subscriber also invalidates its last speed if the server disappears or delivers no telemetry for three seconds.

Select a live device explicitly:

```sh
cargo run --locked --bin walkingpad -- bridge --device KS-NACH-X21C --seconds 300 --connection-file captures/live.json
cargo run --locked --bin walkingpad -- subscribe --connection-file captures/live.json
```

Run the subscriber in another terminal. `--simulate` and `--device` are mutually exclusive; speed and fault-demo options apply only to simulation. A failed live session never falls back to generated data. Live snapshots and credentials use `input_source: live_device`; optional simulation uses `simulation`. No cloud account or game engine is required. The bridge exits after a device disconnect; restart it to establish a new connection epoch.

## Optional speed adjustments

```sh
cargo run --locked --bin walkingpad -- bridge --device KS-NACH-X21C --allow-speed-control --min-speed-kmh 1 --max-speed-kmh 5 --seconds 300 --connection-file captures/live.json
```

The same option works with `--simulate` for development. It adds `speed_control` limits to the connection document and enables an authenticated `/control` WebSocket. Each request changes the current reported speed by exactly **+0.1 or −0.1 km/h**, within the configured range (default **1.0–5.0 km/h**). Start, stop, mode changes, and speed restoration on reconnect are not implemented. FTMS motor control is not implemented.

The SDK requires fresh running-state and speed reports, a matching connection epoch/current speed, a unique request ID, and ten seconds between accepted requests. It serializes writes with telemetry polling. A later fresh device report must match the target within eight seconds; an acknowledgment alone is insufficient. An uncertain write or missing confirmation locks controls for that bridge session and is never retried automatically. See the [request/response contract](docs/protocol-notes.md#optional-speed-control-contract).

For a live diagnostic session, add `--record captures/control-session.jsonl` to the bridge command. This records notifications without opening a second BLE connection. Control requests and results appear on stderr without the authentication token. Raw captures may contain device identifiers; sanitize them before sharing.

## Discover the X21

Power the pad on, leave the belt stopped, and disconnect WalkingPad+/KS Fit before connecting this tool.

```sh
cargo run --locked --bin walkingpad -- scan --seconds 10
mkdir -p captures
cargo run --locked --bin walkingpad -- inspect --device X21 --output captures/x21-profile.json
```

`scan` only observes advertisements. It lists recognizable walking-pad candidates by default; use `--all` for an unknown advertisement name. `--device` accepts an exact OS identifier or a unique name substring. Ambiguous selection is rejected.

`inspect` connects, enumerates all services and characteristics, reports candidate protocols, and disconnects. A candidate is not a verified driver. Unknown profiles are preserved as diagnostics.

For recognized FTMS or KingSmith telemetry:

```sh
cargo run --locked --bin walkingpad -- watch --device X21 --seconds 30
cargo run --locked --bin walkingpad -- capture --device X21 --telemetry --seconds 30 --output captures/session.jsonl
```

For a proprietary profile, choose the exact service and notifying characteristic from the inspection result:

```text
walkingpad capture --device DEVICE_ID --service SERVICE_UUID --characteristic NOTIFY_UUID --output captures/vendor-session.jsonl
```

Plain `capture` is passive: it subscribes to notifications and may write the standard notification descriptor, but sends no application-level commands. `capture --telemetry`, `watch`, and a bridge without `--allow-speed-control` send only the selected driver’s handshake and read requests. The KingSmith driver reads network/device/product/version information and requests properties. Only an explicitly enabled control bridge can set speed; it never writes run state, control mode, user ID, or clock. Unknown alphabets fail before property queries. Acknowledgements alone do not count as telemetry. Ctrl-C during capture unsubscribes and disconnects. Neither stopping this tool nor losing Bluetooth stops the physical belt.

Files are created exclusively: existing captures are never overwritten. Captures can contain device identifiers; `captures/` is gitignored. Sanitize identifiers before sharing a recording.

## Library

```toml
[dependencies]
walkingpad-sdk = { path = "../walkingpad-sdk/crates/walkingpad", default-features = false }
```

The Rust library name is `walkingpad`. Enable its default `ble` feature for device access. Pure parsing, session aggregation, and replay work without any Bluetooth dependency. This repository has no dependency on the Walkpadead game or Unity.

| Module | Purpose |
| --- | --- |
| `ble` | Device selection, bounded GATT operations, capture lifecycle, disconnects |
| `device` | Portable profiles and conservative protocol candidates |
| `control` | Relative speed policy, bounds, duplicate/cooldown checks, and telemetry confirmation |
| `kingsmith` | Bounded text reassembly, handshake-based v1/v5 selection, read queries, speed/counter parsing, speed-set encoding |
| `protocol` | Standard FTMS speed, distance, time, incline, energy, and heart-rate decoding |
| `telemetry` | Partial reports, per-field age, stale speed, connection epochs, reset detection |
| `recording` | Versioned JSONL capture and bounded, validated replay input |

Use `usable_speed_mps` for movement. It is null when the speed is stale or the session has disconnected. `speed_mps` remains the last observation for diagnostics. Other notifications do not refresh speed's age. Unsupported fields remain null, and device values are not assumed to prove that someone is walking on the belt.

## Compatibility and next work

The initial local build target is macOS ARM64. CI is configured for macOS, Windows, and Linux; those CI jobs and real-device combinations must be validated before advertising support. Android packaging is future work.

The standard FTMS decoder rejects unverified vendor flags and malformed frames. KingSmith service aliases `00001234`, `00011234`, and `00021234` are recognized; only the observed X21 profile is hardware-inspected. Other alphabets and WiLink telemetry remain unsupported. Automatic BLE reconnect, companion GUI, and Quest transport are subsequent milestones. See the validation record for the precise extent of hardware verification.

See [protocol and contract notes](docs/protocol-notes.md) and [initial validation results](docs/validation.md). This project is independent of KingSmith and WalkingPad. MIT license; dependencies retain their own licenses.

