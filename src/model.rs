//! Persistent data model: one routing graph of nodes and wires, stored as TOML.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// The name programs and sound settings show (`node.description`); the node name if empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl BusSpec {
    pub fn new(name: &str, kind: BusKind, channels: Channels) -> BusSpec {
        BusSpec { name: name.into(), kind, channels, description: String::new() }
    }
    /// The name shown to people.
    pub fn display(&self) -> &str {
        if self.description.is_empty() { &self.name } else { &self.description }
    }
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

/// Which neural network removes the noise. Both run on the CPU inside the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DenoiseModel {
    /// DeepFilterNet 3: 40 ms of delay. Can sound muffled with headset mics that only record
    /// up to 8 kHz (16 kHz sample rate), as its model expects full-band audio.
    DeepFilterNet,
    /// RNNoise 0.2 (the system's librnnoise): light, 20 ms of delay, keeps the voice natural.
    #[default]
    RnNoise,
    /// DPDFNet-2 (Ceva): strongest on mixed noise, but about 80 ms of delay and a fifth of a CPU
    /// core, on its own thread.
    DpdfNet,
}

impl DenoiseModel {
    pub const ALL: [DenoiseModel; 3] = [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet];
    pub fn label(self) -> &'static str {
        match self {
            DenoiseModel::DeepFilterNet => "DeepFilterNet 3",
            DenoiseModel::RnNoise => "RNNoise 0.2",
            DenoiseModel::DpdfNet => "DPDFNet 2",
        }
    }
    /// Delay from input to output, in milliseconds (model plus 10 ms frames).
    pub fn delay_ms(self) -> u32 {
        match self {
            DenoiseModel::DeepFilterNet => 40,
            DenoiseModel::RnNoise => 20,
            // With PipeWire buffers up to 480 samples; larger ones add more.
            DenoiseModel::DpdfNet => 80,
        }
    }
}

/// At or above this reduction the noise isn't limited at all: it's removed as far as the model can.
pub const DENOISE_FULL_DB: f32 = 100.0;

fn mono() -> Channels {
    Channels::Mono
}

fn full_reduction() -> f32 {
    DENOISE_FULL_DB
}

/// Noise suppression: removes background noise from speech.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DenoiseSpec {
    pub name: String,
    #[serde(default = "mono")]
    pub channels: Channels,
    #[serde(default)]
    pub model: DenoiseModel,
    /// The most the noise is turned down, in dB. Lower values sound more natural but leave
    /// some noise; [`DENOISE_FULL_DB`] and above don't limit it.
    #[serde(default = "full_reduction")]
    pub reduction_db: f32,
    /// Off passes the audio unchanged (still delayed the same, so nothing shifts in time).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Voice gate: silence between words.
    #[serde(default)]
    pub gate: bool,
    /// Audio less likely than this to be speech is silenced (0–1).
    #[serde(default = "gate_threshold")]
    pub gate_threshold: f32,
    /// How long the gate stays open after speech, in ms.
    #[serde(default = "gate_hold_ms")]
    pub gate_hold_ms: f32,
}

fn gate_threshold() -> f32 {
    0.7
}

fn gate_hold_ms() -> f32 {
    200.0
}

impl DenoiseSpec {
    pub fn new(name: &str) -> Self {
        DenoiseSpec {
            name: name.into(),
            channels: Channels::Mono,
            model: DenoiseModel::default(),
            reduction_db: DENOISE_FULL_DB,
            enabled: true,
            gate: false,
            gate_threshold: gate_threshold(),
            gate_hold_ms: gate_hold_ms(),
        }
    }
}

/// One audio-processing node that runs inside the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterSpec {
    Eq(EqSpec),
    Levels(LevelsSpec),
    Volume(VolumeSpec),
    Denoise(DenoiseSpec),
}

