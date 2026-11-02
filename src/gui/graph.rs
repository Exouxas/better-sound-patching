//! The node editor: conversion between the saved graph and the snarl graph, and the node viewer.

use crate::dsp::{self, Reading};
use crate::live::{Live, LiveNode};
use crate::model::{
    AppRef, Band, BandKind, BusKind, BusSpec, Channels, DeviceRef, EqSpec, Graph, GraphNode, Labels, LevelsSpec, MAX_BANDS,
    MAX_VOLUME, NodeKind, SwitchPin, VolumeSpec, Wire,
};
use egui::{Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, vec2};
use egui_snarl::ui::{PinInfo, SnarlPin, SnarlViewer};
use egui_snarl::{InPin, InPinId, NodeId, OutPin, OutPinId, Snarl};
use std::collections::BTreeMap;

const GREEN: Color32 = Color32::from_rgb(80, 200, 120);
const GREY: Color32 = Color32::from_gray(110);
const AMBER: Color32 = Color32::from_rgb(230, 170, 60);

pub fn to_snarl(profile: &Graph) -> Snarl<NodeKind> {
    let mut snarl = Snarl::new();
    let mut ids = BTreeMap::new();
    for n in &profile.nodes {
        let id = snarl.insert_node(egui::pos2(n.pos[0], n.pos[1]), n.kind.clone());
        ids.insert(n.id, id);
    }
    for w in &profile.wires {
        if let (Some(&a), Some(&b)) = (ids.get(&w.from), ids.get(&w.to)) {
            snarl.connect(
                OutPinId { node: a, output: w.from_pin as usize },
                InPinId { node: b, input: w.to_pin as usize },
            );
        }
    }
    snarl
}

pub fn from_snarl(snarl: &Snarl<NodeKind>) -> Graph {
    let mut nodes: Vec<GraphNode> = snarl
        .nodes_pos_ids()
        .map(|(id, pos, kind)| GraphNode { id: id.0 as u32, pos: [pos.x.round(), pos.y.round()], kind: kind.clone() })
        .collect();
    nodes.sort_by_key(|n| n.id);
    let mut wires: Vec<Wire> = snarl
        .wires()
        .map(|(o, i)| Wire {
            from: o.node.0 as u32,
            to: i.node.0 as u32,
            from_pin: o.output as u32,
            to_pin: i.input as u32,
        })
        .collect();
    wires.sort();
    wires.dedup();
    Graph { nodes, wires }
}

