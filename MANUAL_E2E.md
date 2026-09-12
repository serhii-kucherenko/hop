# hop manual end-to-end checklist

Use this checklist on real hardware before shipping changes to handoff behavior.

Preconditions:
- Windows server owns physical mouse and keyboard.
- Mac client is paired and configured to the right of Windows (`position: "right"` on server).
- Both daemons are running and connected.

A. Enter right edge once:
- Push into the Windows right sticky edge.
- Expected: Mac receives mouse and keyboard input.
- Expected: Windows local apps do not receive mouse movement, clicks, or typing.

B. Exit left edge on Mac:
- Push into the Mac left sticky edge.
- Expected: `HandoffEnd` return to server.
- Expected: Windows receives mouse and keyboard input again.
- Expected: Mac stops injecting events.

C. Rapid enter/exit thrashing:
- Flick across Windows right edge and Mac left edge repeatedly.
- Expected: no crash, no mirrored input, no stuck remote state.

D. Enter then kill Mac client:
- Enter remote mode from Windows.
- Force-stop the Mac client process.
- Expected: Windows recovers local input ownership quickly.

E. Enter then kill Windows server:
- Enter remote mode from Windows.
- Force-stop the Windows server process.
- Expected: Mac stops receiving forwarded input and remains in safe local state.

F. Second enter while already remote:
- While remote is active, keep pushing further into the Windows handoff edge.
- Expected: no second handoff transition and no duplicate handoff commit.

G. Keyboard follows ownership:
- In remote mode, type several keys.
- Expected: keys appear only on Mac.
- After return, type again.
- Expected: keys appear only on Windows.

H. Quit behavior:
- Press Ctrl+C on server and client in separate runs.
- Expected: app stops cleanly and prints a stopped message.
- Expected: local input remains usable after stop.

I. Restart and reconnect:
- Restart both daemons after quit.
- Expected: pair/connect still works and A/B still pass.

J. Sticky threshold safety:
- Brush the active edge lightly without sustained outbound push.
- Expected: no accidental handoff.
