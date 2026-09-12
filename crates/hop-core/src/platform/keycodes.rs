pub(crate) fn mac_keycode_to_wire(mac_keycode: u16) -> Option<u16> {
    match mac_keycode {
        0 => Some(0x41),   // A
        1 => Some(0x53),   // S
        2 => Some(0x44),   // D
        3 => Some(0x46),   // F
        4 => Some(0x48),   // H
        5 => Some(0x47),   // G
        6 => Some(0x5A),   // Z
        7 => Some(0x58),   // X
        8 => Some(0x43),   // C
        9 => Some(0x56),   // V
        11 => Some(0x42),  // B
        12 => Some(0x51),  // Q
        13 => Some(0x57),  // W
        14 => Some(0x45),  // E
        15 => Some(0x52),  // R
        16 => Some(0x59),  // Y
        17 => Some(0x54),  // T
        18 => Some(0x31),  // 1
        19 => Some(0x32),  // 2
        20 => Some(0x33),  // 3
        21 => Some(0x34),  // 4
        22 => Some(0x36),  // 6
        23 => Some(0x35),  // 5
        24 => Some(0xBB),  // =
        25 => Some(0x39),  // 9
        26 => Some(0x37),  // 7
        27 => Some(0xBD),  // -
        28 => Some(0x38),  // 8
        29 => Some(0x30),  // 0
        30 => Some(0xDD),  // ]
        31 => Some(0x4F),  // O
        32 => Some(0x55),  // U
        33 => Some(0xDB),  // [
        34 => Some(0x49),  // I
        35 => Some(0x50),  // P
        36 => Some(0x0D),  // Return
        37 => Some(0x4C),  // L
        38 => Some(0x4A),  // J
        39 => Some(0xDE),  // '
        40 => Some(0x4B),  // K
        41 => Some(0xBA),  // ;
        42 => Some(0xDC),  // \
        43 => Some(0xBC),  // ,
        44 => Some(0xBF),  // /
        45 => Some(0x4E),  // N
        46 => Some(0x4D),  // M
        47 => Some(0xBE),  // .
        48 => Some(0x09),  // Tab
        49 => Some(0x20),  // Space
        50 => Some(0xC0),  // `
        51 => Some(0x08),  // Backspace
        53 => Some(0x1B),  // Escape
        54 => Some(0x5C),  // Right Command
        55 => Some(0x5B),  // Left Command
        56 => Some(0xA0),  // Left Shift
        57 => Some(0x14),  // Caps Lock
        58 => Some(0xA4),  // Left Option
        59 => Some(0xA2),  // Left Control
        60 => Some(0xA1),  // Right Shift
        61 => Some(0xA5),  // Right Option
        62 => Some(0xA3),  // Right Control
        64 => Some(0x80),  // F17
        65 => Some(0x6E),  // Keypad .
        67 => Some(0x6A),  // Keypad *
        69 => Some(0x6B),  // Keypad +
        71 => Some(0x0C),  // Keypad Clear
        72 => Some(0xAF),  // Volume Up
        73 => Some(0xAE),  // Volume Down
        74 => Some(0xAD),  // Mute
        75 => Some(0x6F),  // Keypad /
        76 => Some(0x0D),  // Keypad Enter
        78 => Some(0x6D),  // Keypad -
        79 => Some(0x81),  // F18
        80 => Some(0x82),  // F19
        81 => Some(0xBB),  // Keypad =
        82 => Some(0x60),  // Keypad 0
        83 => Some(0x61),  // Keypad 1
        84 => Some(0x62),  // Keypad 2
        85 => Some(0x63),  // Keypad 3
        86 => Some(0x64),  // Keypad 4
        87 => Some(0x65),  // Keypad 5
        88 => Some(0x66),  // Keypad 6
        89 => Some(0x67),  // Keypad 7
        91 => Some(0x68),  // Keypad 8
        92 => Some(0x69),  // Keypad 9
        96 => Some(0x74),  // F5
        97 => Some(0x75),  // F6
        98 => Some(0x76),  // F7
        99 => Some(0x72),  // F3
        100 => Some(0x77), // F8
        101 => Some(0x78), // F9
        103 => Some(0x7A), // F11
        105 => Some(0x7C), // F13
        106 => Some(0x7F), // F16
        107 => Some(0x7D), // F14
        109 => Some(0x79), // F10
        111 => Some(0x7B), // F12
        113 => Some(0x7E), // F15
        114 => Some(0x2F), // Help
        115 => Some(0x24), // Home
        116 => Some(0x21), // Page Up
        117 => Some(0x2E), // Forward Delete
        118 => Some(0x73), // F4
        119 => Some(0x23), // End
        120 => Some(0x71), // F2
        121 => Some(0x22), // Page Down
        122 => Some(0x70), // F1
        123 => Some(0x25), // Left Arrow
        124 => Some(0x27), // Right Arrow
        125 => Some(0x28), // Down Arrow
        126 => Some(0x26), // Up Arrow
        _ => None,
    }
}

