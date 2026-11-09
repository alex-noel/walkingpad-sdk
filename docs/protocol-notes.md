# Protocol and contract notes

## Evidence and driver selection

This implementation selects candidate families using the discovered GATT layout. X21 model-name claims conflict across existing projects. The inspected `KS-NACH-X21C` exposes service `00011234-0000-1000-8000-00805f9b34fb`, write-without-response `0001fed7`, and notifying `0001fed8`; it does not expose FTMS.

References used for interoperability research:

- [Bluetooth SIG Fitness Machine Service tests](https://files.bluetooth.com/wp-content/uploads/dlm_uploads/2024/10/FTMS.TS_.p6.pdf), including the inverted More Data flag for Treadmill Data.
- [Huawei Treadmill Data field layout](https://developer.huawei.com/consumer/en/doc/hmscore-guides-v5/td-0000001050147077-V5/), used for field order and units, not its platform-specific mandatory-field restrictions.
- [btleplug](https://github.com/deviceplug/btleplug), the BLE backend, pinned to release 0.13.5.
- [BeltControl protocol research](https://github.com/morsela/beltcontrol/blob/main/docs/protocols.md) and [Trot compatibility notes](https://github.com/marcuspuchalla/trot#supported-treadmills), used to identify candidate families. Protocol constants are implemented independently in Rust; no upstream driver implementation or application binary is included.

The FTMS decoder is an original implementation of the documented field layout. It handles optional fields in order, including packets with no speed when More Data is set. Reserved/vendor flags and trailing data are rejected. Unsupported optional metrics are consumed to preserve offsets but are not exported. Explicit invalid energy and heart-rate values do not refresh a previous reading.

## Capture format version 1

Each JSONL file starts with a `profile` record containing the format version, source quality, and device profile. Subsequent `notification` records contain elapsed monotonic milliseconds, service and characteristic UUIDs, and the original bytes as hexadecimal. An `end` record describes normal capture termination; replay treats EOF without it as a disconnect.

The reader rejects misplaced headers, unknown format versions, invalid hex, backwards timestamps, records after the end marker, lines exceeding 64 KiB, and values larger than 512 bytes. Real captures include local identifiers and are not committed by default.

## Normalized telemetry version 1

Snapshots contain the connection epoch, report sequence, input quality, connection status, and nullable fields. Units are metres per second, metres, seconds, percent incline, kilocalories, and beats per minute. `field_age_ms` holds each observed field's own age. `sample_age_ms` is the age of the most recent recognized metric update.

The CLI uses a three-second stale-speed threshold, allowing one second of jitter around the observed roughly two-second X21 reports. Longer sessions and other firmware need separate validation. A tick or unrelated metric cannot refresh the speed timestamp. `usable_speed_mps` is null when stale or disconnected and zero for a fresh explicit stop. Counter decreases are exposed as resets/wraps; this version does not manufacture lifetime workout totals.

Replay preserves the original data quality and sets `input_source` to `replay`. A recorded device observation is still historical; it is never evidence of a live connection. Live input uses `live_device`; simulation uses `simulation`. Neither API is a physical emergency-stop mechanism.

## First hardware validation

The user lifted the simulation-only restriction and authorized direct inspection with the belt stopped and WalkingPad+/KS Fit disconnected. GATT discovery and passive capture succeeded. Passive capture produced no notifications. The harmless `shake` command returned `DAEEOAUghwiN\r`, which decodes to `shake 00\r` with the v5 alphabet. The driver selects v1/v5 only after a complete successful handshake. Subsequent read-only initialization produced stopped telemetry; later live sessions reported 3.2 km/h with increasing steps/time, confirmed by the user against the physical display. The device slept between sessions and had to be awakened for discovery.

Record the actual firmware when available, Bluetooth service layout, notification interval, and observed fields. Compare speed and counters with the pad display. Inspect acceleration and stopping separately to establish whether reported speed is measured or commanded. Confirm the physical controls remain usable while the SDK is connected.

No application-level write is needed for the initial service inspector. If the proprietary protocol requires a status request or handshake, implement only a documented, reviewed telemetry operation and test it separately. Do not run examples from other controllers that start a belt or restore a target speed automatically.

## Local bridge contract

The bridge emits live BLE or simulated snapshots over a localhost WebSocket at `/telemetry`. The connection file contains `schema_version`, `url`, `token`, and `input_source`. Clients send the token as a Bearer authorization header. Without `--allow-speed-control`, no command endpoint is available.

Snapshots are published on live notifications and every 250 ms through a channel that retains only the latest value. Field age and report sequence remain tied to telemetry updates. A deliberate telemetry gap therefore increases speed age even though the network connection continues sending snapshots. The SDK subscriber also enforces a three-second receive watchdog independently of the source's field freshness.

The optional fault demonstration pauses reports at 8–12 seconds, reports a stopped belt at 12–15 seconds, then resumes walking. All messages retain `input_source: simulation`. Live BLE uses the same contract with `input_source: live_device`. Local-network Quest transport remains future work.

## Optional speed-control contract

The user requested two-way road bonuses with a **1.0–5.0 km/h** range. `bridge --allow-speed-control` adds `"speed_control": {"min_tenths": 10, "max_tenths": 50}` to the connection document and enables `/control` on the same host/port with the same Bearer authentication. Omitted capability means read-only. Replay cannot control a device.

Each control connection accepts one JSON request and returns one reply before closing. A request carries `request_id` (new UUID), `connection_id` and `sequence` from fresh telemetry, `expected_speed_tenths` (30 means 3.0 km/h), and `delta_tenths` (exactly −1 or +1). Unknown fields are rejected. Requests expire after one second in the host queue and require current speed to match the expected base, fresh running state, and a current connection epoch. Duplicate IDs, out-of-range targets, concurrent pending requests, and requests within the ten-second cooldown are rejected.

Replies contain `request_id`, `status` (`confirmed`, `rejected`, or `unconfirmed`), `target_kmh` (nullable), `message`, and `control_locked` (boolean, absent means false for older bridges). A locked reply disables further game requests for that telemetry connection epoch while allowing telemetry to continue. Rejection means no speed write was issued. Confirmation requires a later fresh speed report matching the target, at least one second after the request and within eight seconds. It confirms the device's reported value, not independently measured belt velocity. Timeout, interrupted input, or uncertain writes lock controls until the bridge is restarted. Clients must not automatically retry an unconfirmed request.

The KingSmith setter is `props CurrentSpeed 3.1` (one decimal, km/h), encoded with the handshake-selected alphabet and CR framing. It is serialized with read queries and bounded to two seconds for the entire write. The implementation follows [BeltControl's documented property setter](https://github.com/morsela/beltcontrol/blob/6809303910f87a4f1fe68eb7c5caad00015edff9/docs/protocols.md), without sending `ControlMode`, run-state, user-ID, or clock changes. This path only operates on an already-running belt. Connection, reconnection, and game pause do not set or restore belt speed.

## KingSmith interoperability implementation

The wire format is substituted Base64 terminated by CR. Incoming lines may span notifications or share one notification; binary sidecars are excluded. Lines are bounded to 4096 bytes and malformed values never refresh field ages. Writes are split into 16-byte chunks, with 30 ms after each fragment and a further 60 ms after each complete line. These intervals follow [BeltControl's transport pacing](https://github.com/morsela/beltcontrol/blob/6809303910f87a4f1fe68eb7c5caad00015edff9/packages/belt-drivers/src/drivers.ts#L1274); the SDK retains the X21's previously used 16-byte chunk size. Queries and speed changes share this paced writer, so complete lines cannot overlap. Handshake retries stop after five unanswered attempts. After successful selection, fixed read-only initialization queries precede property polling at one-second intervals.

For diagnostics, `bridge --device … --record captures/session.jsonl` records the same notification format as `capture`. It never overwrites an existing file. Speed requests and results are logged with their request ID, expected speed, target, and status; credentials are excluded. Raw recordings may contain device identifiers and remain local by default.

The decoder exports `CurrentSpeed` (km/h converted to m/s), `RunningDistance` (metres), `RunningTotalTime` (seconds), `RunningSteps`, and `runState` (0 stopped, 1 running). These are device-reported values, not independently measured belt velocity or human steps. `BurnCalories` and `spm` remain unexported pending validation of units and semantics. Missing fields remain unknown. Firmware or app acknowledgements alone never create a zero-speed observation.

Protocol research and provenance:

- [BeltControl protocol notes at 6809303](https://github.com/morsela/beltcontrol/blob/6809303910f87a4f1fe68eb7c5caad00015edff9/docs/protocols.md): text framing, v1 alphabet, property names, configuration/telemetry queries. Its KS-C2 driver uses 16-bit UUID aliases and includes motor/control writes; it is not used wholesale here. Upstream is Apache-2.0.
- [walkingpad-ble-footpod at 27da894](https://github.com/LucasFrendorf/walkingpad-ble-footpod/blob/27da8944575dc41fb783158791ca6c1bd748277d/kingsmith_g1c.py): 32-bit UUID aliases, alternate substitution tables, and read-query vocabulary. Upstream is GPL-3.0.
- [QDomyos-Zwift KingSmith driver at 10eb005](https://github.com/cagnulein/qdomyos-zwift/blob/10eb005713a9c327f2f6629f225b05096e4c5f76/src/devices/kingsmithr2treadmill/kingsmithr2treadmill.cpp): corroborating X21 UUID variants and initialization sequence.

These were read as wire-protocol references. The Rust codec, buffering, query allowlist, BLE lifecycle, speed-set encoding, and tests were written independently. No upstream source code was copied or ported. Only the bounded current-speed setter is exposed through the optional control path; other upstream setters and clock updates remain excluded. The SDK remains MIT-licensed. Hardware profile identifiers are redacted in shared fixtures; raw diagnostic captures stay local.
