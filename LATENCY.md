# SideShift Latency Notes (MVP)

## Targets

- Wired same-LAN: aim for end-to-end p99 under ~10ms
- Stretch target: under ~5ms on very stable links

## Why the design uses this data path

- Input events use encrypted UDP datagrams for low-latency forwarding
- Control state uses a separate encrypted TCP channel
- Session handshake runs once; events do not require extra round-trips
- Clipboard is out of scope and intentionally excluded from the input path

## Expected Wi-Fi behavior

Wi-Fi can work well for casual use, but jitter spikes are common under contention or interference. In practice, p99 handoff and keypress latency on Wi-Fi should be expected to degrade versus wired Ethernet.

## Fast validation checklist

1. Baseline ping stability between both machines
2. Repeated edge-cross flick tests while typing immediately after handoff
3. Compare wired vs Wi-Fi p95/p99 feel and logged estimates (`--log-latency`)