pub(crate) fn wire_to_mac_keycode(wire_keycode: u16) -> Option<u16> {
    match wire_keycode {
        0x08 => Some(51),  // Backspace
        0x09 => Some(48),  // Tab
        0x0D => Some(36),  // Return
        0x14 => Some(57),  // Caps Lock
        0x1B => Some(53),  // Escape
        0x20 => Some(49),  // Space
        0x21 => Some(116), // Page Up
        0x22 => Some(121), // Page Down
        0x23 => Some(119), // End
        0x24 => Some(115), // Home
        0x25 => Some(123), // Left Arrow
        0x26 => Some(126), // Up Arrow
        0x27 => Some(124), // Right Arrow
        0x28 => Some(125), // Down Arrow
        0x2C => Some(105), // Print Screen -> F13 fallback
        0x2D => Some(114), // Insert -> Help fallback
        0x2E => Some(117), // Delete
        0x2F => Some(114), // Help
        0x30 => Some(29),  // 0
        0x31 => Some(18),  // 1
        0x32 => Some(19),  // 2
        0x33 => Some(20),  // 3
        0x34 => Some(21),  // 4
        0x35 => Some(23),  // 5
        0x36 => Some(22),  // 6
        0x37 => Some(26),  // 7
        0x38 => Some(28),  // 8
        0x39 => Some(25),  // 9
        0x41 => Some(0),   // A
        0x42 => Some(11),  // B
        0x43 => Some(8),   // C
        0x44 => Some(2),   // D
        0x45 => Some(14),  // E
        0x46 => Some(3),   // F
        0x47 => Some(5),   // G
        0x48 => Some(4),   // H
        0x49 => Some(34),  // I
        0x4A => Some(38),  // J
        0x4B => Some(40),  // K
        0x4C => Some(37),  // L
        0x4D => Some(46),  // M
        0x4E => Some(45),  // N
        0x4F => Some(31),  // O
        0x50 => Some(35),  // P
        0x51 => Some(12),  // Q
        0x52 => Some(15),  // R
        0x53 => Some(1),   // S
        0x54 => Some(17),  // T
        0x55 => Some(32),  // U
        0x56 => Some(9),   // V
        0x57 => Some(13),  // W
        0x58 => Some(7),   // X
        0x59 => Some(16),  // Y
        0x5A => Some(6),   // Z
        0x5B => Some(55),  // Left Command
        0x5C => Some(54),  // Right Command
        0x60 => Some(82),  // Numpad 0
        0x61 => Some(83),  // Numpad 1
        0x62 => Some(84),  // Numpad 2
        0x63 => Some(85),  // Numpad 3
        0x64 => Some(86),  // Numpad 4
        0x65 => Some(87),  // Numpad 5
        0x66 => Some(88),  // Numpad 6
        0x67 => Some(89),  // Numpad 7
        0x68 => Some(91),  // Numpad 8
        0x69 => Some(92),  // Numpad 9
        0x6A => Some(67),  // Numpad *
        0x6B => Some(69),  // Numpad +
        0x6D => Some(78),  // Numpad -
        0x6E => Some(65),  // Numpad .
        0x6F => Some(75),  // Numpad /
        0x70 => Some(122), // F1
        0x71 => Some(120), // F2
        0x72 => Some(99),  // F3
        0x73 => Some(118), // F4
        0x74 => Some(96),  // F5
        0x75 => Some(97),  // F6
        0x76 => Some(98),  // F7
        0x77 => Some(100), // F8
        0x78 => Some(101), // F9
        0x79 => Some(109), // F10
        0x7A => Some(103), // F11
        0x7B => Some(111), // F12
        0x7C => Some(105), // F13
        0x7D => Some(107), // F14
        0x7E => Some(113), // F15
        0x7F => Some(106), // F16
        0x80 => Some(64),  // F17
        0x81 => Some(79),  // F18
        0x82 => Some(80),  // F19
        0x90 => Some(71),  // Num Lock -> Keypad Clear fallback
        0xA0 => Some(56),  // Left Shift
        0xA1 => Some(60),  // Right Shift
        0xA2 => Some(59),  // Left Control
        0xA3 => Some(62),  // Right Control
        0xA4 => Some(58),  // Left Option
        0xA5 => Some(61),  // Right Option
        0xAD => Some(74),  // Mute
        0xAE => Some(73),  // Volume Down
        0xAF => Some(72),  // Volume Up
        0xBA => Some(41),  // ;
        0xBB => Some(24),  // = and keypad =
        0xBC => Some(43),  // ,
        0xBD => Some(27),  // -
        0xBE => Some(47),  // .
        0xBF => Some(44),  // /
        0xC0 => Some(50),  // `
        0xDB => Some(33),  // [
        0xDC => Some(42),  // \
        0xDD => Some(30),  // ]
        0xDE => Some(39),  // '
        _ => None,
    }
}