impl FilterSpec {
    pub fn name(&self) -> &str {
        match self {
            FilterSpec::Eq(e) => &e.name,
            FilterSpec::Levels(l) => &l.name,
            FilterSpec::Volume(v) => &v.name,
            FilterSpec::Denoise(d) => &d.name,
        }
    }
    pub fn channels(&self) -> Channels {
        match self {
            FilterSpec::Eq(e) => e.channels,
            FilterSpec::Levels(l) => l.channels,
            FilterSpec::Volume(v) => v.channels,
            FilterSpec::Denoise(d) => d.channels,
        }
    }
    /// Same kind and layout: can be retuned in place instead of rebuilt.
    pub fn compatible(&self, other: &FilterSpec) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
            && self.name() == other.name()
            && self.channels() == other.channels()
            && self.denoise_model() == other.denoise_model()
    }
    /// A different model needs a new node: its state can't carry over.
    fn denoise_model(&self) -> Option<DenoiseModel> {
        match self {
            FilterSpec::Denoise(d) => Some(d.model),
            _ => None,
        }
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
    /// Removes background noise from speech.
    Denoise(DenoiseSpec),
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
            NodeKind::Levels(_) | NodeKind::Volume(_) | NodeKind::Denoise(_) => 1,
            NodeKind::SwitchInput { pins, .. } => pins.len(),
            NodeKind::Input { .. } | NodeKind::OtherAppsPlayback => 0,
        }
    }
    pub fn outputs(&self) -> usize {
        match self {
            NodeKind::Input { .. } | NodeKind::Bus(_) | NodeKind::Eq(_) | NodeKind::App { .. } => 1,
            NodeKind::OtherAppsPlayback | NodeKind::SwitchInput { .. } => 1,
            NodeKind::Levels(_) | NodeKind::Volume(_) | NodeKind::Denoise(_) => 1,
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
            NodeKind::Denoise(d) => Some(FilterSpec::Denoise(d.clone())),
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
            NodeKind::Denoise(d) => Some(&mut d.name),
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
            NodeKind::Denoise(d) => Some(&d.name),
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

/// How long audio takes to get somewhere, in ms, over every path it takes there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delay {
    pub min: u32,
    pub max: u32,
    /// The same sound arrives more than once, at different delays: an echo, or a hollow,
    /// phasey sound when the delays are short.
    pub doubled: bool,
}

/// The delay of each node: what it adds itself, and how late audio arrives at its input and
/// leaves its output, counted from where it entered the graph (a device, a program or a
/// virtual output programs play into). `None` where no audio gets to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeDelay {
    pub own: u32,
    pub arriving: Option<Delay>,
    pub leaving: Option<Delay>,
}

impl NodeKind {
    /// Delay this node adds itself, in ms. PipeWire's own buffering isn't counted.
    pub fn delay_ms(&self) -> u32 {
        match self {
            NodeKind::Denoise(d) => d.model.delay_ms(),
            _ => 0,
        }
    }
}

/// Earliest and latest arrival of each source (by node id) at one point.
type Arrivals = BTreeMap<u32, (u32, u32)>;

fn merge(into: &mut Arrivals, from: &Arrivals, add: u32) {
    for (&src, &(lo, hi)) in from {
        let e = into.entry(src).or_insert((u32::MAX, 0));
        e.0 = e.0.min(lo + add);
        e.1 = e.1.max(hi + add);
    }
}

fn summary(a: &Arrivals) -> Option<Delay> {
    (!a.is_empty()).then(|| Delay {
        min: a.values().map(|v| v.0).min().unwrap(),
        max: a.values().map(|v| v.1).max().unwrap(),
        doubled: a.values().any(|v| v.0 != v.1),
    })
}

/// The delay at every node of `graph`, following only the pin each switch is using
/// (`active`: switch node id -> pin). Loops are cut where they close.
pub fn delays(graph: &Graph, active: &BTreeMap<u32, Option<u32>>) -> BTreeMap<u32, NodeDelay> {
    struct Walk<'a> {
        kinds: BTreeMap<u32, &'a NodeKind>,
        wires: &'a [Wire],
        active: &'a BTreeMap<u32, Option<u32>>,
        leaving: BTreeMap<u32, Arrivals>,
        busy: std::collections::BTreeSet<u32>,
    }
    impl Walk<'_> {
        fn passes(&self, w: &Wire) -> bool {
            let on = |id: u32, pin: u32| self.active.get(&id).copied().flatten() == Some(pin);
            let from_ok = !matches!(self.kinds.get(&w.from), Some(NodeKind::SwitchOutput { .. })) || on(w.from, w.from_pin);
            let to_ok = !matches!(self.kinds.get(&w.to), Some(NodeKind::SwitchInput { .. })) || on(w.to, w.to_pin);
            from_ok && to_ok
        }
        fn arriving(&mut self, id: u32) -> Arrivals {
            let mut a = Arrivals::new();
            for w in self.wires.iter().filter(|w| w.to == id) {
                if self.passes(w) {
                    let from = self.leave(w.from);
                    merge(&mut a, &from, 0);
                }
            }
            a
        }
        fn leave(&mut self, id: u32) -> Arrivals {
            if let Some(a) = self.leaving.get(&id) {
                return a.clone();
            }
            let Some(kind) = self.kinds.get(&id).copied() else { return Arrivals::new() };
            // A loop: what comes round again is already counted where it started.
            if !self.busy.insert(id) {
                return Arrivals::new();
            }
            let a = match kind {
                // What a program plays has nothing to do with what it records.
                NodeKind::App { .. } | NodeKind::Input { .. } | NodeKind::OtherAppsPlayback => Arrivals::from([(id, (0, 0))]),
                NodeKind::Output { .. } | NodeKind::OtherAppsRecording => Arrivals::new(),
                NodeKind::Bus(b) => {
                    let mut a = self.arriving(id);
                    if b.kind == BusKind::Playback {
                        // Programs play into it directly.
                        a.insert(id, (0, 0));
                    }
                    a
                }
                k => {
                    let mut a = Arrivals::new();
                    merge(&mut a, &self.arriving(id), k.delay_ms());
                    a
                }
            };
            self.busy.remove(&id);
            self.leaving.insert(id, a.clone());
            a
        }
    }
    let mut walk = Walk {
        kinds: graph.nodes.iter().map(|n| (n.id, &n.kind)).collect(),
        wires: &graph.wires,
        active,
        leaving: BTreeMap::new(),
        busy: Default::default(),
    };
    graph
        .nodes
        .iter()
        .map(|n| {
            let leaving = summary(&walk.leave(n.id));
            let arriving = summary(&walk.arriving(n.id));
            (n.id, NodeDelay { own: n.kind.delay_ms(), arriving, leaving })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default, rename = "node")]
    pub nodes: Vec<GraphNode>,
    #[serde(default, rename = "wire")]
    pub wires: Vec<Wire>,
}


