//! Persistent data model: one routing graph of nodes and wires, stored as TOML.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A reference to a PipeWire node by its stable `node.name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRef {
    /// Stable identifier, e.g. `alsa_output.usb-Astro_Gaming_Astro_A50-00.stereo-chat`.
    pub node_name: String,
    /// Human-readable name remembered for display while the device is unplugged.
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BusKind {
    /// A virtual speaker (Audio/Sink). Apps play into it; its output is what they played.
    #[default]
    Playback,
    /// A virtual microphone (Audio/Source). Feed it audio; apps can record from it.
    Microphone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Channels {
    Mono,
    #[default]
    Stereo,
}


#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusSpec {
    /// PipeWire `node.name`.
    pub name: String,
    #[serde(default)]
    pub kind: BusKind,
    #[serde(default)]
    pub channels: Channels,
}

impl BusSpec {
    pub fn media_class(&self) -> &'static str {
        match self.kind {
            BusKind::Playback => "Audio/Sink",
            BusKind::Microphone => "Audio/Source/Virtual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BandKind {
    #[default]
    Bell,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    Notch,
}

impl BandKind {
    pub const ALL: [BandKind; 6] =
        [BandKind::Bell, BandKind::LowShelf, BandKind::HighShelf, BandKind::LowPass, BandKind::HighPass, BandKind::Notch];
    pub fn label(self) -> &'static str {
        match self {
            BandKind::Bell => "Bell",
            BandKind::LowShelf => "Low shelf",
            BandKind::HighShelf => "High shelf",
            BandKind::LowPass => "Low pass",
            BandKind::HighPass => "High pass",
            BandKind::Notch => "Notch",
        }
    }
    /// Whether the gain setting does anything for this filter type.
    pub fn has_gain(self) -> bool {
        matches!(self, BandKind::Bell | BandKind::LowShelf | BandKind::HighShelf)
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Band {
    #[serde(default)]
    pub kind: BandKind,
    /// Hz
    pub freq: f32,
    #[serde(default)]
    pub gain_db: f32,
    pub q: f32,
    #[serde(default = "yes")]
    pub enabled: bool,
}

pub const MAX_BANDS: usize = 32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqSpec {
    /// PipeWire `node.name` of the filter.
    pub name: String,
    #[serde(default)]
    pub channels: Channels,
    /// Gain applied before the bands, to make room for boosts.
    #[serde(default)]
    pub preamp_db: f32,
    #[serde(default)]
    pub bands: Vec<Band>,
}

impl EqSpec {
    pub fn new(name: &str, channels: Channels) -> Self {
        EqSpec { name: name.into(), channels, preamp_db: 0.0, bands: vec![] }
    }
}

/// A level meter with a noise gate (bottom limit) and a limiter (top limit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelsSpec {
    pub name: String,
    #[serde(default)]
    pub channels: Channels,
    /// Below this the signal is muted (noise gate).
    pub gate_on: bool,
    pub gate_db: f32,
    /// The signal never goes above this (limiter).
    pub limit_on: bool,
    pub limit_db: f32,
}

impl LevelsSpec {
    pub fn new(name: &str) -> Self {
        LevelsSpec { name: name.into(), channels: Channels::Stereo, gate_on: false, gate_db: -50.0, limit_on: true, limit_db: -1.0 }
    }
}

/// A volume control: multiplies the signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeSpec {
    pub name: String,
    #[serde(default)]
    pub channels: Channels,
    /// Linear factor: 1.0 leaves the signal unchanged, 0.5 halves it, 2.0 doubles it.
    pub gain: f32,
}

/// Highest volume you can type in (1000%).
pub const MAX_VOLUME: f32 = 10.0;

/// One audio-processing node that runs inside the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterSpec {
    Eq(EqSpec),
    Levels(LevelsSpec),
    Volume(VolumeSpec),
}

impl FilterSpec {
    pub fn name(&self) -> &str {
        match self {
            FilterSpec::Eq(e) => &e.name,
            FilterSpec::Levels(l) => &l.name,
            FilterSpec::Volume(v) => &v.name,
        }
    }
    pub fn channels(&self) -> Channels {
        match self {
            FilterSpec::Eq(e) => e.channels,
            FilterSpec::Levels(l) => l.channels,
            FilterSpec::Volume(v) => v.channels,
        }
    }
    /// Same kind and layout: can be retuned in place instead of rebuilt.
    pub fn compatible(&self, other: &FilterSpec) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
            && self.name() == other.name()
            && self.channels() == other.channels()
    }
}

