# Contributing to SideShift

Thanks for helping improve SideShift.

## Scope for this MVP phase

Please keep changes focused on:
- protocol correctness
- handoff behavior
- macOS/Windows adapter completion
- test coverage and reliability

Out of scope for now:
- clipboard/file transfer
- Bluetooth device switching logic
- large GUI efforts

## Setup

1. Install Rust
2. Clone the repository
3. Copy example config:

```bash
cp sideshift.example.json sideshift.json
```

## Development checks

Run all checks before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Pull request expectations

- Keep changes small and purpose-driven
- Add or update tests for behavior changes
- Update README/docs when user-facing setup or behavior changes
- Call out platform-specific caveats clearly (especially macOS/Windows permissions)

## Platform implementation notes

The repository uses trait boundaries in `sideshift-core/src/platform/`:
- `LocalInputCapture`
- `RemoteInputInjector`
- `ScreenInfoProvider`
- `CursorController`

When improving macOS/Windows support, prefer implementing one trait at a time with tests around the handoff behavior that can still run on Linux.
