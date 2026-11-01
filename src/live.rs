//! A plain-data mirror of the parts of the PipeWire graph we care about.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveNode {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub media_class: String,
    /// Set for nodes that belong to a physical device (ALSA, Bluetooth, ...).
    pub device_id: Option<u32>,
    /// For app streams: `application.process.binary` and `application.name`.
    #[serde(default)]
    pub app_binary: String,
    #[serde(default)]
    pub app_name: String,
    #[serde(default)]
    pub client_id: Option<u32>,
    /// A level meter (pavucontrol, the volume applet): never routed.
    #[serde(default)]
    pub meter: bool,
}

impl LiveNode {
    pub fn is_audio_endpoint(&self) -> bool {
        matches!(
            self.media_class.as_str(),
            "Audio/Sink" | "Audio/Source" | "Audio/Source/Virtual" | "Audio/Duplex"
        )
    }
    pub fn is_hardware(&self) -> bool {
        self.device_id.is_some() && self.is_audio_endpoint()
    }
    pub fn is_sink(&self) -> bool {
        self.media_class == "Audio/Sink" || self.media_class == "Audio/Duplex"
    }
    pub fn is_source(&self) -> bool {
        self.media_class.starts_with("Audio/Source") || self.media_class == "Audio/Duplex"
    }
    /// An audio stream belonging to a program (playback or recording).
    pub fn is_app_stream(&self) -> bool {
        matches!(self.media_class.as_str(), "Stream/Output/Audio" | "Stream/Input/Audio")
            && !self.app_binary.is_empty()
            && !self.meter
    }
    pub fn is_playback_stream(&self) -> bool {
        self.media_class == "Stream/Output/Audio"
    }
    pub fn label(&self) -> &str {
        if self.description.is_empty() { &self.name } else { &self.description }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    In,
    Out,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LivePort {
    pub id: u32,
    pub node_id: u32,
    pub name: String,
    pub dir: Dir,
    pub channel: String,
    /// Index of the port within its node (`port.id`), used for stable ordering.
    pub index: u32,
    /// A monitor port: a copy of what the node receives. On a recording stream that's the
    /// microphone itself, so it must never be routed as the program's output.
    #[serde(default)]
    pub monitor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveLink {
    pub id: u32,
    pub out_port: u32,
    pub in_port: u32,
    pub out_node: u32,
    pub in_node: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Live {
    pub nodes: BTreeMap<u32, LiveNode>,
    pub ports: BTreeMap<u32, LivePort>,
    pub links: BTreeMap<u32, LiveLink>,
    /// Client id -> executable name (`application.process.binary`), which identifies the
    /// program behind each stream.
    #[serde(default)]
    pub clients: BTreeMap<u32, String>,
}

impl Live {
    pub fn node_by_name(&self, name: &str) -> Option<&LiveNode> {
        self.nodes.values().find(|n| n.name == name)
    }

    /// Ports of a node in one direction, sorted by their index within the node.
    pub fn ports_of(&self, node_id: u32, dir: Dir) -> Vec<&LivePort> {
        let mut v: Vec<&LivePort> = self
            .ports
            .values()
            .filter(|p| p.node_id == node_id && p.dir == dir)
            .filter(|p| !(p.monitor && self.nodes.get(&node_id).is_some_and(|n| n.media_class.starts_with("Stream/"))))
            .collect();
        v.sort_by_key(|p| (p.index, p.id));
        v
    }

    pub fn audio_endpoints(&self) -> impl Iterator<Item = &LiveNode> {
        self.nodes.values().filter(|n| n.is_audio_endpoint())
    }
}