/// Runtime choices that aren't part of the graph's design: which pin each switch uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    /// Switch name -> selected pin (0-based). Switches not listed use their first pin.
    #[serde(default)]
    pub selected: Selections,
    /// Master volume: a gain on everything leaving the virtual outputs (0..=1).
    #[serde(default = "full")]
    pub master: f32,
}

fn full() -> f32 {
    1.0
}

impl Default for AppState {
    fn default() -> Self {
        AppState { selected: Selections::default(), master: 1.0 }
    }
}

pub type Selections = std::collections::BTreeMap<String, u32>;

/// The same switch choices, for when `from` (e.g. unsaved edits that moved pins around) gives
/// way to `to` (e.g. the saved graph): each switch keeps using the same pin, found by its
/// label and key, wherever that pin sits in `to`. Returns only the switches whose number changes.
pub fn follow_selection(from: &Graph, to: &Graph, selected: &Selections) -> Vec<(String, u32)> {
    let mut out = vec![];
    for n in &from.nodes {
        let Some((name, pins)) = n.kind.switch() else { continue };
        let s = selected.get(name).copied().unwrap_or(0);
        let Some(pin) = pins.get(s as usize) else { continue };
        let Some((_, to_pins)) = to.nodes.iter().filter_map(|m| m.kind.switch()).find(|(m, _)| *m == name) else { continue };
        let same: Vec<u32> = (0..to_pins.len() as u32).filter(|&j| to_pins[j as usize] == *pin).collect();
        let Some(&j) = same.iter().find(|&&j| j == s).or(same.first()) else { continue };
        if j != s {
            out.push((name.to_owned(), j));
        }
    }
    out
}

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

/// Unsaved changes from the editor, kept between sessions until you Save or Discard them.
fn draft_path() -> PathBuf {
    config_dir().join("draft.toml")
}

pub fn load_draft() -> Option<Graph> {
    toml::from_str(&std::fs::read_to_string(draft_path()).ok()?).ok()
}

pub fn save_draft(graph: &Graph) -> Result<()> {
    std::fs::create_dir_all(config_dir())?;
    write_atomic(&draft_path(), &toml::to_string_pretty(graph)?)
}