pub struct Viewer<'a> {
    pub live: &'a Live,
    /// Graph node id -> live node ids it currently stands for.
    pub resolved: &'a BTreeMap<u32, Vec<u32>>,
    /// Which pin each switch node is using right now.
    pub active_slots: &'a BTreeMap<u32, Option<u32>>,
    /// Recent signal levels of the processing nodes, by node name.
    pub readings: &'a BTreeMap<String, Reading>,
    /// Whether the new system is running (processing nodes only exist then).
    pub running: bool,
    /// Virtual devices the daemon made (and so can rename); others come from elsewhere.
    pub owned_buses: &'a [String],
    /// A switch pin the user clicked this frame: (switch name, pin).
    pub select: Option<(String, u32)>,
    /// A switch whose pin in use got a new number this frame (pins moved): show it there
    /// right away. Nothing is sent; the daemon follows the pin by itself.
    pub renumbered: Option<(String, u32)>,
    /// Set to the graph area to zoom and pan so every node fits in it (once).
    pub fit_to: Option<Rect>,
    /// Set to the graph-to-screen transform used this frame.
    pub transform: Option<egui::emath::TSTransform>,
    /// What the "Other apps" nodes made the default output / input (live ids).
    pub defaults: (Option<u32>, Option<u32>),
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

    fn colored(color: Color32) -> PinInfo {
        PinInfo::circle().with_fill(color).with_wire_color(color)
    }

    fn slot_active(&self, node: NodeId, slot: usize) -> bool {
        self.active_slots.get(&(node.0 as u32)).copied().flatten() == Some(slot as u32)
    }

    fn app_name(&self, binary: &str) -> String {
        self.labels.get(binary).filter(|l| !l.is_empty()).cloned().unwrap_or_else(|| binary.to_owned())
    }

    /// Programs currently making or recording sound, by executable.
    fn running_apps(&self) -> Vec<AppRef> {
        let mut apps: BTreeMap<String, String> = BTreeMap::new();
        for n in self.live.nodes.values().filter(|n| n.is_app_stream()) {
            apps.entry(n.app_binary.clone()).or_insert_with(|| n.app_name.clone());
        }
        apps.into_iter().map(|(binary, name)| AppRef { binary, name }).collect()
    }

    fn app_picker(&mut self, ui: &mut Ui, id: NodeId, app: &mut Option<AppRef>) {
        let selected = app.as_ref().map(|a| self.app_name(&a.binary)).unwrap_or_else(|| "Choose a program…".into());
        let running = self.running_apps();
        egui::ComboBox::from_id_salt(("app", id)).selected_text(selected).width(230.0).show_ui(ui, |ui| {
            if running.is_empty() {
                ui.label(RichText::new("No program is playing or recording right now.").weak());
            }
            for a in running {
                let is_sel = app.as_ref().is_some_and(|x| x.binary == a.binary);
                let hover = format!("{}\nstream name: {}", a.binary, a.name);
                if ui.selectable_label(is_sel, self.app_name(&a.binary)).on_hover_text(hover).clicked() && !is_sel {
                    *app = Some(a);
                    self.changed = true;
                }
            }
        });
        if let Some(a) = app.as_ref() {
            let binary = a.binary.clone();
            self.label_editor(ui, &binary, &binary);
            let n = self.resolved.get(&(id.0 as u32)).map_or(0, |v| v.len());
            if n > 0 {
                ui.label(RichText::new(format!("✔ running ({n} audio stream{})", if n == 1 { "" } else { "s" })).color(GREEN).small());
            } else {
                ui.label(RichText::new("not running — routed as soon as it starts").color(GREY).small());
            }
            ui.label(RichText::new("out = what it plays · in = what it hears as its mic").small().weak());
        }
    }

    fn apps_list(&self, ui: &mut Ui, node: NodeId) {
        let ids = self.resolved.get(&(node.0 as u32)).cloned().unwrap_or_default();
        let mut names: Vec<String> =
            ids.iter().filter_map(|id| self.live.nodes.get(id)).map(|n| self.app_name(&n.app_binary)).collect();
        names.sort();
        names.dedup();
        if names.is_empty() {
            ui.label(RichText::new("(none right now)").small().weak());
        }
        for n in names {
            ui.label(RichText::new(format!("• {n}")).small());
        }
    }

    fn default_note(&self, ui: &mut Ui, id: Option<u32>, what: &str) {
        match id.and_then(|id| self.live.nodes.get(&id)) {
            Some(n) => {
                let name = display_name(self.labels, &n.name, n.label());
                ui.label(RichText::new(format!("✔ default {what}: {name}")).color(GREEN).small());
            }
            None => {
                ui.label(RichText::new(format!("Wire to a virtual or real device; it becomes the default {what}.")).color(AMBER).small());
            }
        }
    }

    /// A switch: pick which pin is live, name the pins, and give them keyboard shortcuts.
    /// `moved` is set when pins were reordered or removed: old pin index -> new index (`None` = removed).
    fn switch_editor(&mut self, ui: &mut Ui, node: NodeId, name: &str, pins: &mut Vec<SwitchPin>, outputs: bool, moved: &mut Option<Vec<Option<usize>>>) {
        let active = self.active_slots.get(&(node.0 as u32)).copied().flatten();
        ui.label(RichText::new(if outputs { "Click a number to send audio there." } else { "Click a number to use that input." }).small().weak());
        let n = pins.len();
        let mut op = None;
        // Rows sit at animated heights so a dragged row can follow the pointer while the
        // others slide out of its way.
        let anim_id = egui::Id::new(("pin-rows", node));
        let mut anim: RowAnim = ui.data(|d| d.get_temp(anim_id)).unwrap_or_default();
        let row_h = anim.row_h.max(16.0);
        let gap = ui.spacing().item_spacing.y;
        let step = row_h + gap;
        let slot_y = |slot: usize| slot as f32 * step;
        if anim.y.len() != n {
            anim.y = (0..n).map(slot_y).collect();
            anim.drag = None;
        }
        let top = ui.cursor().top();
        let (left, width) = (ui.cursor().left(), ui.available_width());
        let pointer = ui.ctx().pointer_latest_pos().map(|p| {
            let local = ui.ctx().layer_transform_from_global(ui.layer_id()).map_or(p, |t| t * p);
            local.y - top
        });
        // Where the dragged row is now, and the slot it would drop into.
        let held = anim.drag.and_then(|(from, grab)| {
            let y = (pointer? - grab).clamp(0.0, slot_y(n - 1));
            Some((from, y, ((y / step).round() as usize).min(n - 1)))
        });
        let dt = ui.input(|i| i.stable_dt).min(0.1);
        let ease = 1.0 - (-dt * 18.0).exp();
        let mut moving = false;
        for k in 0..n {
            let target = match held {
                Some((from, y, _)) if k == from => y,
                Some((from, _, to)) => {
                    // Its place in the list with the dragged row taken out and put back at `to`.
                    let without = if k > from { k - 1 } else { k };
                    slot_y(if without >= to { without + 1 } else { without })
                }
                None => slot_y(k),
            };
            if held.is_some_and(|(from, ..)| from == k) {
                anim.y[k] = target;
            } else {
                anim.y[k] += (target - anim.y[k]) * ease;
                if (target - anim.y[k]).abs() < 0.3 {
                    anim.y[k] = target;
                } else {
                    moving = true;
                }
            }
        }
        // Draw the dragged row last, so it is on top.
        let mut order: Vec<usize> = (0..n).collect();
        if let Some((from, ..)) = held {
            order.retain(|&k| k != from);
            order.push(from);
        }
        let mut still_held = false;
        let mut tallest: f32 = 0.0;
        for k in order {
            let rect = egui::Rect::from_min_size(egui::pos2(left, top + anim.y[k]), vec2(width, row_h));
            let mut row = ui.new_child(egui::UiBuilder::new().id_salt(("pin-row", k)).max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
            let lifted = held.is_some_and(|(from, ..)| from == k);
            if lifted {
                let bg = rect.expand2(vec2(3.0, 1.0));
                row.painter().rect(bg, 4.0, Color32::from_gray(48), egui::Stroke::new(1.0, Color32::from_gray(80)), egui::StrokeKind::Outside);
            }
            let handle = drag_handle(&mut row, egui::Id::new(("pin-handle", node, k)), n > 1);
            if handle.drag_started()
                && let Some(p) = pointer
            {
                anim.drag = Some((k, p - anim.y[k]));
            }
            if lifted {
                if handle.dragged() {
                    still_held = true;
                } else if let Some((from, _, to)) = held
                    && to != from
                {
                    op = Some(PinOp::Move(from, to));
                }
            }
            let on = active == Some(k as u32);
            let text = RichText::new(format!(" {} ", k + 1)).strong().color(if on { Color32::BLACK } else { GREY });
            let b = egui::Button::new(text).fill(if on { GREEN } else { Color32::from_gray(40) });
            if row.add(b).on_hover_text("Use this pin").clicked() && !on {
                self.select = Some((name.to_owned(), k as u32));
            }
            let pin = &mut pins[k];
            self.changed |= row.add(egui::TextEdit::singleline(&mut pin.label).hint_text("label").desired_width(80.0)).changed();
            self.changed |= key_field(&mut row, (node, k), &mut pin.key);
            if row.add_enabled(n > 1, egui::Button::new("×").small()).on_hover_text("Remove this pin (and its wires)").clicked() {
                op = Some(PinOp::Remove(k));
            }
            tallest = tallest.max(row.min_rect().height());
        }
        if held.is_some() && !still_held {
            anim.drag = None;
        }
        if held.is_some() || moving {
            ui.ctx().request_repaint();
        }
        anim.row_h = tallest.max(16.0);
        ui.allocate_rect(egui::Rect::from_min_size(egui::pos2(left, top), vec2(width, slot_y(n) - gap)), egui::Sense::hover());
        if n < 8 && ui.small_button("+ pin").clicked() {
            pins.push(SwitchPin::default());
            self.changed = true;
        }
        if let Some(op) = op {
            // The daemon keeps the switch on the same pin at its new number when the change
            // reaches it (see `model::follow_selection`).
            let map = op.apply(pins);
            if let Some(Some(now)) = active.and_then(|a| map.get(a as usize).copied())
                && Some(now as u32) != active
            {
                self.renumbered = Some((name.to_owned(), now as u32));
            }
            // Rows start from where they are on screen and glide to their new places.
            let mut y = vec![0.0; pins.len()];
            for (old, new) in map.iter().enumerate() {
                if let Some(new) = *new {
                    y[new] = anim.y[old];
                }
            }
            anim.y = y;
            anim.drag = None;
            ui.ctx().request_repaint();
            *moved = Some(map);
            self.changed = true;
        }
        ui.data_mut(|d| d.insert_temp(anim_id, anim));
        ui.label(RichText::new("Pins on different switches with the same key switch together.").small().weak());
    }

    fn reading(&self, name: &str) -> Option<Reading> {
        self.readings.get(name).copied()
    }

    fn not_running_note(&self, ui: &mut Ui) {
        if !self.running {
            ui.label(RichText::new("runs when the new system is active").color(GREY).small());
        }
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

/// A change to a switch's list of pins.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PinOp {
    /// Take the pin at `.0` out and put it back so it ends up at `.1`.
    Move(usize, usize),
    Remove(usize),
}

impl PinOp {
    /// Change `pins`; returns where each old pin went (`None` = removed).
    fn apply<T>(self, pins: &mut Vec<T>) -> Vec<Option<usize>> {
        let mut order: Vec<usize> = (0..pins.len()).collect();
        let mut taken: Vec<Option<T>> = pins.drain(..).map(Some).collect();
        match self {
            PinOp::Move(from, to) => {
                let k = order.remove(from);
                order.insert(to.min(order.len()), k);
            }
            PinOp::Remove(k) => {
                order.remove(k);
            }
        }
        let mut map = vec![None; taken.len()];
        for (new, &old) in order.iter().enumerate() {
            map[old] = Some(new);
            pins.push(taken[old].take().unwrap());
        }
        map
    }
}

/// Where a switch's pin rows are drawn (relative to the first row), for animating reorders.
#[derive(Clone, Default)]
struct RowAnim {
    /// Current height of each row, by pin.
    y: Vec<f32>,
    /// The pin being dragged, and where on its row it was grabbed.
    drag: Option<(usize, f32)>,
    /// Height of one row, measured last frame.
    row_h: f32,
}

/// Six dots, the usual sign for "grab here to move".
/// `id` must stay the same while the row moves (widgets' automatic ids depend on drawing order).
fn drag_handle(ui: &mut Ui, id: egui::Id, enabled: bool) -> egui::Response {
    let sense = if enabled { egui::Sense::drag() } else { egui::Sense::hover() };
    let (rect, _) = ui.allocate_exact_size(vec2(12.0, 20.0), egui::Sense::hover());
    let mut r = ui.interact(rect, id, sense);
    if enabled {
        r = r.on_hover_cursor(egui::CursorIcon::Grab).on_hover_text("Drag to reorder");
    }
    let dragging = r.dragged();
    if dragging {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    let color = if !enabled { Color32::from_gray(45) } else if dragging || r.hovered() { Color32::from_gray(170) } else { Color32::from_gray(85) };
    for col in [-2.0, 2.0] {
        for row in [-4.0, 0.0, 4.0] {
            ui.painter().circle_filled(rect.center() + vec2(col, row), 1.1, color);
        }
    }
    r
}

/// Move a switch's wires along with its pins (`map`: old pin -> new pin, `None` = removed).
/// A switch's pins are on one side only (outputs on an output switch, inputs on an input
/// switch); its single pin on the other side keeps its wires.
fn follow_pins(snarl: &mut Snarl<NodeKind>, node: NodeId, map: &[Option<usize>]) {
    let outputs = matches!(snarl.get_node(node), Some(NodeKind::SwitchOutput { .. }));
    let wires: Vec<(OutPinId, InPinId)> = snarl
        .wires()
        .filter(|(o, i)| if outputs { o.node == node } else { i.node == node })
        .collect();
    for &(o, i) in &wires {
        snarl.disconnect(o, i);
    }
    for (mut o, mut i) in wires {
        let pin = if outputs { &mut o.output } else { &mut i.input };
        let Some(Some(new)) = map.get(*pin).copied() else { continue };
        *pin = new;
        snarl.connect(o, i);
    }
}

/// Roughly how much room a node takes on screen at 100% zoom (the library doesn't expose sizes).
fn approx_size(kind: &NodeKind) -> egui::Vec2 {
    match kind {
        NodeKind::Eq(_) => vec2(340.0, 340.0),
        NodeKind::Levels(_) => vec2(320.0, 160.0),
        NodeKind::Volume(_) => vec2(320.0, 110.0),
        NodeKind::SwitchOutput { pins, .. } | NodeKind::SwitchInput { pins, .. } => vec2(360.0, 110.0 + 26.0 * pins.len() as f32),
        _ => vec2(290.0, 120.0),
    }
}

/// The zoom and pan that show every node inside `area`.
fn fit_transform(snarl: &Snarl<NodeKind>, area: Rect) -> Option<egui::emath::TSTransform> {
    let mut bb = Rect::NOTHING;
    for (_, pos, kind) in snarl.nodes_pos_ids() {
        bb.extend_with(pos);
        bb.extend_with(pos + approx_size(kind));
    }
    if !bb.is_finite() || !area.is_finite() {
        return None;
    }
    let bb = bb.expand(30.0);
    let scaling = (area.size() / bb.size()).min_elem().clamp(0.2, 1.5);
    Some(egui::emath::TSTransform::new(area.center().to_vec2() - bb.center().to_vec2() * scaling, scaling))
}

impl SnarlViewer<NodeKind> for Viewer<'_> {
    fn current_transform(&mut self, to_global: &mut egui::emath::TSTransform, snarl: &mut Snarl<NodeKind>) {
        if let Some(area) = self.fit_to.take()
            && let Some(t) = fit_transform(snarl, area)
        {
            *to_global = t;
        }
        self.transform = Some(*to_global);
    }

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
                format!("{kind} · {}", b.display())
            }
            NodeKind::Eq(_) => "EQ".into(),
            NodeKind::App { app: Some(a) } => format!("App · {}", self.app_name(&a.binary)),
            NodeKind::App { app: None } => "App".into(),
            NodeKind::OtherAppsPlayback => "Other apps · playback".into(),
            NodeKind::OtherAppsRecording => "Other apps · recording".into(),
            NodeKind::SwitchOutput { .. } => "Output switch".into(),
            NodeKind::SwitchInput { .. } => "Input switch".into(),
            NodeKind::Levels(_) => "Levels".into(),
            NodeKind::Volume(_) => "Volume".into(),
        }
    }

    fn inputs(&mut self, node: &NodeKind) -> usize {
        node.inputs()
    }

    fn outputs(&mut self, node: &NodeKind) -> usize {
        node.outputs()
    }

    fn show_input(&mut self, pin: &InPin, ui: &mut Ui, snarl: &mut Snarl<NodeKind>) -> impl SnarlPin + 'static {
        let node = pin.id.node;
        match &snarl[node] {
            NodeKind::SwitchInput { pins, .. } => {
                let on = self.slot_active(node, pin.id.input);
                let label = pins.get(pin.id.input).map(|p| p.label.as_str()).unwrap_or("");
                ui.label(RichText::new(pin_text(pin.id.input, label)).color(if on { GREEN } else { GREY }));
                Self::colored(if on { GREEN } else { GREY })
            }
            NodeKind::SwitchOutput { .. } => {
                ui.label("in");
                Self::colored(GREEN)
            }
            NodeKind::Levels(_) | NodeKind::Volume(_) => {
                ui.label("in");
                Self::colored(if self.running { GREEN } else { GREY })
            }
            NodeKind::OtherAppsRecording => {
                ui.label("in");
                Self::colored(if self.defaults.1.is_some() { GREEN } else { GREY })
            }
            _ => {
                ui.label("in");
                self.pin(node)
            }
        }
    }

    fn show_output(&mut self, pin: &OutPin, ui: &mut Ui, snarl: &mut Snarl<NodeKind>) -> impl SnarlPin + 'static {
        let node = pin.id.node;
        match &snarl[node] {
            NodeKind::SwitchOutput { pins, .. } => {
                let on = self.slot_active(node, pin.id.output);
                let label = pins.get(pin.id.output).map(|p| p.label.as_str()).unwrap_or("");
                ui.label(RichText::new(pin_text(pin.id.output, label)).color(if on { GREEN } else { GREY }));
                Self::colored(if on { GREEN } else { GREY })
            }
            NodeKind::SwitchInput { .. } => {
                ui.label("out");
                Self::colored(GREEN)
            }
            NodeKind::Levels(_) | NodeKind::Volume(_) => {
                ui.label("out");
                Self::colored(if self.running { GREEN } else { GREY })
            }
            NodeKind::OtherAppsPlayback => {
                ui.label("out");
                Self::colored(if self.defaults.0.is_some() { GREEN } else { GREY })
            }
            _ => {
                ui.label("out");
                self.pin(node)
            }
        }
    }

    fn has_body(&mut self, _node: &NodeKind) -> bool {
        true
    }

    fn show_body(&mut self, node: NodeId, _i: &[InPin], _o: &[OutPin], ui: &mut Ui, snarl: &mut Snarl<NodeKind>) {
        // Pins moved or removed this frame (old index -> new). Their wires follow after the match.
        let mut moved_pins: Option<Vec<Option<usize>>> = None;
        ui.vertical(|ui| {
            ui.set_max_width(260.0);
            match &mut snarl[node] {
                NodeKind::Input { device } => self.device_picker(ui, node, device, true),
                NodeKind::Output { device } => self.device_picker(ui, node, device, false),
                NodeKind::Bus(b) => {
                    // The label is the name programs and sound settings show.
                    ui.horizontal(|ui| {
                        ui.label("Label");
                        let r = ui.add(egui::TextEdit::singleline(&mut b.description).hint_text(b.name.as_str()).desired_width(170.0));
                        self.changed |= r.changed();
                    });
                    let present = self.present(node);
                    if present && self.running && !self.owned_buses.contains(&b.name) {
                        ui.label(RichText::new("Made outside this program, so its name there can't change yet. \
                            Approve, then use \"Take over\" in the bottom bar.").small().color(AMBER));
                    }
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
                    if present {
                        ui.label(RichText::new("✔ exists").color(GREEN).small());
                    } else {
                        ui.label(RichText::new("created when the new system is active").color(GREY).small());
                    }
                }
                NodeKind::Eq(eq) => {
                    let before = eq.clone();
                    eq_editor(ui, node, eq);
                    self.changed |= *eq != before;
                    self.not_running_note(ui);
                }
                NodeKind::App { app } => self.app_picker(ui, node, app),
                NodeKind::OtherAppsPlayback => {
                    ui.label(RichText::new("What every program without its own App node plays:").small().weak());
                    self.apps_list(ui, node);
                    let d = self.defaults.0;
                    self.default_note(ui, d, "output");
                }
                NodeKind::OtherAppsRecording => {
                    ui.label(RichText::new("Every program without its own App node records from here:").small().weak());
                    self.apps_list(ui, node);
                    let d = self.defaults.1;
                    self.default_note(ui, d, "input");
                }
                NodeKind::SwitchOutput { name, pins } => self.switch_editor(ui, node, name, pins, true, &mut moved_pins),
                NodeKind::SwitchInput { name, pins } => self.switch_editor(ui, node, name, pins, false, &mut moved_pins),
                NodeKind::Levels(l) => {
                    let reading = self.reading(&l.name);
                    let before = l.clone();
                    levels_editor(ui, node, l, reading);
                    self.changed |= *l != before;
                    self.not_running_note(ui);
                }
                NodeKind::Volume(v) => {
                    let reading = self.reading(&v.name);
                    let before = v.clone();
                    volume_editor(ui, v, reading);
                    self.changed |= *v != before;
                    self.not_running_note(ui);
                }
            }
        });
        if let Some(map) = moved_pins {
            follow_pins(snarl, node, &map);
        }
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
            add = Some(NodeKind::Bus(BusSpec::new(&unique_name(snarl, "output"), BusKind::Playback, Channels::Stereo)));
        }
        if ui.button("Virtual mic (apps record from it)").clicked() {
            add = Some(NodeKind::Bus(BusSpec::new(&unique_name(snarl, "mic"), BusKind::Microphone, Channels::Mono)));
        }
        if ui.button("Equalizer").clicked() {
            let mut eq = EqSpec::new(&unique_name(snarl, "eq"), Channels::Stereo);
            eq.bands = default_bands();
            add = Some(NodeKind::Eq(eq));
        }
        ui.separator();
        if ui.button("App (one program)").clicked() {
            add = Some(NodeKind::App { app: None });
        }
        if ui.button("Other apps · playback").clicked() {
            add = Some(NodeKind::OtherAppsPlayback);
        }
        if ui.button("Other apps · recording").clicked() {
            add = Some(NodeKind::OtherAppsRecording);
        }
        if ui.button("Levels (meter, gate, limiter)").clicked() {
            add = Some(NodeKind::Levels(LevelsSpec::new(&unique_name(snarl, "levels"))));
        }
        if ui.button("Volume").clicked() {
            add = Some(NodeKind::Volume(VolumeSpec { name: unique_name(snarl, "volume"), channels: Channels::Stereo, gain: 1.0 }));
        }
        ui.separator();
        if ui.button("Output switch").on_hover_text("Send audio to one of several outputs; you pick which").clicked() {
            add = Some(NodeKind::SwitchOutput { name: unique_name(snarl, "Output"), pins: vec![SwitchPin::default(); 2] });
        }
        if ui.button("Input switch").on_hover_text("Use one of several inputs; you pick which").clicked() {
            add = Some(NodeKind::SwitchInput { name: unique_name(snarl, "Input"), pins: vec![SwitchPin::default(); 2] });
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

/// `base-1`, `base-2`, ...: the first name no node uses yet.
fn unique_name(snarl: &Snarl<NodeKind>, base: &str) -> String {
    let taken: Vec<String> = snarl
        .nodes()
        .filter_map(|n| n.owned_name().map(str::to_owned).or_else(|| n.switch().map(|s| s.0.to_owned())))
        .collect();
    (1..).map(|k| format!("{base}-{k}")).find(|n| !taken.contains(n)).unwrap()
}

fn pin_text(k: usize, label: &str) -> String {
    if label.is_empty() { format!("{}", k + 1) } else { format!("{} {label}", k + 1) }
}

/// A button showing a keyboard shortcut. Click it, then press the keys; Esc cancels,
/// Backspace clears. Returns whether the key changed.
const KEY_CAPTURE: &str = "bsp-key-capture-frame";

/// Whether a shortcut field was waiting for keys last frame.
pub fn capturing_keys(ctx: &egui::Context) -> bool {
    let last: Option<u64> = ctx.data(|d| d.get_temp(egui::Id::new(KEY_CAPTURE)));
    last.is_some_and(|f| f + 1 >= ctx.cumulative_frame_nr())
}

fn key_field(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, key: &mut String) -> bool {
    let id = egui::Id::new(("key-capture", id));
    let capturing: bool = ui.data(|d| d.get_temp(id)).unwrap_or(false);
    let mut changed = false;
    if capturing {
        let pressed = ui.input(|i| {
            i.events.iter().find_map(|e| match e {
                // Ctrl, Alt etc. on their own come as keys too; wait for the key they go with.
                egui::Event::Key { key, pressed: true, modifiers, .. } if !is_modifier(*key) => Some((*key, *modifiers)),
                _ => None,
            })
        });
        match pressed {
            Some((egui::Key::Escape, _)) => {ui.data_mut(|d| d.insert_temp(id, false));}
            Some((egui::Key::Backspace | egui::Key::Delete, m)) if !m.any() => {
                changed = !key.is_empty();
                key.clear();
                ui.data_mut(|d| d.insert_temp(id, false));
            }
            Some((k, m)) => {
                let mut s = String::new();
                for (on, name) in [(m.ctrl, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift"), (m.mac_cmd, "Meta")] {
                    if on {
                        s.push_str(name);
                        s.push('+');
                    }
                }
                s.push_str(k.name());
                changed = *key != s;
                *key = s;
                ui.data_mut(|d| d.insert_temp(id, false));
            }
            None => {}
        }
    }
    let capturing: bool = ui.data(|d| d.get_temp(id)).unwrap_or(false);
    if capturing {
        // Keep editor shortcuts (copy, paste, delete) from acting on these keys.
        let frame = ui.ctx().cumulative_frame_nr();
        ui.data_mut(|d| d.insert_temp(egui::Id::new(KEY_CAPTURE), frame));
    }
    let text = if capturing {
        RichText::new("press keys…").color(AMBER)
    } else if key.is_empty() {
        RichText::new("set key").weak()
    } else {
        RichText::new(key.as_str())
    };
    let button = ui.add(egui::Button::new(text).small().min_size(egui::vec2(84.0, 0.0))).on_hover_text("Shortcut that selects this pin. Esc cancels, Backspace or right-click clears.");
    if button.clicked() {
        ui.data_mut(|d| d.insert_temp(id, !capturing));
    }
    if button.secondary_clicked() {
        changed |= !key.is_empty();
        key.clear();
        ui.data_mut(|d| d.insert_temp(id, false));
    }
    changed
}

fn is_modifier(key: egui::Key) -> bool {
    use egui::Key::*;
    matches!(key, ShiftLeft | ShiftRight | ControlLeft | ControlRight | AltLeft | AltRight | SuperLeft | SuperRight)
}

const METER_MIN: f32 = -60.0;
const METER_MAX: f32 = 6.0;

/// A horizontal level bar from -60 to +6 dB.
fn meter_bar(painter: &egui::Painter, rect: Rect, peak: Option<f32>) {
    painter.rect_filled(rect, 2.0, Color32::from_gray(22));
    let x = |db: f32| rect.left() + rect.width() * ((db - METER_MIN) / (METER_MAX - METER_MIN)).clamp(0.0, 1.0);
    if let Some(p) = peak {
        let db = dsp::lin_to_db(p);
        let color = if db > -0.5 { Color32::from_rgb(230, 80, 70) } else if db > -12.0 { AMBER } else { GREEN };
        painter.rect_filled(Rect::from_min_max(rect.left_top(), Pos2::new(x(db), rect.bottom())), 2.0, color);
    }
    painter.vline(x(0.0), rect.y_range(), Stroke::new(1.0, Color32::from_gray(90)));
}

/// Levels: input and output meters, with the gate (bottom) and limiter (top) thresholds as
/// handles you drag along the scale.
fn levels_editor(ui: &mut Ui, node: NodeId, l: &mut LevelsSpec, reading: Option<Reading>) {
    ui.horizontal(|ui| {
        for (c, label) in [(Channels::Mono, "Mono"), (Channels::Stereo, "Stereo")] {
            if ui.selectable_label(l.channels == c, label).clicked() {
                l.channels = c;
            }
        }
    });
    let width = 260.0;
    let (resp, painter) = ui.allocate_painter(vec2(width, 46.0), Sense::click_and_drag());
    let r = resp.rect;
    let scale = Rect::from_min_size(r.left_top() + vec2(0.0, 12.0), vec2(width, 34.0));
    let x = |db: f32| scale.left() + scale.width() * ((db - METER_MIN) / (METER_MAX - METER_MIN)).clamp(0.0, 1.0);
    let db_at = |px: f32| (METER_MIN + (px - scale.left()) / scale.width() * (METER_MAX - METER_MIN)).clamp(METER_MIN, 0.0);
    let in_rect = Rect::from_min_size(scale.left_top() + vec2(0.0, 2.0), vec2(width, 12.0));
    let out_rect = Rect::from_min_size(scale.left_top() + vec2(0.0, 18.0), vec2(width, 12.0));
    meter_bar(&painter, in_rect, reading.map(|r| r.in_peak));
    meter_bar(&painter, out_rect, reading.map(|r| r.out_peak));
    let small = egui::FontId::proportional(9.0);
    let dim = Color32::from_gray(160);
    painter.text(in_rect.right_center() - vec2(2.0, 0.0), egui::Align2::RIGHT_CENTER, "in", small.clone(), dim);
    painter.text(out_rect.right_center() - vec2(2.0, 0.0), egui::Align2::RIGHT_CENTER, "out", small.clone(), dim);
    for db in [-48.0f32, -36.0, -24.0, -12.0, 0.0] {
        painter.text(Pos2::new(x(db), r.top()), egui::Align2::CENTER_TOP, format!("{db:.0}"), small.clone(), GREY);
    }
    let gate_c = Color32::from_rgb(110, 170, 250);
    let limit_c = Color32::from_rgb(240, 120, 90);
    for (on, db, c) in [(l.gate_on, l.gate_db, gate_c), (l.limit_on, l.limit_db, limit_c)] {
        if on {
            painter.vline(x(db), scale.y_range(), Stroke::new(2.0, c));
            painter.circle_filled(Pos2::new(x(db), scale.top()), 4.0, c);
        }
    }
    // Drag whichever handle is nearest the press.
    let drag_id = egui::Id::new(("levels-drag", node));
    if resp.drag_started()
        && let Some(p) = resp.interact_pointer_pos()
    {
        let gate_d = if l.gate_on { (x(l.gate_db) - p.x).abs() } else { f32::MAX };
        let limit_d = if l.limit_on { (x(l.limit_db) - p.x).abs() } else { f32::MAX };
        let which = if gate_d.min(limit_d) > 12.0 { 0u8 } else if gate_d < limit_d { 1 } else { 2 };
        ui.data_mut(|d| d.insert_temp(drag_id, which));
    }
    if resp.dragged()
        && let Some(p) = resp.interact_pointer_pos()
    {
        let db = (db_at(p.x) * 2.0).round() / 2.0;
        match ui.data(|d| d.get_temp::<u8>(drag_id)).unwrap_or(0) {
            1 => l.gate_db = db.min(l.limit_db - 3.0),
            2 => l.limit_db = db.max(l.gate_db + 3.0),
            _ => {}
        }
    }
    resp.on_hover_text("Blue: gate (anything quieter is muted). Red: limiter (never louder than this). Drag them.");
    ui.horizontal(|ui| {
        ui.checkbox(&mut l.gate_on, RichText::new("Gate").color(gate_c));
        ui.add_enabled(l.gate_on, egui::DragValue::new(&mut l.gate_db).range(METER_MIN..=0.0).speed(0.2).suffix(" dB"));
        ui.checkbox(&mut l.limit_on, RichText::new("Limiter").color(limit_c));
        ui.add_enabled(l.limit_on, egui::DragValue::new(&mut l.limit_db).range(METER_MIN..=0.0).speed(0.2).suffix(" dB"));
    });
    if let Some(r) = reading {
        ui.horizontal(|ui| {
            if l.gate_on {
                let (t, c) = if r.gate > 0.5 { ("gate open", GREEN) } else { ("gate closed", GREY) };
                ui.label(RichText::new(t).color(c).small());
            }
            if l.limit_on && r.limit < 0.99 {
                ui.label(RichText::new(format!("limiting {:.1} dB", dsp::lin_to_db(r.limit))).color(limit_c).small());
            }
        });
    }
}

/// Volume: a linear 0–200% slider (type a number for anything beyond), with a level meter.
fn volume_editor(ui: &mut Ui, v: &mut VolumeSpec, reading: Option<Reading>) {
    ui.horizontal(|ui| {
        for (c, label) in [(Channels::Mono, "Mono"), (Channels::Stereo, "Stereo")] {
            if ui.selectable_label(v.channels == c, label).clicked() {
                v.channels = c;
            }
        }
    });
    let mut pct = v.gain * 100.0;
    let r = ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 170.0;
        // Never clamps: a typed value past 200% stays (up to the maximum below).
        let r = ui.add(
            egui::Slider::new(&mut pct, 0.0..=200.0)
                .clamping(egui::SliderClamping::Never)
                .step_by(1.0)
                .suffix("%")
                .max_decimals(0),
        );
        if ui.small_button("100%").clicked() {
            pct = 100.0;
        }
        r
    });
    let r = r.inner;
    if r.double_clicked() {
        pct = 100.0;
    }
    // Only write back real changes: gain -> % -> gain isn't exact in floats, and a drift of
    // one bit would count as an unsaved edit.
    if pct != v.gain * 100.0 {
        v.gain = (pct / 100.0).clamp(0.0, MAX_VOLUME);
    }
    // Output meter, -60 to +6 dB like the Levels node.
    let (rect, _) = ui.allocate_exact_size(vec2(170.0, 6.0), Sense::hover());
    meter_bar(ui.painter(), rect, reading.map(|r| r.out_peak));
    let db_text = if v.gain <= 0.0 { "muted".to_owned() } else { format!("{:+.1} dB", dsp::lin_to_db(v.gain)) };
    ui.label(RichText::new(format!("{db_text} · type a number for more than 200%")).small().weak());
}