/// One choice on a switch node.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SwitchPin {
    /// Optional name shown next to the pin.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Keyboard shortcut that selects this pin, e.g. `Ctrl+Alt+1`. Pins on different switches
    /// with the same key are selected together.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NodeKind {
    /// A specific capture device (or any audio source).
    Input { device: Option<DeviceRef> },
    /// A specific playback device (or any audio sink).
    Output { device: Option<DeviceRef> },
    /// A virtual device owned by the graph.
    Bus(BusSpec),
    /// A native equalizer: audio goes in, gets EQ'd, comes out.
    Eq(EqSpec),
    /// One program's audio. Its out is what it plays; its in is what it records.
    App { app: Option<AppRef> },
    /// Every program without its own App node: what they play comes out here.
    /// Wiring it to a device makes that device the system default output.
    OtherAppsPlayback,
    /// Every program without its own App node records from what's wired in here.
    /// Wiring a device to it makes that device the system default input.
    OtherAppsRecording,
    /// Sends its input to the one output you selected.
    SwitchOutput { name: String, pins: Vec<SwitchPin> },
    /// Passes on the one input you selected.
    SwitchInput { name: String, pins: Vec<SwitchPin> },
    /// Passes audio through, shows its level, and can gate and limit it.
    Levels(LevelsSpec),
    /// Passes audio through at a volume you set.
    Volume(VolumeSpec),
}

/// A program, matched by its executable name (stable across restarts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRef {
    /// `application.process.binary`, e.g. `DiscordCanary`.
    pub binary: String,
    /// `application.name` as last seen, for display.
    #[serde(default)]
    pub name: String,
}

