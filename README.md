# hop

hop forwards mouse and keyboard input between two machines over a local network.

Latency budget: 1-3ms target and 5ms hard max on wired LAN; Wi-Fi is best-effort and may exceed that.

## Quick start

1. Install `hop` on each machine.

Prefer release binaries from GitHub Releases:

- macOS: `hop-x86_64-apple-darwin.tar.gz` or `hop-aarch64-apple-darwin.tar.gz`
- Windows: `hop-x86_64-pc-windows-msvc.zip`

If a release has no attached binaries yet, build locally:

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

Edge crossing switches ownership automatically while the daemon keeps running: the active machine receives input, and the inactive machine does not. Control is exclusive, not mirrored.

Edge detection and return warps use the full virtual desktop bounds (multi-monitor aware), not only the primary monitor metrics.

Switch matrix (server perspective): right -> client returns on left, left -> return on right, above -> return on bottom, below -> return on top. Behavior is symmetric when roles swap: whichever machine owns physical input is the server for that run.

Windows-primary default layout: the first paired client is on the server's right (`position: "right"`), so moving to the far right edge of the Windows virtual desktop hands off to a MacBook on the right; push into the Mac's left edge to return control to Windows. Override with `hop pair --position <left|right|above|below>` if needed.

Modifier swap support is directional and controlled by `local.swap_ctrl_cmd` on the client side:
- macOS client (default `true`): incoming Windows `Ctrl` is injected as Mac `Command`, and incoming Windows `Win` is injected as Mac `Control`.
- Windows client (default `false`): when enabled, incoming Mac `Command` is injected as Windows `Ctrl`, and incoming Mac `Control` is injected as Windows `Win`.

This keeps common shortcuts natural when driving macOS from Windows or Windows from macOS. Set `local.swap_ctrl_cmd` explicitly per machine when needed.

Stop hop with Ctrl+C in each machine terminal; hop prints `hop stopped; local input restored` and releases local input.

To run without blocking a terminal:

```bash
hop run --background
hop stop
```

Background logs are written next to your config as `hop.log`.

Autostart (manual MVP):
- macOS: create a LaunchAgent that runs `hop run --background` at login.
- Windows: add `hop run --background` to Task Scheduler (At log on) or Startup.

`hop doctor` reports current screen geometry, effective `swap_ctrl_cmd`, clipboard backend readiness, and whether the binary was built from source or release pipeline.

Manual end-to-end validation checklist: [MANUAL_E2E.md](MANUAL_E2E.md)

Default config path:
- Unix/macOS: `~/.config/hop/config.json` (or `$XDG_CONFIG_HOME/hop/config.json`)
- Windows: `%APPDATA%\hop\config.json`

Use `--config <path>` with any command to override the config location.

## Permissions

- macOS: Accessibility + Input Monitoring are required. `hop` checks permissions during real runs and prints/opens the relevant System Settings panes when needed.
- Windows: hook/injection reliability is highest when `hop` and target apps run at matching privilege levels (same elevation).

## How it works

`hop` keeps input ownership on the server machine and forwards events over LAN to the client machine. Control messages run on an encrypted TCP channel, and input events run on encrypted UDP datagrams to minimize handoff latency. It is a network handoff model, not Bluetooth re-pairing.

### Clipboard sync (MVP)

During remote ownership, `hop` also syncs clipboard changes over the encrypted control channel (TCP). This path is separate from the UDP input hot path.

Supported clipboard payloads:
- Text (Unicode)
- Images (`PNG`)
- Files (staged copy; files are transferred and written to a temporary staging directory on the receiving machine, then placed on that machine's clipboard as local file paths)

Safety/size limits in MVP:
- text: up to 1,000,000 bytes
- image (`PNG`): up to 8 MiB
- files: up to 8 files, each up to 8 MiB, with 24 MiB total payload cap

Staged clipboard files are written under the OS temp directory in `hop-clipboard/<machine>/...`.

Pairing is LAN-only in MVP. Treat pairing codes and shared secrets like passwords, and do not share them outside your trusted network.

## Permissions and known gaps

- macOS secure input contexts (some password/login flows) can block keyboard capture/injection.
- Windows elevated or secure desktop surfaces (UAC/admin contexts) can block hooks or injection when privilege levels do not match.
- Linux remains mock-only in CI; native desktop capture/injection validation is focused on macOS and Windows.
- iOS/iPadOS are not handoff targets.

## More detail

- Latency notes and validation checklist: [LATENCY.md](LATENCY.md)
- Latency benchmark command: `hop bench [--host <host:port>] [--samples N] [--timeout-ms N] [--strict]`
- Trust model and reporting guidance: [SECURITY.md](SECURITY.md)
- Contribution workflow: [CONTRIBUTING.md](CONTRIBUTING.md)
- Optional power-user commands: `hop init`, `hop pair`, `hop join`, `hop run`, `hop doctor`

## Comparison (short)

- `hop`: small CLI-first codebase focused on low-latency LAN handoff.
- Deskflow/Synergy family: broader feature set and more mature desktop UX.
- Logi Flow: tight Logitech ecosystem integration, but not an open protocol daemon.
- `hop` does not attempt Bluetooth device switching; it forwards input over the network.
