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

2. On the machine that owns keyboard/mouse first (server), run:

```bash
hop
```

If this machine is not configured yet, `hop` becomes the server, creates a shared secret, prints a short pairing code plus LAN IPs, waits for a peer, then starts handoff automatically.

3. On the second machine (client), run:

```bash
hop <code-from-server>
```

This writes config, connects to the server, and starts the client automatically.

4. Push into the configured edge on the server machine (sticky edge handoff). You do not need to move beyond the display bounds.

Windows-primary default layout: the first paired client is on the server's right (`position: "right"`), so moving to the far right edge of the Windows screen hands off to a MacBook on the right; moving left from the Mac returns control. Override with `hop pair --position <left|right|above|below>` if needed.

Default config path:
- Unix/macOS: `~/.config/hop/config.json` (or `$XDG_CONFIG_HOME/hop/config.json`)
- Windows: `%APPDATA%\hop\config.json`

Use `--config <path>` with any command to override the config location.

## Permissions

- macOS: Accessibility + Input Monitoring are required. `hop` checks permissions during real runs and prints/opens the relevant System Settings panes when needed.
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
- Optional power-user commands: `hop init`, `hop pair`, `hop join`, `hop run`, `hop doctor`

## Comparison (short)

- `hop`: small CLI-first codebase focused on low-latency LAN handoff.
- Deskflow/Synergy family: broader feature set and more mature desktop UX.
- Logi Flow: tight Logitech ecosystem integration, but not an open protocol daemon.
- `hop` does not attempt Bluetooth device switching; it forwards input over the network.
