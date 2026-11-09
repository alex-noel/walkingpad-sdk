# Fixtures

- `ftms-synthetic.jsonl`: generated FTMS payloads, not a hardware capture; exercises missing speed, expiry, stop, and disconnect.
- `x21-gatt-profile.json`: actual KS-NACH-X21C GATT layout from 8 November 2026. OS identifier replaced and RSSI removed. Candidate annotations reflect the inspector version used for that capture.
- `x21-handshake-only.jsonl`: actual handshake and property acknowledgements without metrics. It must never yield usable zero speed.
- `x21-stopped-telemetry.jsonl`: actual successful handshake and two stopped reports. Bytes and elapsed times are unchanged; other notifications, including identity/configuration responses, are omitted. The OS identifier is replaced and RSSI removed.

All recordings replay as `input_source: replay`, including those whose quality is `device_observation`. Raw sessions are kept in ignored local directories.
