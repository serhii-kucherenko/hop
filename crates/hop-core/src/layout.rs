use hop_protocol::control::Edge;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenSize {
    pub width: u32,
    pub height: u32,
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

pub fn detect_edge_crossing(position: CursorPosition, screen: ScreenSize) -> Option<Edge> {
    if position.x < 0 {
        return Some(Edge::Left);
    }
    if position.y < 0 {
        return Some(Edge::Top);
    }
    if position.x >= screen.width as i32 {
        return Some(Edge::Right);
    }
    if position.y >= screen.height as i32 {
        return Some(Edge::Bottom);
    }
    None
}
