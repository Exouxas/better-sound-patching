//! Persistent data model: profiles made of nodes and wires, stored as TOML.

use anyhow::{Context, Result, bail};
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
    /// PipeWire `node.name`. Buses with the same name in different profiles are the same bus,
    /// which is what lets profile switches happen without apps noticing.
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
    /// PipeWire `node.name` of the filter. Same name in two profiles = same filter, so switching
    /// profiles only retunes it instead of rebuilding it.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NodeKind {
    /// A specific capture device (or any audio source).
    Input { device: Option<DeviceRef> },
    /// A specific playback device (or any audio sink).
    Output { device: Option<DeviceRef> },
    /// A virtual device owned by this profile.
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
    /// Sends audio to the first of its numbered outputs whose device is plugged in.
    FirstOutput { slots: u32 },
    /// Passes on the first of its numbered inputs whose device is plugged in.
    FirstInput { slots: u32 },
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
            NodeKind::OtherAppsRecording | NodeKind::FirstOutput { .. } => 1,
            NodeKind::FirstInput { slots } => *slots as usize,
            NodeKind::Input { .. } | NodeKind::OtherAppsPlayback => 0,
        }
    }
    pub fn outputs(&self) -> usize {
        match self {
            NodeKind::Input { .. } | NodeKind::Bus(_) | NodeKind::Eq(_) | NodeKind::App { .. } => 1,
            NodeKind::OtherAppsPlayback | NodeKind::FirstInput { .. } => 1,
            NodeKind::FirstOutput { slots } => *slots as usize,
            NodeKind::Output { .. } | NodeKind::OtherAppsRecording => 0,
        }
    }
    /// Routing-only nodes that stand for no PipeWire object of their own.
    pub fn is_switch(&self) -> bool {
        matches!(self, NodeKind::FirstOutput { .. } | NodeKind::FirstInput { .. })
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
/// Pins only matter for "first available" nodes, which have numbered pins.
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
pub struct Profile {
    #[serde(default, rename = "node")]
    pub nodes: Vec<GraphNode>,
    #[serde(default, rename = "wire")]
    pub wires: Vec<Wire>,
}


#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    pub active_profile: Option<String>,
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("better-sound-patching")
}

fn profiles_dir() -> PathBuf {
    config_dir().join("profiles")
}

pub fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        && !name.starts_with('.')
}

fn profile_path(name: &str) -> Result<PathBuf> {
    if !valid_profile_name(name) {
        bail!("invalid profile name {name:?} (use letters, digits, space, - _ .)");
    }
    Ok(profiles_dir().join(format!("{name}.toml")))
}

pub fn list_profiles() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(profiles_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "toml")
                .then(|| p.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

pub fn load_profile(name: &str) -> Result<Profile> {
    let path = profile_path(name)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_profile(name: &str, profile: &Profile) -> Result<()> {
    let path = profile_path(name)?;
    std::fs::create_dir_all(profiles_dir())?;
    write_atomic(&path, &toml::to_string_pretty(profile)?)
}

pub fn delete_profile(name: &str) -> Result<()> {
    Ok(std::fs::remove_file(profile_path(name)?)?)
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

/// Your own names for devices, keyed by `node.name`. Shared by all profiles and the mixer.
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
    fn profile_roundtrips_through_toml() {
        let p = Profile {
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
                GraphNode { id: 5, pos: [0.0, 90.0], kind: NodeKind::FirstOutput { slots: 3 } },
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
        let back: Profile = toml::from_str(&text).unwrap();
        assert_eq!(p, back);
    }
}
