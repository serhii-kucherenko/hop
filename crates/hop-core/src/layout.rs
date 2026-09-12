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
