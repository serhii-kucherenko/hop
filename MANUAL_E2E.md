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

For each position above, run all cases A-J:

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

G. Keyboard follows ownership:
- In remote mode, type several keys.
- Expected: keys appear only on client.
- After return, type again.
- Expected: keys appear only on server.

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