/// A neutral starting point: four bells spread over the spectrum, all at 0 dB.
fn default_bands() -> Vec<Band> {
    [100.0, 500.0, 2000.0, 8000.0]
        .into_iter()
        .map(|freq| Band { kind: BandKind::Bell, freq, gain_db: 0.0, q: 1.0, enabled: true })
        .collect()
}

const F_MIN: f32 = 20.0;
const F_MAX: f32 = 20000.0;
const DB_RANGE: f32 = 24.0;
const BAND_COLORS: [Color32; 6] = [
    Color32::from_rgb(240, 180, 70),
    Color32::from_rgb(110, 190, 250),
    Color32::from_rgb(230, 110, 160),
    Color32::from_rgb(130, 220, 140),
    Color32::from_rgb(190, 150, 250),
    Color32::from_rgb(250, 130, 90),
];

struct Axes {
    rect: Rect,
}

impl Axes {
    fn x(&self, f: f32) -> f32 {
        self.rect.left() + self.rect.width() * (f / F_MIN).log10() / (F_MAX / F_MIN).log10()
    }
    fn freq(&self, x: f32) -> f32 {
        let t = ((x - self.rect.left()) / self.rect.width()).clamp(0.0, 1.0);
        F_MIN * (F_MAX / F_MIN).powf(t)
    }
    fn y(&self, db: f32) -> f32 {
        self.rect.center().y - db.clamp(-DB_RANGE, DB_RANGE) / DB_RANGE * self.rect.height() / 2.0
    }
    fn db(&self, y: f32) -> f32 {
        ((self.rect.center().y - y) / (self.rect.height() / 2.0) * DB_RANGE).clamp(-DB_RANGE, DB_RANGE)
    }
}

