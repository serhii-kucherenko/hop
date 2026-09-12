use hop_protocol::control::Edge;

use crate::layout::{detect_edge_crossing, CursorPosition, ScreenSize, SpatialLayout};

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
}

impl HandoffController {
    pub fn new(owner_machine: impl Into<String>) -> Self {
        Self {
            owner_machine: owner_machine.into(),
            focus_state: FocusState::Local,
        }
    }

    pub fn focus_state(&self) -> &FocusState {
        &self.focus_state
    }

    pub fn on_local_cursor(
        &mut self,
        cursor: CursorPosition,
        screen: ScreenSize,
        layout: &SpatialLayout,
    ) -> HandoffAction {
        if !matches!(self.focus_state, FocusState::Local) {
            return HandoffAction::None;
        }

        let Some(edge) = detect_edge_crossing(cursor, screen) else {
            return HandoffAction::None;
        };

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

    pub fn on_remote_release(&mut self) -> HandoffAction {
        if matches!(self.focus_state, FocusState::Remote { .. }) {
            self.focus_state = FocusState::Local;
            return HandoffAction::End {
                owner_machine: self.owner_machine.clone(),
            };
        }
        HandoffAction::None
    }
}
