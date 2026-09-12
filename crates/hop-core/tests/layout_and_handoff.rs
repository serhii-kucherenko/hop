use hop_core::handoff::{FocusState, HandoffAction, HandoffController};
use hop_core::layout::{
    detect_edge_crossing, edge_for_peer_position, invert_relative_position, CursorPosition,
    RelativePosition, ScreenBounds, SpatialLayout, SpatialNeighbor,
};
use hop_protocol::control::Edge;
use hop_protocol::datagram::InputEvent;

fn two_machine_layout() -> SpatialLayout {
    SpatialLayout::new(vec![SpatialNeighbor {
        machine_name: "windows-box".to_owned(),
        position: RelativePosition::Right,
    }])
}

#[test]
fn edge_detection_matches_screen_boundaries() {
    let screen = ScreenBounds::from_size(1920, 1080);

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
    let screen = ScreenBounds::from_size(1920, 1080);
    let layout = two_machine_layout();
    let mut controller = HandoffController::new("macbook-pro");

    let action =
        controller.on_local_cursor(CursorPosition { x: 1921, y: 400 }, screen, &layout, &[]);
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
    let screen = ScreenBounds::from_size(1920, 1080);
    let layout = two_machine_layout();
    let mut controller = HandoffController::new("macbook-pro");
    let _ = controller.on_local_cursor(CursorPosition { x: 1925, y: 540 }, screen, &layout, &[]);

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
fn force_local_resets_remote_focus_state() {
    let screen = ScreenBounds::from_size(1920, 1080);
    let layout = two_machine_layout();
    let mut controller = HandoffController::new("macbook-pro");
    let _ = controller.on_local_cursor(CursorPosition { x: 1925, y: 540 }, screen, &layout, &[]);

    controller.force_local();

    assert_eq!(controller.focus_state(), &FocusState::Local);
    let action =
        controller.on_local_cursor(CursorPosition { x: 1925, y: 540 }, screen, &layout, &[]);
    assert_eq!(
        action,
        HandoffAction::Begin {
            target_machine: "windows-box".to_owned(),
            edge: Edge::Right
        }
    );
}

#[test]
fn no_handoff_when_crossing_without_neighbor() {
    let layout = SpatialLayout::new(vec![SpatialNeighbor {
        machine_name: "windows-box".to_owned(),
        position: RelativePosition::Left,
    }]);
    let screen = ScreenBounds::from_size(1920, 1080);
    let mut controller = HandoffController::new("macbook-pro");
    let action =
        controller.on_local_cursor(CursorPosition { x: 1921, y: 20 }, screen, &layout, &[]);
    assert_eq!(action, HandoffAction::None);
    assert_eq!(controller.focus_state(), &FocusState::Local);
}

#[test]
fn handoff_begins_when_pushing_into_clamped_right_edge() {
    let layout = two_machine_layout();
    let screen = ScreenBounds::from_size(1920, 1080);
    let mut controller = HandoffController::new("macbook-pro");
    let edge_cursor = CursorPosition { x: 1919, y: 320 };

    let first_push = [InputEvent::MouseMove { dx: 2, dy: 0 }];
    let action = controller.on_local_cursor(edge_cursor, screen, &layout, &first_push);
    assert_eq!(action, HandoffAction::None);

    let second_push = [InputEvent::MouseMove { dx: 3, dy: 0 }];
    let action = controller.on_local_cursor(edge_cursor, screen, &layout, &second_push);
    assert_eq!(
        action,
        HandoffAction::Begin {
            target_machine: "windows-box".to_owned(),
            edge: Edge::Right
        }
    );
}

#[test]
fn handoff_requires_outbound_motion_on_clamped_edge() {
    let layout = two_machine_layout();
    let screen = ScreenBounds::from_size(1920, 1080);
    let mut controller = HandoffController::new("macbook-pro");
    let edge_cursor = CursorPosition { x: 1919, y: 320 };

    for _ in 0..3 {
        let action = controller.on_local_cursor(
            edge_cursor,
            screen,
            &layout,
            &[InputEvent::MouseMove { dx: -2, dy: 0 }],
        );
        assert_eq!(action, HandoffAction::None);
    }

    assert_eq!(controller.focus_state(), &FocusState::Local);
}

#[test]
fn sticky_handoff_supports_left_top_and_bottom_edges() {
    let screen = ScreenBounds::from_size(1920, 1080);
    let cases = [
        (
            RelativePosition::Left,
            CursorPosition { x: 0, y: 200 },
            InputEvent::MouseMove { dx: -2, dy: 0 },
            Edge::Left,
        ),
        (
            RelativePosition::Above,
            CursorPosition { x: 200, y: 0 },
            InputEvent::MouseMove { dx: 0, dy: -2 },
            Edge::Top,
        ),
        (
            RelativePosition::Below,
            CursorPosition { x: 200, y: 1079 },
            InputEvent::MouseMove { dx: 0, dy: 2 },
            Edge::Bottom,
        ),
    ];

    for (position, cursor, push, expected_edge) in cases {
        let layout = SpatialLayout::new(vec![SpatialNeighbor {
            machine_name: "windows-box".to_owned(),
            position,
        }]);
        let mut controller = HandoffController::new("macbook-pro");
        let action =
            controller.on_local_cursor(cursor, screen, &layout, std::slice::from_ref(&push));
        assert_eq!(action, HandoffAction::None);

        let action = controller.on_local_cursor(cursor, screen, &layout, &[push]);
        assert_eq!(
            action,
            HandoffAction::Begin {
                target_machine: "windows-box".to_owned(),
                edge: expected_edge,
            }
        );
    }
}

#[test]
fn return_edge_matches_peer_position() {
    assert_eq!(edge_for_peer_position(RelativePosition::Left), Edge::Left);
    assert_eq!(edge_for_peer_position(RelativePosition::Right), Edge::Right);
    assert_eq!(edge_for_peer_position(RelativePosition::Above), Edge::Top);
    assert_eq!(
        edge_for_peer_position(RelativePosition::Below),
        Edge::Bottom
    );
}

#[test]
fn invert_layout_position_round_trips() {
    for position in [
        RelativePosition::Left,
        RelativePosition::Right,
        RelativePosition::Above,
        RelativePosition::Below,
    ] {
        assert_eq!(
            invert_relative_position(invert_relative_position(position)),
            position
        );
    }
}

#[test]
fn return_edge_matrix_matches_inverted_server_layout() {
    let matrix = [
        (RelativePosition::Right, Edge::Right, Edge::Left),
        (RelativePosition::Left, Edge::Left, Edge::Right),
        (RelativePosition::Above, Edge::Top, Edge::Bottom),
        (RelativePosition::Below, Edge::Bottom, Edge::Top),
    ];

    for (server_position, expected_enter_edge, expected_return_edge) in matrix {
        assert_eq!(edge_for_peer_position(server_position), expected_enter_edge);
        let client_view_of_server = invert_relative_position(server_position);
        assert_eq!(
            edge_for_peer_position(client_view_of_server),
            expected_return_edge
        );
    }
}

#[test]
fn second_enter_is_noop_while_already_remote_for_all_positions() {
    let screen = ScreenBounds::from_size(1920, 1080);
    let scenarios = [
        (
            RelativePosition::Right,
            CursorPosition { x: 1921, y: 540 },
            Edge::Right,
        ),
        (
            RelativePosition::Left,
            CursorPosition { x: -1, y: 540 },
            Edge::Left,
        ),
        (
            RelativePosition::Above,
            CursorPosition { x: 960, y: -1 },
            Edge::Top,
        ),
        (
            RelativePosition::Below,
            CursorPosition { x: 960, y: 1080 },
            Edge::Bottom,
        ),
    ];

    for (position, crossing_cursor, expected_edge) in scenarios {
        let layout = SpatialLayout::new(vec![SpatialNeighbor {
            machine_name: "peer-box".to_owned(),
            position,
        }]);
        let mut controller = HandoffController::new("local-box");
        let begin = controller.on_local_cursor(crossing_cursor, screen, &layout, &[]);
        assert_eq!(
            begin,
            HandoffAction::Begin {
                target_machine: "peer-box".to_owned(),
                edge: expected_edge,
            }
        );

        let second = controller.on_local_cursor(crossing_cursor, screen, &layout, &[]);
        assert_eq!(second, HandoffAction::None);
        assert_eq!(
            controller.focus_state(),
            &FocusState::Remote {
                target_machine: "peer-box".to_owned(),
                edge: expected_edge,
            }
        );
    }
}

#[test]
fn edge_detection_supports_virtual_desktop_origins() {
    let screen = ScreenBounds {
        origin_x: -1920,
        origin_y: -120,
        width: 3840,
        height: 2280,
    };

    assert_eq!(
        detect_edge_crossing(
            CursorPosition {
                x: screen.origin_x - 1,
                y: 400
            },
            screen
        ),
        Some(Edge::Left)
    );
    assert_eq!(
        detect_edge_crossing(
            CursorPosition {
                x: screen.max_x() + 1,
                y: 400
            },
            screen
        ),
        Some(Edge::Right)
    );
    assert_eq!(
        detect_edge_crossing(
            CursorPosition {
                x: 100,
                y: screen.origin_y - 1
            },
            screen
        ),
        Some(Edge::Top)
    );
    assert_eq!(
        detect_edge_crossing(
            CursorPosition {
                x: 100,
                y: screen.max_y() + 1
            },
            screen
        ),
        Some(Edge::Bottom)
    );
}
