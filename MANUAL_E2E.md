# hop manual end-to-end checklist

Use this checklist on real hardware before shipping handoff changes.

Roles:
- Whichever machine owns physical mouse/keyboard in this run is the server.
- The other machine is the client.
- Repeat this checklist with roles swapped to confirm symmetric behavior.

Position matrix (from server perspective):
- client on right: enter on server right edge, return on client left edge
- client on left: enter on server left edge, return on client right edge
- client above: enter on server top edge, return on client bottom edge
- client below: enter on server bottom edge, return on client top edge

Virtual desktop geometry:
- When either machine has multiple displays, test against the full virtual desktop bounds.
- Confirm handoff triggers on the outer-most virtual edge (not only a primary display edge).

For each position above, run all cases A-M:

A. Enter edge once:
- Push into the configured server enter edge.
- Expected: client receives mouse and keyboard input.
- Expected: server local apps do not receive movement, clicks, or typing.

B. Exit edge on client:
- Push into the configured client return edge.
- Expected: `HandoffEnd` returns control to server.
- Expected: server receives mouse and keyboard input again.
- Expected: client stops injecting events.

C. Rapid enter/exit thrashing:
- Flick across the configured enter and return edges repeatedly.
- Expected: no crash, no mirrored input, no stuck remote state.

D. Enter then kill client:
- Enter remote mode from server.
- Force-stop the client process.
- Expected: server recovers local input ownership quickly.

E. Enter then kill server:
- Enter remote mode from server.
- Force-stop the server process.
- Expected: client stops receiving forwarded input and remains in safe local state.

F. Second enter while already remote:
- While remote is active, keep pushing further into the server enter edge.
- Expected: no second handoff transition and no duplicate handoff commit.

G. Keyboard follows ownership + combos:
- In remote mode, type several keys.
- Expected: keys appear only on client.
- After return, type again.
- Expected: keys appear only on server.
- In remote mode, hold Ctrl (Windows server) and press `C`, `V`, `A`, and `Z` in a text editor.
- Expected: client receives standard shortcuts while modifier is held.
- On macOS server runs, hold Command and press the same keys.
- Expected: client receives Command shortcuts while modifier is held.

H. Quit behavior:
- Press Ctrl+C on server and client in separate runs.
- Expected: app stops cleanly and prints `hop stopped; local input restored`.
- Expected: local input remains usable after stop.

I. Restart and reconnect:
- Restart both daemons after quit.
- Expected: pair/connect still works and A/B still pass.

J. Sticky threshold safety:
- Brush the active edge lightly without sustained outbound push.
- Expected: no accidental handoff.

K. Cursor feel and stability (Windows server → macOS client):
- On Windows server, set a non-default pointer speed and toggle "Enhance pointer precision".
- Enter remote mode and move in medium-speed circles and short flicks.
- Expected: cursor feel on macOS changes with the Windows pointer settings.
- Expected: no visible cursor shake/jitter during moderate-speed movement.

L. Mouse click semantics and side buttons:
- In remote mode, double-click left button on client desktop/file list.
- Expected: double-click action triggers reliably (for example, open item/select word).
- Repeat with right and middle button in an app that responds to those buttons.
- Expected: each button down/up pair is delivered correctly without missed clicks.
- On a mouse with side buttons, press Back (X1) and Forward (X2) in a browser on the client.
- Expected: client navigates back/forward as if locally clicked.

M. Clipboard sync (bidirectional while remote ownership is active):
- Enter remote mode and copy plain text on the active machine.
- Expected: text paste on the other machine matches exactly, including Unicode characters.
- Copy an image (PNG source is preferred) on the active machine.
- Expected: image pastes on the other machine.
- Copy one or more files on the active machine.
- Expected: files appear in the other machine clipboard and can be pasted from a staged local temp location.
- While still remote, repeat text/image/file copy from the other side after return handoff.
- Expected: sync works in both directions without ping-pong loops or repeated clipboard churn.

N. Multi-monitor edge and warp behavior:
- Arrange two displays so one monitor has negative X or Y in OS arrangement.
- Push into each configured handoff edge on the server.
- Expected: handoff begins at virtual desktop outer edge, including negative-coordinate layouts.
- End handoff from client return edge.
- Expected: local cursor warps to a safe point inside the opposite edge of the full virtual desktop.

O. Modifier swap matrix (Mac <-> Windows):
- Windows server -> macOS client (default swap): hold `Ctrl` and press `C`, `V`, `A`, `Z`.
- Expected: macOS receives `Command` shortcuts.
- macOS server -> Windows client (set `local.swap_ctrl_cmd=true` on Windows client): hold `Command` and press `C`, `V`, `A`, `Z`.
- Expected: Windows receives `Ctrl` shortcuts.
- In both directions, verify disabled mode (`local.swap_ctrl_cmd=false`) preserves native modifiers.

P. Background mode lifecycle:
- Start hop with `hop run --background`.
- Confirm terminal returns immediately while handoff still works.
- Run `hop stop`.
- Expected: daemon exits and local input is restored.

Q. Bench command sanity:
- With peer daemon running, execute `hop bench`.
- Expected: p50/p95 one-way and RTT values are printed against 1-3ms goal and 5ms hard max.
- If using wired LAN and `--strict`, command should fail when p95 one-way exceeds 5ms.

Additional hybrid handoff cases (Logi Options+ + Easy-Switch):

R. Auto mode chooses Logi path when available:
- Set `handoff.mode` to `auto`.
- Ensure Logi Options+ agent is running on both machines and paired MX/Casa keyboard+mouse are visible in Options+.
- With Mac hop online, push into Windows right edge (Mac on right).
- Expected: devices switch to the Mac Easy-Switch host, and hop does not require UDP input forwarding for that handoff.
- With Mac hop offline/off: same edge push must do nothing (no channel switch).

S. Return edge switches back to owner channel:
- While on Mac in Logi handoff, push into Mac left edge (return to Windows).
- Expected: client sends handoff end and switches devices back to Windows/owner channel.
- Expected: server regains local ownership cleanly.
- With Windows hop offline: Mac left-edge return must do nothing.

T. Logi failure fallback:
- Keep `handoff.mode` as `auto` or `logi`, then stop Options+ agent on one side (or use a device without Easy-Switch).
- Trigger handoff with peer still online.
- Expected: hop falls back to existing network handoff behavior (A/B still pass).

U. Network-only mode bypasses Logi:
- Set `handoff.mode` to `network` with Options+ still running.
- Trigger handoff.
- Expected: behavior matches existing UDP forwarding path; no native channel switch is attempted.

V. Doctor/onboarding detection summary:
- Run `hop doctor`.
- Run fresh onboarding (`hop` on server then `hop <code>` on client).
- Expected: summary includes OS, hostname, role hint, screen geometry, Options+ presence, detected Easy-Switch devices, and peer->channel mapping.
