//! The node editor: conversion between profiles and the snarl graph, and the node viewer.

use crate::live::{Live, LiveNode};
use crate::model::{BusKind, BusSpec, Channels, DeviceRef, GraphNode, Labels, NodeKind, Profile, Wire};
use egui::{Color32, RichText, Ui};
use egui_snarl::ui::{PinInfo, SnarlPin, SnarlViewer};
use egui_snarl::{InPin, InPinId, NodeId, OutPin, OutPinId, Snarl};
use std::collections::BTreeMap;

const GREEN: Color32 = Color32::from_rgb(80, 200, 120);
const GREY: Color32 = Color32::from_gray(110);
const AMBER: Color32 = Color32::from_rgb(230, 170, 60);

pub fn to_snarl(profile: &Profile) -> Snarl<NodeKind> {
    let mut snarl = Snarl::new();
    let mut ids = BTreeMap::new();
    for n in &profile.nodes {
        let id = snarl.insert_node(egui::pos2(n.pos[0], n.pos[1]), n.kind.clone());
        ids.insert(n.id, id);
    }
    for w in &profile.wires {
        if let (Some(&a), Some(&b)) = (ids.get(&w.from), ids.get(&w.to)) {
            snarl.connect(OutPinId { node: a, output: 0 }, InPinId { node: b, input: 0 });
        }
    }
    snarl
}

pub fn from_snarl(snarl: &Snarl<NodeKind>) -> Profile {
    let mut nodes: Vec<GraphNode> = snarl
        .nodes_pos_ids()
        .map(|(id, pos, kind)| GraphNode { id: id.0 as u32, pos: [pos.x.round(), pos.y.round()], kind: kind.clone() })
        .collect();
    nodes.sort_by_key(|n| n.id);
    let mut wires: Vec<Wire> = snarl
        .wires()
        .map(|(o, i)| Wire { from: o.node.0 as u32, to: i.node.0 as u32 })
        .collect();
    wires.sort();
    wires.dedup();
    Profile { nodes, wires }
}

pub struct Viewer<'a> {
    pub live: &'a Live,
    /// Graph node id -> live node ids it currently stands for.
    pub resolved: &'a BTreeMap<u32, Vec<u32>>,
    /// Your names for devices (node.name -> label), edited in place.
    pub labels: &'a mut Labels,
    pub labels_changed: bool,
    pub changed: bool,
}

/// The name to show for a device: your label, else its description.
pub fn display_name(labels: &Labels, node_name: &str, description: &str) -> String {
    labels
        .get(node_name)
        .filter(|l| !l.is_empty())
        .cloned()
        .unwrap_or_else(|| if description.is_empty() { node_name.to_owned() } else { description.to_owned() })
}

impl Viewer<'_> {
    fn label_editor(&mut self, ui: &mut Ui, node_name: &str, placeholder: &str) {
        let mut text = self.labels.get(node_name).cloned().unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("Label");
            let r = ui.add(egui::TextEdit::singleline(&mut text).hint_text(placeholder).desired_width(170.0));
            if r.changed() {
                let text = text.trim();
                if text.is_empty() {
                    self.labels.remove(node_name);
                } else {
                    self.labels.insert(node_name.to_owned(), text.to_owned());
                }
                self.labels_changed = true;
            }
        });
    }

    fn present(&self, node: NodeId) -> bool {
        self.resolved.get(&(node.0 as u32)).is_some_and(|v| !v.is_empty())
    }

    fn pin(&self, node: NodeId) -> PinInfo {
        let color = if self.present(node) { GREEN } else { GREY };
        PinInfo::circle().with_fill(color).with_wire_color(color)
    }

    fn device_picker(&mut self, ui: &mut Ui, id: NodeId, device: &mut Option<DeviceRef>, want_source: bool) {
        let mut candidates: Vec<&LiveNode> = self
            .live
            .audio_endpoints()
            .filter(|n| if want_source { n.is_source() } else { n.is_sink() })
            .collect();
        candidates.sort_by(|a, b| (!a.is_hardware(), a.label()).cmp(&(!b.is_hardware(), b.label())));

        let selected = device
            .as_ref()
            .map(|d| display_name(self.labels, &d.node_name, &d.description))
            .unwrap_or_else(|| "Choose a device…".into());
        egui::ComboBox::from_id_salt(("device", id))
            .selected_text(selected)
            .width(230.0)
            .show_ui(ui, |ui| {
                for n in candidates {
                    let is_sel = device.as_ref().is_some_and(|d| d.node_name == n.name);
                    let name = display_name(self.labels, &n.name, n.label());
                    let text = if n.is_hardware() { name } else { format!("{name} (virtual)") };
                    let hover = format!("{}\n{}", n.label(), n.name);
                    if ui.selectable_label(is_sel, text).on_hover_text(hover).clicked() && !is_sel {
                        *device = Some(DeviceRef { node_name: n.name.clone(), description: n.label().to_owned() });
                        self.changed = true;
                    }
                }
            });
        if let Some(d) = device.as_ref() {
            let (node_name, description) = (d.node_name.clone(), d.description.clone());
            self.label_editor(ui, &node_name, &description);
            if self.present(id) {
                ui.label(RichText::new("✔ connected").color(GREEN).small());
            } else {
                ui.label(RichText::new("⚠ not plugged in — waiting for it").color(AMBER).small());
            }
        }
    }
}

