# Validation

Validated on 8 November 2026 on macOS 15.7.7, ARM64 (M4), with Rust 1.99.1.

| Check | Result |
| --- | --- |
| Library/CLI release build | Passed locally; ad-hoc signed Mac CLI bundle |
| Library tests | 23 passed |
| CLI integration tests | 7 passed |
| WebSocket integration tests | 5 passed |
| Library with Bluetooth disabled | 22 tests passed |
| Clippy, warnings as errors; formatting | Passed |
| Rust simulator → production C# client | Passed speed, stale, stop, resume, disconnect and credential cleanup |
| Optional control policy | Passed upper/lower bounds, exact ±0.1 steps, duplicate/cooldown checks, canceled requests, stale/stopped/replay rejection, and timeout lockout |
| Optional control endpoint | Passed authentication/opt-in checks and simulated 3.0 → 3.1 → 3.0 confirmations |
| Unity road pickup → C# → Rust → telemetry → movement | Both simulated pickup directions confirmed in a rendered game run |
| X21 speed commands | First +0.1 pickup confirmed by user (3.2 → 3.3); later 3.4 request unconfirmed. Write-spacing fix awaits physical retest |
| X21 GATT inspection | KS-NACH-X21C, 00011234 / 0001fed7 / 0001fed8; no FTMS |
| X21 text encoding | v5, selected from a captured successful handshake |
| Stopped telemetry capture | 20 seconds; 28 notifications including speed 0, stopped state, zero steps/time |
| Running live bridge | 90 seconds; 3.2 km/h and increasing steps/time; speed confirmed by user |
| X21 → bridge → production C# gameplay core | Passed a subsequent 35-second session; movement and shutdown pause verified |
| Final live WebSocket capture | 177 snapshots; no stale data after first valid sample while connected; final invalidation and credential cleanup passed |
| Windows/Linux | CI configured; execution and hardware validation pending |
| Quest | Not implemented |

The original two-second expiry was too close to the device’s roughly two-second report spacing: one premature stale snapshot appeared in the first live bridge recording. CLI and C# freshness now use three seconds. Acks and bridge heartbeats do not refresh measurements. Actual sample timestamps, not heartbeat counts, determine freshness.

During the original telemetry validation, the user operated the physical belt controls and confirmed 3.2 km/h on the display. The SDK sent only its allowlisted handshake/read requests in those sessions. An initial property-query-only session received acknowledgements without metrics; read-only initialization plus the telemetry property list subsequently produced reports. The exact minimal initialization sequence is not yet isolated.

The user subsequently requested ±0.1 km/h road bonuses and selected limits of 1.0–5.0 km/h. The opt-in speed-control implementation uses the documented KingSmith `CurrentSpeed` setter and later telemetry confirmation. Simulation and policy checks pass. After waking the X21, the user confirmed one physical +0.1 pickup; a subsequent command did not change the reported 3.3 km/h and correctly locked further control. Bluetooth fragment/line pacing and explicit lock-state feedback were added; a repeat physical check is pending. The game defaults bonuses to off. No start/stop, control-mode, user-ID, or clock command has been added.

Observed metrics are speed, run state, steps, and elapsed time. Distance was not present in these replies, so it stays null; energy/cadence semantics remain unvalidated. Do not infer measured belt velocity or independently measured human steps from these values. Sleeping stopped the device’s BLE advertisements; waking it restored discovery.

Sanitized fixtures preserve real notification bytes and timings. `x21-gatt-profile.json` replaces the OS identifier and removes RSSI. `x21-handshake-only.jsonl` verifies that acknowledgements never become invented zero speed. `x21-stopped-telemetry.jsonl` retains only handshake and two stopped metric reports; identity/configuration responses were excluded. The FTMS fixture remains synthetic. Replay always labels these files as historical input.

Long sessions, physical speed-command verification and stop/acceleration latency, unexpected radio loss, reconnect robustness, firmware variants, and platform parity remain to be validated. The sibling Unity project is now activated; six Play Mode tests, rendered pickup confirmation, and a native ARM64 build pass. The Mac CLI is signed only for local development; distribution signing/notarization and public publication are future work.