fn band_color(k: usize) -> Color32 {
    BAND_COLORS[k % BAND_COLORS.len()]
}

/// Where a band's handle sits: at its gain for bells/shelves, on the curve for the others.
fn handle_pos(ax: &Axes, eq: &EqSpec, k: usize) -> Pos2 {
    let b = &eq.bands[k];
    let db = if b.kind.has_gain() { b.gain_db } else { dsp::response_db(0.0, std::slice::from_ref(b), b.freq as f64) as f32 };
    Pos2::new(ax.x(b.freq), ax.y(db))
}

fn fmt_freq(f: f32) -> String {
    if f >= 1000.0 { format!("{:.1} kHz", f / 1000.0) } else { format!("{f:.0} Hz") }
}

/// The equalizer editor: a response curve with draggable band handles, plus fields for the
/// selected band. Drag a handle to move a band (left/right = frequency, up/down = gain);
/// double-click empty space to add one.
fn eq_editor(ui: &mut Ui, node: NodeId, eq: &mut EqSpec) {
    let sel_id = egui::Id::new(("eq-selected", node));
    let drag_id = egui::Id::new(("eq-dragging", node));
    let mut selected: usize = ui.data(|d| d.get_temp(sel_id)).unwrap_or(0);

    ui.horizontal(|ui| {
        for (c, label) in [(Channels::Mono, "Mono"), (Channels::Stereo, "Stereo")] {
            if ui.selectable_label(eq.channels == c, label).clicked() {
                eq.channels = c;
            }
        }
    });

    let (resp, painter) = ui.allocate_painter(vec2(300.0, 150.0), Sense::click_and_drag());
    let ax = Axes { rect: resp.rect };
    painter.rect_filled(ax.rect, 4.0, Color32::from_gray(22));
    let grid = Stroke::new(1.0, Color32::from_gray(45));
    for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0] {
        painter.vline(ax.x(f), ax.rect.y_range(), grid);
    }
    for db in [-12.0, 12.0] {
        painter.hline(ax.rect.x_range(), ax.y(db), grid);
    }
    painter.hline(ax.rect.x_range(), ax.y(0.0), Stroke::new(1.0, Color32::from_gray(75)));
    let small = egui::FontId::proportional(9.0);
    for (f, t) in [(100.0, "100"), (1000.0, "1k"), (10000.0, "10k")] {
        painter.text(Pos2::new(ax.x(f) + 2.0, ax.rect.bottom() - 2.0), egui::Align2::LEFT_BOTTOM, t, small.clone(), GREY);
    }
    for db in [-12.0f32, 12.0] {
        painter.text(Pos2::new(ax.rect.left() + 2.0, ax.y(db)), egui::Align2::LEFT_CENTER, format!("{db:+}"), small.clone(), GREY);
    }

    // Interaction: pick the nearest handle on press, then follow the pointer.
    if resp.drag_started()
        && let Some(p) = resp.interact_pointer_pos()
    {
        let near = (0..eq.bands.len())
            .map(|k| (k, handle_pos(&ax, eq, k).distance(p)))
            .filter(|(_, d)| *d < 14.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(k, _)| k);
        if let Some(k) = near {
            selected = k;
        }
        ui.data_mut(|d| d.insert_temp(drag_id, near));
    }
    let dragging: Option<usize> = ui.data(|d| d.get_temp(drag_id)).flatten();
    if resp.dragged()
        && let (Some(k), Some(p)) = (dragging, resp.interact_pointer_pos())
        && let Some(b) = eq.bands.get_mut(k)
    {
        b.freq = (ax.freq(p.x) * 10.0).round() / 10.0;
        if b.kind.has_gain() {
            b.gain_db = (ax.db(p.y) * 10.0).round() / 10.0;
        }
    }
    if resp.drag_stopped() {
        ui.data_mut(|d| d.remove::<Option<usize>>(drag_id));
    }
    if resp.clicked()
        && let Some(p) = resp.interact_pointer_pos()
        && let Some((k, _)) = (0..eq.bands.len())
            .map(|k| (k, handle_pos(&ax, eq, k).distance(p)))
            .filter(|(_, d)| *d < 14.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
    {
        selected = k;
    }
    if resp.double_clicked()
        && eq.bands.len() < MAX_BANDS
        && let Some(p) = resp.interact_pointer_pos()
        && !(0..eq.bands.len()).any(|k| handle_pos(&ax, eq, k).distance(p) < 14.0)
    {
        let freq = (ax.freq(p.x) * 10.0).round() / 10.0;
        eq.bands.push(Band { kind: BandKind::Bell, freq, gain_db: (ax.db(p.y) * 10.0).round() / 10.0, q: 1.0, enabled: true });
        selected = eq.bands.len() - 1;
    }

    // The curve: total response including preamp.
    let points: Vec<Pos2> = (0..=150)
        .map(|i| {
            let x = ax.rect.left() + ax.rect.width() * i as f32 / 150.0;
            Pos2::new(x, ax.y(dsp::response_db(eq.preamp_db, &eq.bands, ax.freq(x) as f64) as f32))
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(2.0, Color32::from_rgb(120, 200, 255))));
    for k in 0..eq.bands.len() {
        let p = handle_pos(&ax, eq, k);
        let c = if eq.bands[k].enabled { band_color(k) } else { Color32::from_gray(90) };
        painter.circle(p, if k == selected { 6.0 } else { 4.5 }, c, Stroke::new(1.5, Color32::from_gray(20)));
        painter.text(p + vec2(0.0, -10.0), egui::Align2::CENTER_BOTTOM, format!("{}", k + 1), small.clone(), c);
    }
    resp.on_hover_text("Drag a dot: left/right = frequency, up/down = gain.\nDouble-click empty space to add a band.");

    // Band selector and fields.
    ui.horizontal_wrapped(|ui| {
        for k in 0..eq.bands.len() {
            let text = RichText::new(format!("{}", k + 1)).color(band_color(k));
            if ui.selectable_label(k == selected, text).clicked() {
                selected = k;
            }
        }
        if eq.bands.len() < MAX_BANDS && ui.small_button("+").on_hover_text("Add a band").clicked() {
            eq.bands.push(Band { kind: BandKind::Bell, freq: 1000.0, gain_db: 0.0, q: 1.0, enabled: true });
            selected = eq.bands.len() - 1;
        }
    });
    selected = selected.min(eq.bands.len().saturating_sub(1));
    let mut remove = None;
    if let Some(b) = eq.bands.get_mut(selected) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("eq-kind", node, selected))
                .selected_text(b.kind.label())
                .width(90.0)
                .show_ui(ui, |ui| {
                    for k in BandKind::ALL {
                        ui.selectable_value(&mut b.kind, k, k.label());
                    }
                });
            ui.checkbox(&mut b.enabled, "on");
            if ui.small_button("🗑").on_hover_text("Remove this band").clicked() {
                remove = Some(selected);
            }
        });
        ui.horizontal(|ui| {
            let speed = b.freq * 0.01;
            ui.add(
                egui::DragValue::new(&mut b.freq)
                    .range(F_MIN..=F_MAX)
                    .speed(speed)
                    .custom_formatter(|v, _| fmt_freq(v as f32))
                    .custom_parser(|s| {
                        let s = s.trim().to_lowercase();
                        let (num, mul) = match s.strip_suffix("khz").or_else(|| s.strip_suffix('k')) {
                            Some(n) => (n.to_owned(), 1000.0),
                            None => (s.trim_end_matches("hz").to_owned(), 1.0),
                        };
                        num.trim().parse::<f64>().ok().map(|v| v * mul)
                    }),
            );
            ui.add_enabled(b.kind.has_gain(), egui::DragValue::new(&mut b.gain_db).range(-DB_RANGE..=DB_RANGE).speed(0.1).suffix(" dB"));
            ui.add(egui::DragValue::new(&mut b.q).range(0.1..=20.0).speed(0.02).prefix("Q "));
        });
    }
    if let Some(k) = remove {
        eq.bands.remove(k);
        selected = selected.saturating_sub(1);
    }
    ui.horizontal(|ui| {
        ui.label("Preamp");
        ui.add(egui::DragValue::new(&mut eq.preamp_db).range(-24.0..=12.0).speed(0.1).suffix(" dB"))
            .on_hover_text("Turn this down if loud boosts distort");
        let peak = (0..200)
            .map(|i| dsp::response_db(eq.preamp_db, &eq.bands, (F_MIN * (F_MAX / F_MIN).powf(i as f32 / 199.0)) as f64))
            .fold(f64::MIN, f64::max);
        if peak > 6.0 {
            ui.label(RichText::new(format!("peak +{peak:.0} dB")).color(AMBER).small())
                .on_hover_text("Loud sounds near the peak may clip. Lower the preamp if you hear distortion.");
        }
    });
    ui.data_mut(|d| d.insert_temp(sel_id, selected));
}


