# SideShift Latency Notes (MVP)

## Targets

- Wired same-LAN goal: end-to-end input latency in the 1–3ms range (capture → network → inject)
- Hard max: 5ms on a quiet gigabit LAN for the hot path

## Why the design uses this data path

- Input events use encrypted UDP datagrams for low-latency forwarding
- Control state uses a separate encrypted TCP channel
- Session handshake runs once; events do not require extra round-trips
- Event frames stay tiny (relative deltas + button/key codes)
- Clipboard is out of scope and intentionally excluded from the input path

## Expected Wi-Fi behavior

Wi-Fi can work well for casual use, but jitter spikes are common under contention or interference. Treat Wi-Fi as best-effort: it may miss the 5ms hard max even when average latency feels acceptable.

## Fast validation checklist

1. Baseline ping stability between both machines
2. Repeated edge-cross flick tests while typing immediately after handoff
3. Compare wired vs Wi-Fi feel and logged estimates (`--log-latency`) against the 1–3ms goal / 5ms hard max