impl NodeKind {
    pub fn inputs(&self) -> usize {
        match self {
            NodeKind::Output { .. } | NodeKind::Bus(_) | NodeKind::Eq(_) | NodeKind::App { .. } => 1,
            NodeKind::OtherAppsRecording | NodeKind::SwitchOutput { .. } => 1,
            NodeKind::Levels(_) | NodeKind::Volume(_) => 1,
            NodeKind::SwitchInput { pins, .. } => pins.len(),
            NodeKind::Input { .. } | NodeKind::OtherAppsPlayback => 0,
        }
    }
    pub fn outputs(&self) -> usize {
        match self {
            NodeKind::Input { .. } | NodeKind::Bus(_) | NodeKind::Eq(_) | NodeKind::App { .. } => 1,
            NodeKind::OtherAppsPlayback | NodeKind::SwitchInput { .. } => 1,
            NodeKind::Levels(_) | NodeKind::Volume(_) => 1,
            NodeKind::SwitchOutput { pins, .. } => pins.len(),
            NodeKind::Output { .. } | NodeKind::OtherAppsRecording => 0,
        }
    }
    /// Routing-only nodes that stand for no PipeWire object of their own.
    pub fn is_switch(&self) -> bool {
        matches!(self, NodeKind::SwitchOutput { .. } | NodeKind::SwitchInput { .. })
    }
    pub fn switch(&self) -> Option<(&str, &[SwitchPin])> {
        match self {
            NodeKind::SwitchOutput { name, pins } | NodeKind::SwitchInput { name, pins } => Some((name, pins)),
            _ => None,
        }
    }
    /// The processing node this stands for, if it is one.
    pub fn filter(&self) -> Option<FilterSpec> {
        match self {
            NodeKind::Eq(e) => Some(FilterSpec::Eq(e.clone())),
            NodeKind::Levels(l) => Some(FilterSpec::Levels(l.clone())),
            NodeKind::Volume(v) => Some(FilterSpec::Volume(v.clone())),
            _ => None,
        }
    }
    /// The name that must be unique in the graph: owned PipeWire names and switch names.
    pub fn unique_name_mut(&mut self) -> Option<&mut String> {
        match self {
            NodeKind::Bus(b) => Some(&mut b.name),
            NodeKind::Eq(e) => Some(&mut e.name),
            NodeKind::Levels(l) => Some(&mut l.name),
            NodeKind::Volume(v) => Some(&mut v.name),
            NodeKind::SwitchOutput { name, .. } | NodeKind::SwitchInput { name, .. } => Some(name),
            _ => None,
        }
    }
    /// The PipeWire `node.name` this node owns (virtual devices and processing nodes).
    pub fn owned_name(&self) -> Option<&str> {
        match self {
            NodeKind::Bus(b) => Some(&b.name),
            NodeKind::Eq(e) => Some(&e.name),
            NodeKind::Levels(l) => Some(&l.name),
            NodeKind::Volume(v) => Some(&v.name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: u32,
    pub pos: [f32; 2],
    #[serde(flatten)]
    pub kind: NodeKind,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// Audio flows from node `from` to node `to`. Channels are matched automatically.
/// Pins only matter for switch nodes, which have numbered pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Wire {
    pub from: u32,
    pub to: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub from_pin: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub to_pin: u32,
}

impl Wire {
    pub fn new(from: u32, to: u32) -> Wire {
        Wire { from, to, from_pin: 0, to_pin: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default, rename = "node")]
    pub nodes: Vec<GraphNode>,
    #[serde(default, rename = "wire")]
    pub wires: Vec<Wire>,
}


/// Runtime choices that aren't part of the graph's design: which pin each switch uses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    /// Switch name -> selected pin (0-based). Switches not listed use their first pin.
    #[serde(default)]
    pub selected: Selections,
}

pub type Selections = std::collections::BTreeMap<String, u32>;

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("better-sound-patching")
}

pub fn graph_path() -> PathBuf {
    config_dir().join("graph.toml")
}

pub fn graph_exists() -> bool {
    graph_path().exists()
}

pub fn load_graph() -> Result<Graph> {
    let path = graph_path();
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_graph(graph: &Graph) -> Result<()> {
    std::fs::create_dir_all(config_dir())?;
    write_atomic(&graph_path(), &toml::to_string_pretty(graph)?)
}

/// Names used by two nodes that each need a unique one (devices, processing nodes, switches).
pub fn duplicate_names(g: &Graph) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut dupes = vec![];
    for n in &g.nodes {
        let name = match &n.kind {
            k if k.owned_name().is_some() => k.owned_name().unwrap(),
            NodeKind::SwitchOutput { name, .. } | NodeKind::SwitchInput { name, .. } => name,
            _ => continue,
        };
        if !seen.insert(name.to_owned()) {
            dupes.push(name.to_owned());
        }
    }
    dupes
}

pub fn load_state() -> AppState {
    std::fs::read_to_string(config_dir().join("state.toml"))
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_state(state: &AppState) -> Result<()> {
    std::fs::create_dir_all(config_dir())?;
    write_atomic(&config_dir().join("state.toml"), &toml::to_string_pretty(state)?)
}

/// Your own names for devices, keyed by `node.name`. Shared by the graph and the mixer.
pub type Labels = std::collections::BTreeMap<String, String>;

pub fn load_labels() -> Labels {
    std::fs::read_to_string(config_dir().join("labels.toml"))
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_labels(labels: &Labels) -> Result<()> {
    std::fs::create_dir_all(config_dir())?;
    write_atomic(&config_dir().join("labels.toml"), &toml::to_string_pretty(labels)?)
}

pub fn write_atomic(path: &std::path::Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_roundtrips_through_toml() {
        let p = Graph {
            nodes: vec![
                GraphNode {
                    id: 1,
                    pos: [0.0, 0.0],
                    kind: NodeKind::Bus(BusSpec {
                        name: "system-in".into(),
                        kind: BusKind::Playback,
                        channels: Channels::Stereo,
                    }),
                },
                GraphNode {
                    id: 2,
                    pos: [100.0, 0.0],
                    kind: NodeKind::Output {
                        device: Some(DeviceRef {
                            node_name: "alsa_output.x".into(),
                            description: "X".into(),
                        }),
                    },
                },
                GraphNode { id: 3, pos: [0.0, 50.0], kind: NodeKind::OtherAppsPlayback },
                GraphNode {
                    id: 5,
                    pos: [0.0, 90.0],
                    kind: NodeKind::SwitchOutput {
                        name: "Output".into(),
                        pins: vec![SwitchPin { label: "Headset".into(), key: "Ctrl+Alt+1".into() }, SwitchPin::default()],
                    },
                },
                GraphNode { id: 7, pos: [0.0, 0.0], kind: NodeKind::Levels(LevelsSpec::new("levels-1")) },
                GraphNode {
                    id: 8,
                    pos: [0.0, 0.0],
                    kind: NodeKind::Volume(VolumeSpec { name: "volume-1".into(), channels: Channels::Mono, gain: 0.5 }),
                },
                GraphNode {
                    id: 6,
                    pos: [0.0, 90.0],
                    kind: NodeKind::App { app: Some(AppRef { binary: "DiscordCanary".into(), name: "Discord".into() }) },
                },
                GraphNode {
                    id: 4,
                    pos: [50.0, 50.0],
                    kind: NodeKind::Eq(EqSpec {
                        name: "eq-mic".into(),
                        channels: Channels::Mono,
                        preamp_db: -3.0,
                        bands: vec![Band { kind: BandKind::Bell, freq: 143.3, gain_db: 14.5, q: 4.0, enabled: true }],
                    }),
                },
            ],
            wires: vec![Wire::new(1, 2), Wire { from: 5, to: 2, from_pin: 1, to_pin: 0 }],
        };
        let text = toml::to_string_pretty(&p).unwrap();
        let back: Graph = toml::from_str(&text).unwrap();
        assert_eq!(p, back);
    }
}