#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn pins(labels: &[&str]) -> Vec<SwitchPin> {
        labels.iter().map(|l| SwitchPin { label: (*l).into(), key: String::new() }).collect()
    }

    /// Where each output pin of `node` is wired to, by pin number.
    fn targets(snarl: &Snarl<NodeKind>, node: NodeId) -> Vec<(usize, NodeId)> {
        let mut v: Vec<_> = snarl.wires().filter(|(o, _)| o.node == node).map(|(o, i)| (o.output, i.node)).collect();
        v.sort();
        v
    }

    #[test]
    fn removing_or_moving_a_switch_pin_renumbers_and_wires_follow() {
        let mut snarl = Snarl::new();
        let src = snarl.insert_node(pos2(0.0, 0.0), NodeKind::OtherAppsPlayback);
        let sw = snarl.insert_node(pos2(0.0, 0.0), NodeKind::SwitchOutput { name: "Out".into(), pins: pins(&["Speakers", "Headset", "Index"]) });
        let outs: Vec<NodeId> = (0..3).map(|_| snarl.insert_node(pos2(0.0, 0.0), NodeKind::Output { device: None })).collect();
        snarl.connect(OutPinId { node: src, output: 0 }, InPinId { node: sw, input: 0 });
        for (k, &o) in outs.iter().enumerate() {
            snarl.connect(OutPinId { node: sw, output: k }, InPinId { node: o, input: 0 });
        }

        // Remove Speakers: Headset becomes 1, Index 2, and Speakers' wire goes.
        let NodeKind::SwitchOutput { pins: p, .. } = &mut snarl[sw] else { unreachable!() };
        let map = PinOp::Remove(0).apply(p);
        assert_eq!(p.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(), ["Headset", "Index"]);
        assert_eq!(map, [None, Some(0), Some(1)]);
        follow_pins(&mut snarl, sw, &map);
        assert_eq!(targets(&snarl, sw), [(0, outs[1]), (1, outs[2])]);

        // Move Index up: it becomes 1, and its wire comes along.
        let NodeKind::SwitchOutput { pins: p, .. } = &mut snarl[sw] else { unreachable!() };
        let map = PinOp::Move(1, 0).apply(p);
        assert_eq!(p.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(), ["Index", "Headset"]);
        follow_pins(&mut snarl, sw, &map);
        assert_eq!(targets(&snarl, sw), [(0, outs[2]), (1, outs[1])]);
        // The input side is untouched.
        assert!(snarl.wires().any(|(o, i)| o.node == src && i.node == sw));
    }

    #[test]
    fn moving_a_pin_shifts_the_ones_between() {
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert_eq!(PinOp::Move(0, 3).apply(&mut v), [Some(3), Some(0), Some(1), Some(2)]);
        assert_eq!(v, ['b', 'c', 'd', 'a']);
        assert_eq!(PinOp::Move(3, 1).apply(&mut v), [Some(0), Some(2), Some(3), Some(1)]);
        assert_eq!(v, ['b', 'a', 'c', 'd']);
        assert_eq!(PinOp::Remove(1).apply(&mut v), [Some(0), None, Some(1), Some(2)]);
        assert_eq!(v, ['b', 'c', 'd']);
    }
}