impl SnarlViewer<NodeKind> for Viewer<'_> {
    fn title(&mut self, node: &NodeKind) -> String {
        let named = |kind: &str, d: &Option<DeviceRef>| match d {
            Some(d) => format!("{kind} · {}", display_name(self.labels, &d.node_name, &d.description)),
            None => format!("{kind} device"),
        };
        match node {
            NodeKind::Input { device } => named("🎤 Input", device),
            NodeKind::Output { device } => named("🔊 Output", device),
            NodeKind::Bus(b) => {
                let kind = match b.kind {
                    BusKind::Playback => "Virtual output",
                    BusKind::Microphone => "Virtual mic",
                };
                match self.labels.get(&b.name).filter(|l| !l.is_empty()) {
                    Some(l) => format!("{kind} · {l}"),
                    None => kind.into(),
                }
            }
            NodeKind::FallbackInput => "Fallback input".into(),
            NodeKind::FallbackOutput => "Fallback output".into(),
        }
    }

    fn inputs(&mut self, node: &NodeKind) -> usize {
        node.has_input() as usize
    }

    fn outputs(&mut self, node: &NodeKind) -> usize {
        node.has_output() as usize
    }

    fn show_input(&mut self, pin: &InPin, ui: &mut Ui, _snarl: &mut Snarl<NodeKind>) -> impl SnarlPin + 'static {
        ui.label("in");
        self.pin(pin.id.node)
    }

    fn show_output(&mut self, pin: &OutPin, ui: &mut Ui, _snarl: &mut Snarl<NodeKind>) -> impl SnarlPin + 'static {
        ui.label("out");
        self.pin(pin.id.node)
    }

    fn has_body(&mut self, _node: &NodeKind) -> bool {
        true
    }

    fn show_body(&mut self, node: NodeId, _i: &[InPin], _o: &[OutPin], ui: &mut Ui, snarl: &mut Snarl<NodeKind>) {
        ui.vertical(|ui| {
            ui.set_max_width(260.0);
            match &mut snarl[node] {
                NodeKind::Input { device } => self.device_picker(ui, node, device, true),
                NodeKind::Output { device } => self.device_picker(ui, node, device, false),
                NodeKind::Bus(b) => {
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        let r = ui.add(egui::TextEdit::singleline(&mut b.name).desired_width(150.0));
                        self.changed |= r.changed();
                    });
                    let bus_name = b.name.clone();
                    self.label_editor(ui, &bus_name, "optional");
                    ui.horizontal(|ui| {
                        for (c, label) in [(Channels::Mono, "Mono"), (Channels::Stereo, "Stereo")] {
                            if ui.selectable_label(b.channels == c, label).clicked() && b.channels != c {
                                b.channels = c;
                                self.changed = true;
                            }
                        }
                    });
                    let hint = match b.kind {
                        BusKind::Playback => "Apps play into this; its out carries what they played.",
                        BusKind::Microphone => "Feed its in; apps can record from it.",
                    };
                    ui.label(RichText::new(hint).small().weak());
                    if self.present(node) {
                        ui.label(RichText::new("✔ exists").color(GREEN).small());
                    } else {
                        ui.label(RichText::new("created when the profile is active").color(GREY).small());
                    }
                }
                NodeKind::FallbackInput | NodeKind::FallbackOutput => {
                    let ids = self.resolved.get(&(node.0 as u32)).cloned().unwrap_or_default();
                    ui.label(RichText::new("Every device without its own node:").small().weak());
                    if ids.is_empty() {
                        ui.label(RichText::new("(none right now)").small().weak());
                    }
                    for id in ids {
                        if let Some(n) = self.live.nodes.get(&id) {
                            let name = display_name(self.labels, &n.name, n.label());
                            ui.label(RichText::new(format!("• {name}")).small());
                        }
                    }
                }
            }
        });
    }

    fn has_graph_menu(&mut self, _pos: egui::Pos2, _snarl: &mut Snarl<NodeKind>) -> bool {
        true
    }

    fn show_graph_menu(&mut self, pos: egui::Pos2, ui: &mut Ui, snarl: &mut Snarl<NodeKind>) {
        ui.label(RichText::new("Add node").strong());
        let mut add = None;
        if ui.button("Input device").clicked() {
            add = Some(NodeKind::Input { device: None });
        }
        if ui.button("Output device").clicked() {
            add = Some(NodeKind::Output { device: None });
        }
        if ui.button("Virtual output (apps play into it)").clicked() {
            add = Some(NodeKind::Bus(BusSpec { name: "new-output".into(), kind: BusKind::Playback, channels: Channels::Stereo }));
        }
        if ui.button("Virtual mic (apps record from it)").clicked() {
            add = Some(NodeKind::Bus(BusSpec { name: "new-mic".into(), kind: BusKind::Microphone, channels: Channels::Mono }));
        }
        ui.separator();
        if ui.button("Fallback input").clicked() {
            add = Some(NodeKind::FallbackInput);
        }
        if ui.button("Fallback output").clicked() {
            add = Some(NodeKind::FallbackOutput);
        }
        if let Some(kind) = add {
            snarl.insert_node(pos, kind);
            ui.close();
        }
    }

    fn has_node_menu(&mut self, _node: &NodeKind) -> bool {
        true
    }

    fn show_node_menu(&mut self, node: NodeId, _i: &[InPin], _o: &[OutPin], ui: &mut Ui, snarl: &mut Snarl<NodeKind>) {
        if ui.button("Remove").clicked() {
            snarl.remove_node(node);
            ui.close();
        }
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<NodeKind>) {
        if from.id.node != to.id.node {
            snarl.connect(from.id, to.id);
        }
    }
}
