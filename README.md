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

With `handoff.mode: "auto"` (default), hop prefers native Logitech Easy-Switch host changes when Logi Options+ and compatible multi-host devices are detected, and falls back to network input forwarding otherwise.

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

`hop` keeps an encrypted control channel over TCP for handoff coordination and clipboard sync.

- In `network` mode, input ownership stays on the server and mouse/keyboard events are forwarded to the client over encrypted UDP datagrams.
- In `logi` / `auto` mode, edge handoff prefers **direct HID++ ChangeHost (feature 0x1814)** on Logitech devices (MX mouse + Casa Keys / MX Keys), equivalent to pressing Easy-Switch 1/2/3. Options+ IPC is optional and used only when HID++ is unavailable.
- Windows HID++ works without Options+. macOS HID++ is best-effort and may require Input Monitoring for the `hop` binary.
- Bolt receivers typically switch faster/more reliably than BLE; if a channel's host is asleep, the device can park on a dead channel until you wake that host or use the physical Easy-Switch button.
- In `auto` mode (default), hop chooses Logi when HID++ (or Options+) devices with a peer host mapping are ready; otherwise it uses `network`.

If Logi switching is requested but fails or times out, hop falls back to the network handoff path for that session.

### Handoff config

```json
{
  "handoff": {
    "mode": "auto",
    "logi_peer_host_index": {
      "macbook-pro": 1
    },
    "logi_peer_device_host_index": {
      "macbook-pro": { "keyboard": 1, "mouse": 2 }
    },
    "logi_local_device_host_index": { "keyboard": 0, "mouse": 0 }
  }
}
```

- `handoff.mode`: `auto` (default) | `logi` | `network`
- `handoff.logi_peer_host_index`: uniform peer -> Easy-Switch host index map (`0` = channel 1, `1` = channel 2, `2` = channel 3). Useful when keyboard and mouse share one channel. **Required for HID++-only setups** unless you set per-device maps (HID++ does not expose host names). If omitted and Options+ is available, hop may auto-map using Options+ host names/OS hints.
- `handoff.logi_peer_device_host_index`: optional peer -> `{"keyboard"|"mouse" -> host index}` map. When present for a peer, hop switches keyboard and mouse independently (for setups where Easy-Switch channels differ per device).
- `handoff.logi_local_device_host_index`: optional local `{"keyboard"|"mouse" -> host index}` used when switching back to this machine. Falls back to the detected local host slot when omitted.
- Run `hop doctor` to see HID++ devices, Options+ status, uniform peer→channel maps, and per-device maps.

### Clipboard sync (MVP)

During remote ownership, `hop` also syncs clipboard changes over the encrypted control channel (TCP). This path is separate from the UDP input hot path and stays active in both `network` and `logi` handoff modes.

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
- Logi-native handoff requires Logi Options+ agent on both machines and Logitech multi-host devices (for example MX/Casa devices with Easy-Switch channels).

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
- `hop` can use native Logitech Easy-Switch through Options+ when available, with network fallback always available.