pub(crate) fn windows_vk_to_wire(vk: u32) -> Option<u16> {
    u16::try_from(vk).ok()
}

pub(crate) fn wire_to_windows_vk(wire_keycode: u16) -> u16 {
    wire_keycode
}

#[cfg(test)]
mod tests {
    use super::{mac_keycode_to_wire, windows_vk_to_wire, wire_to_mac_keycode, wire_to_windows_vk};

    #[test]
    fn mac_letters_roundtrip_through_wire_codes() {
        let letters = [0_u16, 1, 2, 6, 12, 13, 14, 15];
        for mac_key in letters {
            let wire = mac_keycode_to_wire(mac_key).expect("wire mapping should exist");
            let mac_roundtrip = wire_to_mac_keycode(wire).expect("mac mapping should exist");
            assert_eq!(mac_roundtrip, mac_key);
        }
    }

    #[test]
    fn wire_codes_cover_modifiers_and_arrows() {
        assert_eq!(wire_to_mac_keycode(0xA0), Some(56));
        assert_eq!(wire_to_mac_keycode(0xA1), Some(60));
        assert_eq!(wire_to_mac_keycode(0xA2), Some(59));
        assert_eq!(wire_to_mac_keycode(0xA4), Some(58));
        assert_eq!(wire_to_mac_keycode(0x25), Some(123));
        assert_eq!(wire_to_mac_keycode(0x27), Some(124));
        assert_eq!(wire_to_mac_keycode(0x28), Some(125));
        assert_eq!(wire_to_mac_keycode(0x26), Some(126));
    }

    #[test]
    fn wire_codes_include_common_windows_extended_keys() {
        assert_eq!(wire_to_mac_keycode(0x2C), Some(105));
        assert_eq!(wire_to_mac_keycode(0x2D), Some(114));
        assert_eq!(wire_to_mac_keycode(0x90), Some(71));
    }

    #[test]
    fn windows_wire_helpers_keep_vk_values() {
        assert_eq!(windows_vk_to_wire(0x41), Some(0x41));
        assert_eq!(windows_vk_to_wire(u32::from(u16::MAX) + 1), None);
        assert_eq!(wire_to_windows_vk(0x7A), 0x7A);
    }
}
