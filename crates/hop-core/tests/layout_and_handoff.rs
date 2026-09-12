use hop_core::handoff::{FocusState, HandoffAction, HandoffController};
use hop_core::layout::{
    detect_edge_crossing, CursorPosition, RelativePosition, ScreenSize, SpatialLayout,
    SpatialNeighbor,
};
use hop_protocol::control::Edge;

fn two_machine_layout() -> SpatialLayout {
    SpatialLayout::new(vec![SpatialNeighbor {
        machine_name: "windows-box".to_owned(),
        position: RelativePosition::Right,
    }])
}

#[test]
fn edge_detection_matches_screen_boundaries() {
    let screen = ScreenSize {
        width: 1920,
        height: 1080,
    };

    assert_eq!(
        detect_edge_crossing(CursorPosition { x: -1, y: 200 }, screen),
        Some(Edge::Left)
    );
    assert_eq!(
        detect_edge_crossing(CursorPosition { x: 1920, y: 200 }, screen),
        Some(Edge::Right)
    );
    assert_eq!(
        detect_edge_crossing(CursorPosition { x: 100, y: -1 }, screen),
        Some(Edge::Top)
    );
    assert_eq!(
        detect_edge_crossing(CursorPosition { x: 100, y: 1080 }, screen),
        Some(Edge::Bottom)
    );
    assert_eq!(
        detect_edge_crossing(CursorPosition { x: 500, y: 500 }, screen),
        None
    );
}

#[test]
fn handoff_begins_when_crossing_edge_with_neighbor() {
    let screen = ScreenSize {
        width: 1920,
        height: 1080,
    };
    let layout = two_machine_layout();
    let mut controller = HandoffController::new("macbook-pro");

    let action = controller.on_local_cursor(CursorPosition { x: 1921, y: 400 }, screen, &layout);
    assert_eq!(
        action,
        HandoffAction::Begin {
            target_machine: "windows-box".to_owned(),
            edge: Edge::Right
        }
    );
    assert_eq!(
        controller.focus_state(),
        &FocusState::Remote {
            target_machine: "windows-box".to_owned(),
            edge: Edge::Right
        }
    );
}

#[test]
fn handoff_release_returns_focus_locally() {
    let screen = ScreenSize {
        width: 1920,
        height: 1080,
    };
    let layout = two_machine_layout();
    let mut controller = HandoffController::new("macbook-pro");
    let _ = controller.on_local_cursor(CursorPosition { x: 1925, y: 540 }, screen, &layout);

    let release = controller.on_remote_release();
    assert_eq!(
        release,
        HandoffAction::End {
            owner_machine: "macbook-pro".to_owned()
        }
    );
    assert_eq!(controller.focus_state(), &FocusState::Local);
}

#[test]
fn no_handoff_when_crossing_without_neighbor() {
    let layout = SpatialLayout::new(vec![SpatialNeighbor {
        machine_name: "windows-box".to_owned(),
        position: RelativePosition::Left,
    }]);
    let screen = ScreenSize {
        width: 1920,
        height: 1080,
    };
    let mut controller = HandoffController::new("macbook-pro");
    let action = controller.on_local_cursor(CursorPosition { x: 1921, y: 20 }, screen, &layout);
    assert_eq!(action, HandoffAction::None);
    assert_eq!(controller.focus_state(), &FocusState::Local);
}
