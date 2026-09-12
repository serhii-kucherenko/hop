# hop

hop forwards mouse and keyboard input between two machines over a local network.

Latency budget: 1-3ms target and 5ms hard max on wired LAN; Wi-Fi is best-effort and may exceed that.

## Quick start (Mac primary -> Windows secondary)

1. Build `hop` on both machines.

```bash
git clone https://github.com/serhii-kucherenko/hop.git
cd hop
cargo build --release -p hop
```

2. Create config files on both machines.

```bash
cp hop.example.json hop.json
```

3. Edit `hop.json` on each machine.
   - Keep the same `shared_secret` on both.
   - Set `local.role` to `server` on the primary machine and `client` on the secondary.
   - Set `peers` so each machine points to the other machine's IP/ports.

Minimal shape:

```json
{
  "local": {
    "machine_name": "macbook-pro",
    "role": "server",
    "control_bind": "0.0.0.0:4600",
    "data_bind": "0.0.0.0:4601",
    "shared_secret": "replace-with-strong-passphrase",
    "screen_width": 1728,
    "screen_height": 1117
  },
  "peers": [
    {
      "machine_name": "windows-desktop",
      "control_addr": "192.168.1.52:4600",
      "data_addr": "192.168.1.52:4601",
      "position": "right"
    }
  ]
}
```

4. On macOS, grant permissions before first real run.
   - System Settings -> Privacy & Security -> Accessibility
   - System Settings -> Privacy & Security -> Input Monitoring
   - Add the `hop` binary (or Terminal while developing), enable both, then relaunch the process.

5. Start server on the primary machine.

```bash
./target/release/hop run --config hop.json --role server
```

6. Start client on the secondary machine.

```bash
./target/release/hop run --config hop.json --role client
```

7. Flick the cursor across the configured edge on the primary machine; cursor and keyboard focus should follow on the secondary machine.

## How it works

`hop` keeps input ownership on the server machine and forwards events over LAN to the client machine. Control messages run on an encrypted TCP channel, and input events run on encrypted UDP datagrams to minimize handoff latency. It is a network handoff model, not Bluetooth re-pairing.

## Permissions and known gaps

- macOS secure input contexts (some password/login flows) can block keyboard capture/injection.
- Windows elevated or secure desktop surfaces (UAC/admin contexts) can block hooks or injection when privilege levels do not match.
- Linux remains mock-only in CI; native desktop capture/injection validation is focused on macOS and Windows.

## More detail

- Latency notes and validation checklist: [LATENCY.md](LATENCY.md)
- Trust model and reporting guidance: [SECURITY.md](SECURITY.md)
- Contribution workflow: [CONTRIBUTING.md](CONTRIBUTING.md)

## Comparison (short)

- `hop`: small CLI-first codebase focused on low-latency LAN handoff.
- Deskflow/Synergy family: broader feature set and more mature desktop UX.
- Logi Flow: tight Logitech ecosystem integration, but not an open protocol daemon.
- `hop` does not attempt Bluetooth device switching; it forwards input over the network.
