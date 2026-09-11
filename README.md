# SideShift

SideShift is an open-source LAN input handoff daemon for **one mouse + one keyboard across two machines**.

Typical setup:
- Logitech mouse + keyboard are physically paired to your **primary** machine
- Mac and Windows sit side by side
- When the cursor reaches a configured edge (for example, the right edge on Mac), focus hands off to Windows
- Keyboard input follows that focus

> SideShift is a network handoff tool (Synergy/Deskflow style), **not** a Bluetooth re-pairing tool.
> Clipboard sync is intentionally out of scope for this MVP.

## MVP status

This repository contains the first MVP architecture and command-line daemon:
- Rust workspace with protocol + daemon core + CLI
- Authenticated + encrypted control and input channel primitives
- Spatial layout and edge-handoff state machine
- macOS / Windows platform adapters behind trait boundaries
- Linux mock adapters so CI runs on Linux now

Native event capture/injection paths are scaffolded for macOS/Windows and ready for deeper implementation on real hardware.

## Why SideShift exists

Logi Flow is convenient but tied to Logitech ecosystem behavior and device switching. SideShift aims for:
- open protocol and source code
- explicit machine layout control
- predictable local network behavior
- no vendor-specific Bluetooth protocol work

## Current architecture

Workspace layout:

```text
crates/
  sideshift-protocol/   # Auth handshake, encrypted frame/datagram codecs
  sideshift-core/       # Config, layout, handoff state machine, platform traits, daemon loops
  sideshift/            # CLI binary
```

### Protocol choice

This MVP uses:
- **TCP control channel** for reliable coordination messages (hello, handoff start/end, keepalive)
- **UDP datagrams** for low-latency input events
- **Pre-shared secret authentication + session key derivation (HKDF)**
- **ChaCha20-Poly1305 authenticated encryption** for both control frames and input datagrams
- **One handshake per session, then sealed datagrams** (no per-event crypto round-trip)

Rationale:
- control events need ordered/reliable delivery
- mouse/keyboard deltas must stay on a non-blocking datagram path, not a clipboard/file stream
- hot-path frames are intentionally tiny (relative pointer deltas + button/key codes)
- transport and platform layers stay separable for future QUIC migration

## Latency target and expectations

SideShift is designed around a latency-first goal:
- **Wired same-LAN target:** end-to-end input latency in the **1–3ms** range
- **Hard max target:** **5ms** on a quiet gigabit LAN

Reality check for Wi-Fi (best-effort only):
- modern 5GHz/6GHz Wi-Fi can feel good, but contention, power saving, and interference can add jitter spikes
- Wi-Fi can miss the 5ms hard max, especially in busy RF environments

### What dominates latency

Most delay comes from three buckets:
1. **Capture time** on the server OS (event tap/hook behavior and scheduling)
2. **Network transit** (LAN RTT + jitter, queueing, AP behavior on Wi-Fi)
3. **Injection time** on the client OS (native input API + scheduler timing)

Crypto overhead is usually small relative to those three once the session key is established.
The hot path is built to avoid additional round-trips and avoid unnecessary per-event logging.

### How to test quickly

1. **Loopback sanity:** run server+client on one machine to validate software path and logs
2. **Two-machine LAN check:** verify ping stability between machines (`ping` baseline)
3. **Subjective edge-flick test:** rapidly flick to the configured handoff edge and type immediately on the other machine

Optional runtime hook:
- run client with `--log-latency` to print a rolling one-way estimate window (clock-sync dependent)
- command `sideshift bench` is reserved as a future active benchmark hook against the 1–3ms goal / 5ms hard max

Compared with Logi Flow-style Bluetooth switching, SideShift avoids Bluetooth re-pair handoff delays by keeping ownership fixed on the server and forwarding events over LAN.

See [LATENCY.md](LATENCY.md) for a short checklist-focused version.

## What this MVP does not do

- Clipboard or file transfer
- Bluetooth hopping / Logitech protocol cloning
- Linux as a first-class desktop target (Linux mock is for CI/dev)
- Multi-monitor spatial graphs (single display per machine for now)
- Auto-discovery / mDNS

## Local config

Copy `sideshift.example.json` to `sideshift.json` and edit values:

```bash
cp sideshift.example.json sideshift.json
```

Key fields:
- `local.machine_name`: unique ID for this machine
- `local.role`: `server` or `client`
- `control_bind` / `data_bind`: local bind addresses
- `shared_secret`: passphrase shared by both machines
- `peers[*]`: remote machine addresses + relative position (`left|right|above|below`)

## Run the daemon

Server machine:

```bash
cargo run -p sideshift -- run --config sideshift.json --role server
```

Client machine:

```bash
cargo run -p sideshift -- run --config sideshift.json --role client
```

The CLI prints:
- connection/authentication status
- handoff transitions
- injected input events on mock/dev paths

Latency hook example:

```bash
cargo run -p sideshift -- run --config sideshift.json --role client --log-latency
```

## macOS + Windows setup path

### 1) Choose ownership machine

Pick the machine that physically owns the Logitech-paired devices as SideShift **server**.

### 2) Network + config

- Place both machines on the same trusted LAN
- Set fixed/private IPs or DHCP reservations if possible
- Configure each machine’s JSON with matching `shared_secret`
- Mirror peer addresses and opposite positions

### 3) Permissions

#### macOS (required for real capture/injection work)

Grant SideShift binary:
- **Accessibility** permission
- **Input Monitoring** permission

You may also need Terminal/runner permission if launching from terminal while developing.

#### Windows (required for real injection/capture work)

Allow the binary through Windows Firewall on private networks.
Depending on final Win32 hook/injection approach, running elevated may be required for certain desktop contexts.

### 4) Launch order

1. Start server
2. Start client
3. Move cursor to configured handoff edge on server machine
4. Confirm client receives handoff status and input events

## Development

Prerequisites:
- Rust toolchain (this repo currently validates on Rust 1.83 in CI)

Commands:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Linux CI note

Linux uses mock platform adapters so protocol/layout/state behavior remains testable in CI even without native desktop APIs.
This includes latency-sensitive framing/state logic, while native capture/inject latency still requires macOS/Windows hardware validation.

## Comparison: SideShift vs Deskflow vs Logi Flow

### SideShift MVP (this repo)
- Focus: minimal, auditable codebase with clear architecture
- Protocol: PSK-authenticated encrypted channels over TCP + UDP
- UI: CLI only
- Platforms: macOS/Windows adapters scaffolded; Linux mock for CI

### Deskflow / Synergy-line projects
- More mature feature set and production hardening
- Broader compatibility and UI tooling
- Better immediate fit if you need turnkey daily-driver behavior today

### Logitech Flow
- Tight Logitech ecosystem integration
- Device-centric switching behavior
- Not an open, vendor-neutral network daemon

## Security model (MVP)

SideShift assumes a **trusted LAN** with a strong shared secret:
- all control/input packets are authenticated and encrypted
- no automatic peer discovery in MVP (manual endpoint config only)
- no cloud relay path

See [SECURITY.md](SECURITY.md) for details and reporting guidance.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](LICENSE).
