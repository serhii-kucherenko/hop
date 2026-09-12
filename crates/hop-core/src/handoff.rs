use hop_protocol::control::Edge;
use hop_protocol::datagram::InputEvent;

use crate::layout::{detect_edge_crossing, CursorPosition, ScreenBounds, SpatialLayout};

const STICKY_OUTBOUND_SAMPLE_THRESHOLD: u8 = 2;
const STICKY_OUTBOUND_DISTANCE_THRESHOLD: i32 = 6;
const STICKY_DWELL_THRESHOLD: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusState {
    Local,
    Remote { target_machine: String, edge: Edge },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffAction {
    None,
    Begin { target_machine: String, edge: Edge },
    End { owner_machine: String },
}

#[derive(Debug, Clone)]
pub struct HandoffController {
    owner_machine: String,
    focus_state: FocusState,
    sticky_edge: Option<StickyEdgeState>,
}

impl HandoffController {
    pub fn new(owner_machine: impl Into<String>) -> Self {
        Self {
            owner_machine: owner_machine.into(),
            focus_state: FocusState::Local,
            sticky_edge: None,
        }
    }

    pub fn focus_state(&self) -> &FocusState {
        &self.focus_state
    }

    pub fn on_local_cursor(
        &mut self,
        cursor: CursorPosition,
        screen: ScreenBounds,
        layout: &SpatialLayout,
        events: &[InputEvent],
    ) -> HandoffAction {
        if !matches!(self.focus_state, FocusState::Local) {
            return HandoffAction::None;
        }

        if let Some(edge) = detect_edge_crossing(cursor, screen) {
            self.sticky_edge = None;
            return self.begin_handoff(edge, layout);
        }

        let Some(edge) = self.detect_sticky_edge(cursor, screen, layout, events) else {
            self.sticky_edge = None;
            return HandoffAction::None;
        };

        let outbound_distance = outbound_distance_for_edge(edge, events);
        let sticky_state = self
            .sticky_edge
            .get_or_insert_with(|| StickyEdgeState::new(edge));
        if sticky_state.edge != edge {
            *sticky_state = StickyEdgeState::new(edge);
        } else {
            sticky_state.dwell_samples = sticky_state.dwell_samples.saturating_add(1);
        }

        if outbound_distance > 0 {
            sticky_state.outbound_samples = sticky_state.outbound_samples.saturating_add(1);
            sticky_state.outbound_distance = sticky_state
                .outbound_distance
                .saturating_add(outbound_distance);
        }

        let sticky_triggered = sticky_state.outbound_samples >= STICKY_OUTBOUND_SAMPLE_THRESHOLD
            || sticky_state.outbound_distance >= STICKY_OUTBOUND_DISTANCE_THRESHOLD
            || (sticky_state.dwell_samples >= STICKY_DWELL_THRESHOLD && outbound_distance > 0);
        if !sticky_triggered {
            return HandoffAction::None;
        }

        self.sticky_edge = None;
        self.begin_handoff(edge, layout)
    }

    fn begin_handoff(&mut self, edge: Edge, layout: &SpatialLayout) -> HandoffAction {
        let Some(neighbor) = layout.neighbor_for_edge(edge) else {
            return HandoffAction::None;
        };
        self.focus_state = FocusState::Remote {
            target_machine: neighbor.machine_name.clone(),
            edge,
        };
        HandoffAction::Begin {
            target_machine: neighbor.machine_name.clone(),
            edge,
        }
    }

    pub fn force_local(&mut self) {
        self.focus_state = FocusState::Local;
        self.sticky_edge = None;
    }

    pub fn on_remote_release(&mut self) -> HandoffAction {
        if matches!(self.focus_state, FocusState::Remote { .. }) {
            self.force_local();
            return HandoffAction::End {
                owner_machine: self.owner_machine.clone(),
            };
        }
        HandoffAction::None
    }

    fn detect_sticky_edge(
        &self,
        cursor: CursorPosition,
        screen: ScreenBounds,
        layout: &SpatialLayout,
        events: &[InputEvent],
    ) -> Option<Edge> {
        let mut candidates = [None, None, None, None];
        if cursor.x <= 0 {
            candidates[0] = Some(Edge::Left);
        }
        if cursor.x >= max_x(screen) {
            candidates[1] = Some(Edge::Right);
        }
        if cursor.y <= 0 {
            candidates[2] = Some(Edge::Top);
        }
        if cursor.y >= max_y(screen) {
            candidates[3] = Some(Edge::Bottom);
        }

        if let Some(previous) = self.sticky_edge {
            if candidates.contains(&Some(previous.edge))
                && layout.neighbor_for_edge(previous.edge).is_some()
            {
                return Some(previous.edge);
            }
        }

        let mut best = None;
        for edge in candidates.into_iter().flatten() {
            if layout.neighbor_for_edge(edge).is_none() {
                continue;
            }
            let outbound = outbound_distance_for_edge(edge, events);
            match best {
                Some((_, best_outbound)) if best_outbound > outbound => {}
                _ => best = Some((edge, outbound)),
            }
        }
        best.map(|(edge, _)| edge)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StickyEdgeState {
    edge: Edge,
    dwell_samples: u8,
    outbound_samples: u8,
    outbound_distance: i32,
}

impl StickyEdgeState {
    fn new(edge: Edge) -> Self {
        Self {
            edge,
            dwell_samples: 0,
            outbound_samples: 0,
            outbound_distance: 0,
        }
    }
}

fn outbound_distance_for_edge(edge: Edge, events: &[InputEvent]) -> i32 {
    let mut outbound = 0_i32;
    for event in events {
        let InputEvent::MouseMove { dx, dy } = event else {
            continue;
        };
        let contribution = match edge {
            Edge::Left => i32::from((-*dx).max(0)),
            Edge::Right => i32::from((*dx).max(0)),
            Edge::Top => i32::from((-*dy).max(0)),
            Edge::Bottom => i32::from((*dy).max(0)),
        };
        outbound = outbound.saturating_add(contribution);
    }
    outbound
}

fn max_x(screen: ScreenBounds) -> i32 {
    screen.max_x()
}

fn max_y(screen: ScreenBounds) -> i32 {
    screen.max_y()
}
