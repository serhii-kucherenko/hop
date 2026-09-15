use hop_protocol::control::Edge;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenBounds {
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
}

impl ScreenBounds {
    pub const fn from_size(width: u32, height: u32) -> Self {
        Self {
            origin_x: 0,
            origin_y: 0,
            width,
            height,
        }
    }

    pub fn max_x(self) -> i32 {
        self.origin_x + self.width.saturating_sub(1) as i32
    }

    pub fn max_y(self) -> i32 {
        self.origin_y + self.height.saturating_sub(1) as i32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RelativePosition {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpatialNeighbor {
    pub machine_name: String,
    pub position: RelativePosition,
}

#[derive(Debug, Clone)]
pub struct SpatialLayout {
    neighbors: Vec<SpatialNeighbor>,
}

impl SpatialLayout {
    pub fn new(neighbors: Vec<SpatialNeighbor>) -> Self {
        Self { neighbors }
    }

    pub fn neighbor_for_edge(&self, edge: Edge) -> Option<&SpatialNeighbor> {
        let wanted_position = match edge {
            Edge::Left => RelativePosition::Left,
            Edge::Right => RelativePosition::Right,
            Edge::Top => RelativePosition::Above,
            Edge::Bottom => RelativePosition::Below,
        };
        self.neighbors
            .iter()
            .find(|neighbor| neighbor.position == wanted_position)
    }
}

pub fn edge_for_peer_position(position: RelativePosition) -> Edge {
    match position {
        RelativePosition::Left => Edge::Left,
        RelativePosition::Right => Edge::Right,
        RelativePosition::Above => Edge::Top,
        RelativePosition::Below => Edge::Bottom,
    }
}

pub fn invert_relative_position(position: RelativePosition) -> RelativePosition {
    match position {
        RelativePosition::Left => RelativePosition::Right,
        RelativePosition::Right => RelativePosition::Left,
        RelativePosition::Above => RelativePosition::Below,
        RelativePosition::Below => RelativePosition::Above,
    }
}

pub fn detect_edge_crossing(position: CursorPosition, screen: ScreenBounds) -> Option<Edge> {
    if position.x < screen.origin_x {
        return Some(Edge::Left);
    }
    if position.y < screen.origin_y {
        return Some(Edge::Top);
    }
    if position.x > screen.max_x() {
        return Some(Edge::Right);
    }
    if position.y > screen.max_y() {
        return Some(Edge::Bottom);
    }
    None
}

/// True when `cursor` sits on `edge` of the display rectangle that contains it.
/// Used for multi-monitor return so MacBook-left (x=0) fires even when a wider
/// union extends further left (e.g. Dell at origin_x=-3440).
pub fn cursor_on_containing_display_edge(
    cursor: CursorPosition,
    displays: &[ScreenBounds],
    edge: Edge,
) -> bool {
    let Some(display) = display_containing(cursor, displays) else {
        return false;
    };
    match edge {
        Edge::Left => cursor.x <= display.origin_x,
        Edge::Right => cursor.x >= display.max_x(),
        Edge::Top => cursor.y <= display.origin_y,
        Edge::Bottom => cursor.y >= display.max_y(),
    }
}

fn display_containing(cursor: CursorPosition, displays: &[ScreenBounds]) -> Option<ScreenBounds> {
    displays
        .iter()
        .copied()
        .find(|display| {
            cursor.x >= display.origin_x
                && cursor.x <= display.max_x()
                && cursor.y >= display.origin_y
                && cursor.y <= display.max_y()
        })
        .or_else(|| {
            // Clamp to nearest display by x for edge cases sitting exactly on a shared boundary.
            displays.iter().copied().min_by_key(|display| {
                let cx = display.origin_x + (display.width as i32) / 2;
                (cursor.x - cx).unsigned_abs()
            })
        })
}

#[cfg(test)]
mod containing_display_edge_tests {
    use super::*;

    #[test]
    fn macbook_left_returns_even_when_dell_extends_union_left() {
        let dell = ScreenBounds {
            origin_x: -3440,
            origin_y: -458,
            width: 3440,
            height: 1440,
        };
        let macbook = ScreenBounds {
            origin_x: 0,
            origin_y: 0,
            width: 1800,
            height: 1169,
        };
        let displays = [dell, macbook];
        // Far left of MacBook (peer-facing) must count as Left.
        assert!(cursor_on_containing_display_edge(
            CursorPosition { x: 0, y: 500 },
            &displays,
            Edge::Left
        ));
        // Interior of MacBook must not.
        assert!(!cursor_on_containing_display_edge(
            CursorPosition { x: 200, y: 500 },
            &displays,
            Edge::Left
        ));
        // Far left of Dell still counts (outer edge).
        assert!(cursor_on_containing_display_edge(
            CursorPosition { x: -3440, y: 200 },
            &displays,
            Edge::Left
        ));
    }
}
