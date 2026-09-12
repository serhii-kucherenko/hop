use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    Hello {
        machine_name: String,
        role: NodeRole,
        udp_port: u16,
        screen: ScreenSize,
    },
    Ack {
        accepted: bool,
        reason: Option<String>,
    },
    HandoffStart {
        from_machine: String,
        to_machine: String,
        edge: Edge,
    },
    HandoffStartAck {
        from_machine: String,
        to_machine: String,
        accepted: bool,
        reason: Option<String>,
    },
    HandoffEnd {
        owner_machine: String,
    },
    Ping {
        at_millis: u64,
    },
    Pong {
        at_millis: u64,
    },
}
