use std::time::Duration;

use hop_protocol::datagram::MouseButton;

const LEFT_COMMAND_KEYCODE: u16 = 55;
const RIGHT_COMMAND_KEYCODE: u16 = 54;
const LEFT_SHIFT_KEYCODE: u16 = 56;
const RIGHT_SHIFT_KEYCODE: u16 = 60;
const LEFT_OPTION_KEYCODE: u16 = 58;
const RIGHT_OPTION_KEYCODE: u16 = 61;
const LEFT_CONTROL_KEYCODE: u16 = 59;
const RIGHT_CONTROL_KEYCODE: u16 = 62;
const CAPS_LOCK_KEYCODE: u16 = 57;

const DOUBLE_CLICK_MAX_INTERVAL: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_MAX_DISTANCE_PX: f64 = 6.0;
const MAX_CLICK_STATE: i64 = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ModifierSnapshot {
    pub command: bool,
    pub shift: bool,
    pub alternate: bool,
    pub control: bool,
    pub alpha_shift: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ModifierFlagsState {
    key_states: [bool; 128],
}

impl Default for ModifierFlagsState {
    fn default() -> Self {
        Self {
            key_states: [false; 128],
        }
    }
}

impl ModifierFlagsState {
    pub(crate) fn apply_key_event(&mut self, mac_keycode: u16, pressed: bool) -> ModifierSnapshot {
        if is_modifier_keycode(mac_keycode) {
            if let Some(state) = self.key_states.get_mut(usize::from(mac_keycode)) {
                *state = pressed;
            }
        }
        self.snapshot()
    }

    pub(crate) fn snapshot(&self) -> ModifierSnapshot {
        ModifierSnapshot {
            command: self.key_is_down(LEFT_COMMAND_KEYCODE)
                || self.key_is_down(RIGHT_COMMAND_KEYCODE),
            shift: self.key_is_down(LEFT_SHIFT_KEYCODE) || self.key_is_down(RIGHT_SHIFT_KEYCODE),
            alternate: self.key_is_down(LEFT_OPTION_KEYCODE)
                || self.key_is_down(RIGHT_OPTION_KEYCODE),
            control: self.key_is_down(LEFT_CONTROL_KEYCODE)
                || self.key_is_down(RIGHT_CONTROL_KEYCODE),
            alpha_shift: self.key_is_down(CAPS_LOCK_KEYCODE),
        }
    }

    fn key_is_down(&self, mac_keycode: u16) -> bool {
        self.key_states
            .get(usize::from(mac_keycode))
            .copied()
            .unwrap_or(false)
    }
}

fn is_modifier_keycode(mac_keycode: u16) -> bool {
    matches!(
        mac_keycode,
        LEFT_COMMAND_KEYCODE
            | RIGHT_COMMAND_KEYCODE
            | LEFT_SHIFT_KEYCODE
            | RIGHT_SHIFT_KEYCODE
            | LEFT_OPTION_KEYCODE
            | RIGHT_OPTION_KEYCODE
            | LEFT_CONTROL_KEYCODE
            | RIGHT_CONTROL_KEYCODE
            | CAPS_LOCK_KEYCODE
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct InputPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ButtonClickState {
    last_down_at: Option<Duration>,
    last_down_point: Option<InputPoint>,
    click_state: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ClickCountTracker {
    left: ButtonClickState,
    right: ButtonClickState,
    middle: ButtonClickState,
}

impl ClickCountTracker {
    pub(crate) fn click_state_for_event(
        &mut self,
        button: MouseButton,
        pressed: bool,
        point: InputPoint,
        now: Duration,
    ) -> i64 {
        match button {
            MouseButton::Left => update_click_state(&mut self.left, pressed, point, now),
            MouseButton::Right => update_click_state(&mut self.right, pressed, point, now),
            MouseButton::Middle => update_click_state(&mut self.middle, pressed, point, now),
            MouseButton::X1 | MouseButton::X2 => 1,
        }
    }
}

fn update_click_state(
    state: &mut ButtonClickState,
    pressed: bool,
    point: InputPoint,
    now: Duration,
) -> i64 {
    if pressed {
        let is_continued_click = state
            .last_down_at
            .zip(state.last_down_point)
            .map(|(last_at, last_point)| {
                let in_time = now >= last_at && now - last_at <= DOUBLE_CLICK_MAX_INTERVAL;
                in_time && is_within_double_click_distance(last_point, point)
            })
            .unwrap_or(false);

        state.click_state = if is_continued_click {
            (state.click_state + 1).min(MAX_CLICK_STATE)
        } else {
            1
        };
        state.last_down_at = Some(now);
        state.last_down_point = Some(point);
    } else if state.click_state == 0 {
        state.click_state = 1;
    }

    state.click_state
}

fn is_within_double_click_distance(from: InputPoint, to: InputPoint) -> bool {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let max_distance_sq = DOUBLE_CLICK_MAX_DISTANCE_PX * DOUBLE_CLICK_MAX_DISTANCE_PX;
    (dx * dx) + (dy * dy) <= max_distance_sq
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hop_protocol::datagram::MouseButton;

    use crate::platform::keycodes::wire_to_mac_keycode_for_injection;

    use super::{ClickCountTracker, InputPoint, ModifierFlagsState};

    #[test]
    fn modifier_snapshot_keeps_command_active_until_all_command_keys_released() {
        let mut state = ModifierFlagsState::default();
        assert!(!state.snapshot().command);

        assert!(state.apply_key_event(55, true).command);
        assert!(state.apply_key_event(54, true).command);
        assert!(state.apply_key_event(55, false).command);
        assert!(!state.apply_key_event(54, false).command);
    }

    #[test]
    fn non_modifier_keys_do_not_change_modifier_snapshot() {
        let mut state = ModifierFlagsState::default();
        let baseline = state.apply_key_event(58, true);
        assert!(baseline.alternate);

        let after_letter_key = state.apply_key_event(0, true);
        assert_eq!(after_letter_key, baseline);
    }

    #[test]
    fn swapped_ctrl_v_sequence_sets_command_flag_for_letter_event() {
        let mut state = ModifierFlagsState::default();
        let swapped_ctrl = wire_to_mac_keycode_for_injection(0xA2, true).expect("ctrl mapping");
        let v_key = wire_to_mac_keycode_for_injection(0x56, true).expect("v mapping");

        let ctrl_down = state.apply_key_event(swapped_ctrl, true);
        assert!(ctrl_down.command);
        assert!(!ctrl_down.control);

        let v_down = state.apply_key_event(v_key, true);
        assert!(v_down.command);
        assert!(!v_down.control);
    }

    #[test]
    fn click_tracker_reports_double_click_for_fast_nearby_presses() {
        let mut tracker = ClickCountTracker::default();
        let point = InputPoint { x: 100.0, y: 100.0 };

        assert_eq!(
            tracker.click_state_for_event(MouseButton::Left, true, point, Duration::from_millis(0)),
            1
        );
        assert_eq!(
            tracker.click_state_for_event(
                MouseButton::Left,
                false,
                point,
                Duration::from_millis(10)
            ),
            1
        );
        assert_eq!(
            tracker.click_state_for_event(
                MouseButton::Left,
                true,
                InputPoint { x: 103.0, y: 102.0 },
                Duration::from_millis(220),
            ),
            2
        );
        assert_eq!(
            tracker.click_state_for_event(
                MouseButton::Left,
                false,
                InputPoint { x: 103.0, y: 102.0 },
                Duration::from_millis(230),
            ),
            2
        );
    }

    #[test]
    fn click_tracker_resets_when_clicks_are_too_slow_or_far_apart() {
        let mut tracker = ClickCountTracker::default();
        let first = InputPoint { x: 40.0, y: 40.0 };
        let far = InputPoint { x: 80.0, y: 80.0 };

        assert_eq!(
            tracker.click_state_for_event(
                MouseButton::Right,
                true,
                first,
                Duration::from_millis(0)
            ),
            1
        );
        assert_eq!(
            tracker.click_state_for_event(
                MouseButton::Right,
                true,
                first,
                Duration::from_millis(900),
            ),
            1
        );
        assert_eq!(
            tracker.click_state_for_event(
                MouseButton::Right,
                true,
                far,
                Duration::from_millis(980),
            ),
            1
        );
    }

    #[test]
    fn x_buttons_keep_single_click_state() {
        let mut tracker = ClickCountTracker::default();
        let point = InputPoint { x: 10.0, y: 10.0 };
        assert_eq!(
            tracker.click_state_for_event(MouseButton::X1, true, point, Duration::from_millis(0)),
            1
        );
        assert_eq!(
            tracker.click_state_for_event(MouseButton::X2, true, point, Duration::from_millis(1)),
            1
        );
    }
}
