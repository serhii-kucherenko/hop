# hop

hop forwards mouse and keyboard input between two machines over a local network.

Latency budget: 1-3ms target and 5ms hard max on wired LAN; Wi-Fi is best-effort and may exceed that.

## Quick start

1. Install `hop` on each machine.

Prefer release binaries from GitHub Releases:

- macOS: `hop-x86_64-apple-darwin.tar.gz` or `hop-aarch64-apple-darwin.tar.gz`
- Windows: `hop-x86_64-pc-windows-msvc.zip`

If release binaries are not available yet, build locally:

```bash
git clone https://github.com/serhii-kucherenko/hop.git
cd hop
cargo build --release -p hop
```

2. On the machine that owns keyboard/mouse first (server):

```bash
hop init --role server
hop pair
```

`hop pair` prints a LAN-only pairing code and LAN IP list.

3. On the second machine (client):

```bash
hop init --role client
hop pair <code-from-server>
```

Alternative when you already know server host + shared secret:

```bash
hop join <server-host> --secret <shared-secret>
```

4. Run onboarding checks:

```bash
hop doctor
```

`hop doctor` reports config presence, secret status, peer control reachability, permission status, and screen size. It exits non-zero on hard blockers.

5. Start both sides:

Server:

```bash
hop run --role server
```

Client:

```bash
hop run --role client
```

6. Flick across the configured edge on the server machine.

Use `--config <path>` with any command if you do not want `./hop.json`.

## Permissions

- macOS: Accessibility + Input Monitoring are required. `hop doctor --open-permissions` can open the relevant System Settings panes.
- Windows: hook/injection reliability is highest when `hop` and target apps run at matching privilege levels (same elevation).

## How it works

`hop` keeps input ownership on the server machine and forwards events over LAN to the client machine. Control messages run on an encrypted TCP channel, and input events run on encrypted UDP datagrams to minimize handoff latency. It is a network handoff model, not Bluetooth re-pairing.

Pairing is LAN-only in MVP. Treat pairing codes and shared secrets like passwords, and do not share them outside your trusted network.

## Permissions and known gaps

- macOS secure input contexts (some password/login flows) can block keyboard capture/injection.
- Windows elevated or secure desktop surfaces (UAC/admin contexts) can block hooks or injection when privilege levels do not match.
- Linux remains mock-only in CI; native desktop capture/injection validation is focused on macOS and Windows.
- iOS/iPadOS are not handoff targets.

## More detail

- Latency notes and validation checklist: [LATENCY.md](LATENCY.md)
- Trust model and reporting guidance: [SECURITY.md](SECURITY.md)
- Contribution workflow: [CONTRIBUTING.md](CONTRIBUTING.md)

## Comparison (short)

- `hop`: small CLI-first codebase focused on low-latency LAN handoff.
- Deskflow/Synergy family: broader feature set and more mature desktop UX.
- Logi Flow: tight Logitech ecosystem integration, but not an open protocol daemon.
- `hop` does not attempt Bluetooth device switching; it forwards input over the network.