pub fn delete_draft() {
    let _ = std::fs::remove_file(draft_path());
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

    fn node(id: u32, kind: NodeKind) -> GraphNode {
        GraphNode { id, pos: [0.0, 0.0], kind }
    }

    fn denoise(name: &str, model: DenoiseModel) -> NodeKind {
        NodeKind::Denoise(DenoiseSpec { model, ..DenoiseSpec::new(name) })
    }

    #[test]
    fn delays_add_up_along_a_chain() {
        // mic -> DeepFilterNet -> RNNoise -> virtual mic
        let g = Graph {
            nodes: vec![
                node(1, NodeKind::Input { device: None }),
                node(2, denoise("a", DenoiseModel::DeepFilterNet)),
                node(3, denoise("b", DenoiseModel::RnNoise)),
                node(4, NodeKind::Bus(BusSpec::new("mic", BusKind::Microphone, Channels::Mono))),
            ],
            wires: vec![Wire::new(1, 2), Wire::new(2, 3), Wire::new(3, 4)],
        };
        let d = delays(&g, &BTreeMap::new());
        assert_eq!(d[&2].own, 40);
        assert_eq!(d[&3].arriving, Some(Delay { min: 40, max: 40, doubled: false }));
        assert_eq!(d[&4].leaving, Some(Delay { min: 60, max: 60, doubled: false }));
    }

    #[test]
    fn one_sound_over_two_paths_is_doubled_but_two_sounds_are_not() {
        // mic -> denoise -> out, and mic -> out directly: the same sound twice.
        let mut g = Graph {
            nodes: vec![
                node(1, NodeKind::Input { device: None }),
                node(2, denoise("a", DenoiseModel::RnNoise)),
                node(3, NodeKind::Output { device: None }),
                node(4, NodeKind::Input { device: None }),
            ],
            wires: vec![Wire::new(1, 2), Wire::new(2, 3), Wire::new(1, 3)],
        };
        assert_eq!(delays(&g, &BTreeMap::new())[&3].arriving, Some(Delay { min: 0, max: 20, doubled: true }));
        // Another mic, undelayed, alongside the denoised one: different sounds, no echo.
        g.wires = vec![Wire::new(1, 2), Wire::new(2, 3), Wire::new(4, 3)];
        assert_eq!(delays(&g, &BTreeMap::new())[&3].arriving, Some(Delay { min: 0, max: 20, doubled: false }));
    }

    #[test]
    fn delays_follow_the_switch_and_survive_loops() {
        // mic -> switch pin 0 -> denoise -> out; switch pin 1 -> out. And a loop out of reach.
        let g = Graph {
            nodes: vec![
                node(1, NodeKind::Input { device: None }),
                node(2, NodeKind::SwitchOutput { name: "s".into(), pins: vec![SwitchPin::default(); 2] }),
                node(3, denoise("a", DenoiseModel::DeepFilterNet)),
                node(4, NodeKind::Output { device: None }),
                node(5, NodeKind::Volume(VolumeSpec { name: "v1".into(), channels: Channels::Mono, gain: 1.0 })),
                node(6, NodeKind::Volume(VolumeSpec { name: "v2".into(), channels: Channels::Mono, gain: 1.0 })),
            ],
            wires: vec![
                Wire::new(1, 2),
                Wire { from: 2, to: 3, from_pin: 0, to_pin: 0 },
                Wire::new(3, 4),
                Wire { from: 2, to: 4, from_pin: 1, to_pin: 0 },
                Wire::new(5, 6),
                Wire::new(6, 5),
            ],
        };
        let at_out = |pin| delays(&g, &BTreeMap::from([(2, Some(pin))]))[&4].arriving;
        assert_eq!(at_out(0), Some(Delay { min: 40, max: 40, doubled: false }));
        assert_eq!(at_out(1), Some(Delay { min: 0, max: 0, doubled: false }));
        let d = delays(&g, &BTreeMap::new());
        assert_eq!(d[&4].arriving, None);
        assert_eq!(d[&5].leaving, None);
    }

    #[test]
    fn graph_roundtrips_through_toml() {
        let p = Graph {
            nodes: vec![
                GraphNode {
                    id: 1,
                    pos: [0.0, 0.0],
                    kind: NodeKind::Bus(BusSpec {
                        description: "Game & music".into(),
                        ..BusSpec::new("system-in", BusKind::Playback, Channels::Stereo)
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
                    id: 9,
                    pos: [0.0, 0.0],
                    kind: NodeKind::Denoise(DenoiseSpec { model: DenoiseModel::RnNoise, reduction_db: 24.0, enabled: false, ..DenoiseSpec::new("denoise-1") }),
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

    #[test]
    fn switches_keep_their_pin_when_pins_are_renumbered() {
        let switch = |labels: &[&str]| Graph {
            nodes: vec![GraphNode {
                id: 0,
                pos: [0.0, 0.0],
                kind: NodeKind::SwitchOutput {
                    name: "Output".into(),
                    pins: labels.iter().map(|l| SwitchPin { label: (*l).into(), key: String::new() }).collect(),
                },
            }],
            wires: vec![],
        };
        let saved = switch(&["Headset", "Speakers", "Index"]);
        let moved = switch(&["Speakers", "Index", "Headset"]);
        let on_headset: Selections = [("Output".to_string(), 0)].into();
        // Headset moved to 3: the switch follows it there, and back again on undo.
        assert_eq!(follow_selection(&saved, &moved, &on_headset), [("Output".to_string(), 2)]);
        let on_moved_headset: Selections = [("Output".to_string(), 2)].into();
        assert_eq!(follow_selection(&moved, &saved, &on_moved_headset), [("Output".to_string(), 0)]);
        // A renamed pin can't be followed: the number stays.
        assert!(follow_selection(&saved, &switch(&["Phones", "Speakers", "Index"]), &on_headset).is_empty());
    }
}
