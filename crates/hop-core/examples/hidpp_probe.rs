//! Probe Logitech HID++ ChangeHost coverage on this machine.
//!
//! Run: `cargo run -p hop-core --example hidpp_probe --release`

fn main() {
    hop_core::logi::dump_hidpp_discovery();
}
