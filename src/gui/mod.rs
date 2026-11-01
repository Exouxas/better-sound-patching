//! Native GUI (egui): node editor, mixer and trial/approve controls.
//!
//! The GUI never changes routing itself. It watches PipeWire read-only, edits the graph file,
//! and asks the daemon to apply it.

mod graph;

use crate::engine::{self, Cmd, EngineConfig};
use crate::dsp::Reading;
use crate::ipc::{self, Request, Response, Status};
use crate::live::Live;
use crate::model::{self, BusKind, Graph, NodeKind, Selections};
use crate::planner;
use crate::session::{self, KNOWN_CONFLICTS, PausedApp};
use anyhow::{Result, anyhow};
use egui::{Color32, RichText};
use egui_snarl::Snarl;
use egui_snarl::ui::{NodeLayout, PinPlacement, SnarlStyle, SnarlWidget};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `BSP_SCREENSHOT_SIZE=2100x1290` opens the window at that size (for docs screenshots).
fn window_size() -> [f32; 2] {
    std::env::var("BSP_SCREENSHOT_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some([w.parse().ok()?, h.parse().ok()?])
        })
        .unwrap_or([1400.0, 860.0])
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(window_size())
            .with_title("Better Sound Patching")
            .with_app_id("better-sound-patching"),
        ..Default::default()
    };
    eframe::run_native("Better Sound Patching", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
        .map_err(|e| anyhow!("{e}"))
}

#[derive(Default, Clone)]
struct DaemonInfo {
    status: Option<Status>,
    approved: bool,
    paused_pending: Vec<PausedApp>,
    running: HashMap<&'static str, bool>,
    /// Switch selections saved on disk (used while the daemon isn't running).
    saved_selected: Selections,
}

enum Dialog {
    Trial { pause: Vec<(&'static str, bool)> },
    Approve { disable: Vec<(&'static str, bool)> },
    Revert,
}

struct App {
    live: Arc<Mutex<Live>>,
    daemon: Arc<Mutex<DaemonInfo>>,
    volumes: Arc<Mutex<HashMap<u32, (f32, bool)>>>,
    volume_ids: Arc<Mutex<Vec<u32>>>,
    toast: Arc<Mutex<Option<(String, bool, Instant)>>>,
    busy: Arc<Mutex<bool>>,

    saved: Graph,
    /// Switch selections as last clicked here, and when (shown until the daemon catches up).
    selected: Selections,
    selected_at: Option<Instant>,
    /// Signal levels of processing nodes, refreshed ~25 times a second while any are shown.
    readings: Arc<Mutex<BTreeMap<String, Reading>>>,
    want_levels: Arc<AtomicBool>,
    /// Zoom the graph to show every node on the next frame.
    fit_pending: bool,
    snarl: Snarl<NodeKind>,
    style: SnarlStyle,
    dialog: Option<Dialog>,
    dragging: HashMap<u32, (f32, Instant)>,
    labels: model::Labels,
    /// Last unsaved version of the graph sent to the daemon for live preview.
    preview_sent: Option<Graph>,
    preview_at: Instant,
    /// The daemon is currently playing unsaved edits.
    previewing: bool,
    _engine: pipewire::channel::Sender<Cmd>,
}

fn spawn_observer(ctx: egui::Context, live: Arc<Mutex<Live>>) -> pipewire::channel::Sender<Cmd> {
    let (tx, rx) = pipewire::channel::channel();
    std::thread::spawn(move || {
        let cfg = EngineConfig {
            enforce: false,
            on_snapshot: Box::new(move |l: &Live| {
                *live.lock().unwrap() = l.clone();
                ctx.request_repaint();
            }),
            paused: vec![],
            hotkeys: None,
        };
        if let Err(e) = engine::run(cfg, rx) {
            log::error!("observer stopped: {e:#}");
        }
    });
    tx
}

fn spawn_pollers(
    ctx: egui::Context,
    daemon: Arc<Mutex<DaemonInfo>>,
    volumes: Arc<Mutex<HashMap<u32, (f32, bool)>>>,
    ids: Arc<Mutex<Vec<u32>>>,
) {
    std::thread::spawn(move || {
        loop {
            let mut info = DaemonInfo {
                status: ipc::status(),
                approved: session::is_approved(),
                paused_pending: session::paused_pending(),
                running: HashMap::new(),
                saved_selected: model::load_state().selected,
            };
            for (name, _) in KNOWN_CONFLICTS {
                info.running.insert(name, session::is_running(name));
            }
            *daemon.lock().unwrap() = info;

            let wanted = ids.lock().unwrap().clone();
            let mut vols = HashMap::new();
            for id in wanted {
                if let Some(v) = read_volume(id) {
                    vols.insert(id, v);
                }
            }
            *volumes.lock().unwrap() = vols;
            ctx.request_repaint();
            std::thread::sleep(Duration::from_millis(700));
        }
    });
}

/// Poll the daemon for levels while the graph shows any processing node. Peaks fall back
/// gradually (about 24 dB a second) so the meters are readable.
fn spawn_level_poller(ctx: egui::Context, readings: Arc<Mutex<BTreeMap<String, Reading>>>, want: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let fall = crate::dsp::db_to_lin(-1.0);
        loop {
            std::thread::sleep(Duration::from_millis(40));
            if !want.load(Ordering::Relaxed) {
                continue;
            }
            let Ok(Response::Levels(new)) = ipc::request(&Request::Levels) else {
                readings.lock().unwrap().clear();
                std::thread::sleep(Duration::from_millis(500));
                continue;
            };
            let mut shown = readings.lock().unwrap();
            let old = std::mem::take(&mut *shown);
            for (name, r) in new {
                let prev = old.get(&name).copied().unwrap_or(r);
                shown.insert(
                    name,
                    Reading {
                        in_peak: r.in_peak.max(prev.in_peak * fall),
                        out_peak: r.out_peak.max(prev.out_peak * fall),
                        gate: r.gate,
                        limit: r.limit.min(1.0 - (1.0 - prev.limit) * 0.9),
                    },
                );
            }
            drop(shown);
            ctx.request_repaint();
        }
    });
}

fn read_volume(id: u32) -> Option<(f32, bool)> {
    let out = std::process::Command::new("wpctl").args(["get-volume", &id.to_string()]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = text.split_whitespace().nth(1)?.parse().ok()?;
    Some((v, text.contains("[MUTED]")))
}

fn wpctl(args: Vec<String>) {
    std::thread::spawn(move || {
        let _ = std::process::Command::new("wpctl").args(&args).status();
    });
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let live = Arc::new(Mutex::new(Live::default()));
        let daemon = Arc::new(Mutex::new(DaemonInfo::default()));
        let volumes = Arc::new(Mutex::new(HashMap::new()));
        let volume_ids = Arc::new(Mutex::new(Vec::new()));
        let engine = spawn_observer(cc.egui_ctx.clone(), live.clone());
        spawn_pollers(cc.egui_ctx.clone(), daemon.clone(), volumes.clone(), volume_ids.clone());
        let readings = Arc::new(Mutex::new(BTreeMap::new()));
        let want_levels = Arc::new(AtomicBool::new(false));
        spawn_level_poller(cc.egui_ctx.clone(), readings.clone(), want_levels.clone());

        let style = SnarlStyle {
            node_layout: Some(NodeLayout::coil()),
            pin_placement: Some(PinPlacement::Edge),
            pin_size: Some(9.0),
            ..SnarlStyle::new()
        };
        let mut app = App {
            live,
            daemon,
            volumes,
            volume_ids,
            toast: Arc::new(Mutex::new(None)),
            busy: Arc::new(Mutex::new(false)),
            saved: Graph::default(),
            selected: model::load_state().selected,
            selected_at: None,
            readings,
            want_levels,
            fit_pending: true,
            snarl: Snarl::new(),
            style,
            dialog: None,
            dragging: HashMap::new(),
            labels: model::load_labels(),
            preview_sent: None,
            preview_at: Instant::now(),
            previewing: false,
            _engine: engine,
        };
        app.open_graph();
        app
    }

    /// Load the saved graph, or a starter graph (unsaved) if there is none yet.
    fn open_graph(&mut self) {
        self.drop_preview();
        self.fit_pending = true;
        if !model::graph_exists() {
            self.snarl = graph::to_snarl(&starter_graph());
            self.saved = Graph::default();
            return;
        }
        match model::load_graph() {
            Ok(g) => {
                self.snarl = graph::to_snarl(&g);
                // Compare against the normalized form (node ids are renumbered on load).
                self.saved = graph::from_snarl(&self.snarl);
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn current(&self) -> Graph {
        graph::from_snarl(&self.snarl)
    }

    fn dirty(&self) -> bool {
        self.current() != self.saved
    }

    fn notify(&self, msg: impl Into<String>, error: bool) {
        *self.toast.lock().unwrap() = Some((msg.into(), error, Instant::now()));
    }

    fn save(&mut self) {
        let graph = self.current();
        let dupes = model::duplicate_names(&graph);
        if !dupes.is_empty() {
            self.notify(format!("Two nodes share a name: {}", dupes.join(", ")), true);
            return;
        }
        match model::save_graph(&graph) {
            Ok(()) => {
                self.saved = graph;
                self.preview_sent = None;
                self.previewing = false;
                if self.daemon.lock().unwrap().status.is_some() {
                    match ipc::request(&Request::Reload) {
                        Ok(Response::Error(e)) => self.notify(e, true),
                        Err(e) => self.notify(format!("{e:#}"), true),
                        _ => self.notify("Saved and applied", false),
                    }
                } else {
                    self.notify("Saved", false);
                }
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    /// Use a switch pin: right away when the new system runs, else from its next start.
    fn select(&mut self, switch: String, pin: u32) {
        self.selected.insert(switch.clone(), pin);
        self.selected_at = Some(Instant::now());
        if self.daemon.lock().unwrap().status.is_some() {
            let toast = self.toast.clone();
            std::thread::spawn(move || {
                if let Ok(Response::Error(e)) | Err(e) = ipc::request(&Request::Select { switch, pin }).map_err(|e| format!("{e:#}")) {
                    *toast.lock().unwrap() = Some((e, true, Instant::now()));
                }
            });
        } else {
            let mut state = model::load_state();
            state.selected = self.selected.clone();
            if let Err(e) = model::save_state(&state) {
                self.notify(format!("{e:#}"), true);
            }
        }
    }

    /// Which pins the switches use: what the daemon reports (or the saved choice when it isn't
    /// running), except right after a click here, until the daemon has caught up.
    fn effective_selected(&mut self, info: &DaemonInfo) -> Selections {
        if self.selected_at.is_some_and(|t| t.elapsed() < Duration::from_millis(1500)) {
            return self.selected.clone();
        }
        self.selected_at = None;
        self.selected = info.status.as_ref().map(|s| s.selected.clone()).unwrap_or_else(|| info.saved_selected.clone());
        self.selected.clone()
    }

    /// Edits play live while the new system runs (about 30 updates a second while dragging).
    /// Save makes them permanent; discarding or closing without saving reverts them.
    fn sync_preview(&mut self, ctx: &egui::Context, daemon_running: bool) {
        if !daemon_running {
            return;
        }
        let current = self.current();
        let last = self.preview_sent.as_ref().unwrap_or(&self.saved);
        if current == *last {
            return;
        }
        if self.preview_at.elapsed() < Duration::from_millis(33) {
            ctx.request_repaint_after(Duration::from_millis(35));
            return;
        }
        self.preview_at = Instant::now();
        self.previewing = current != self.saved;
        self.preview_sent = Some(current.clone());
        let toast = self.toast.clone();
        std::thread::spawn(move || {
            if let Ok(Response::Error(e)) | Err(e) = ipc::request(&Request::Preview { profile: current }).map_err(|e| format!("{e:#}")) {
                *toast.lock().unwrap() = Some((e, true, Instant::now()));
            }
        });
    }

    /// Throw away live, unsaved edits: the daemon goes back to the saved graph.
    fn drop_preview(&mut self) {
        self.preview_sent = None;
        if std::mem::take(&mut self.previewing) {
            std::thread::spawn(|| {
                let _ = ipc::request(&Request::Reload);
            });
        }
    }

    /// Run a slow action off the UI thread and report its result as a toast.
    fn background(&self, f: impl FnOnce() -> Result<String> + Send + 'static) {
        let toast = self.toast.clone();
        let busy = self.busy.clone();
        *busy.lock().unwrap() = true;
        std::thread::spawn(move || {
            let r = f();
            *toast.lock().unwrap() = Some(match r {
                Ok(m) => (m, false, Instant::now()),
                Err(e) => (format!("{e:#}"), true, Instant::now()),
            });
            *busy.lock().unwrap() = false;
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Better Sound Patching");
            ui.separator();
            let dirty = self.dirty();
            if ui.add_enabled(dirty, egui::Button::new(if dirty { "Save*" } else { "Saved" })).on_hover_text("Ctrl+S").clicked() {
                self.save();
            }
            if ui
                .add_enabled(dirty && model::graph_exists(), egui::Button::new("Discard changes"))
                .on_hover_text("Go back to the saved graph")
                .clicked()
            {
                self.open_graph();
            }
            ui.separator();
            if ui.button("Fit view").on_hover_text("Zoom to show the whole graph").clicked() {
                self.fit_pending = true;
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, info: &DaemonInfo) {
        let busy = *self.busy.lock().unwrap();
        ui.horizontal(|ui| {
            match (&info.status, info.approved) {
                (Some(s), true) => {
                    ui.label(RichText::new("✔ New system active").color(Color32::from_rgb(80, 200, 120)));
                    ui.label(format!("— starts at login · {} links", s.links_owned));
                    if ui.add_enabled(!busy, egui::Button::new("Revert to old setup…")).clicked() {
                        self.dialog = Some(Dialog::Revert);
                    }
                }
                (Some(s), false) => {
                    ui.label(RichText::new("⚠ Trial running — not permanent").color(Color32::from_rgb(230, 170, 60)));
                    ui.label(format!(
                        "— {} links · nothing permanent changed{}",
                        s.links_owned,
                        if s.paused.is_empty() { String::new() } else { format!(" · paused: {}", s.paused.join(", ")) }
                    ));
                    if ui.add_enabled(!busy, egui::Button::new("Stop trial")).clicked() {
                        self.background(|| {
                            session::stop_daemon(true)?;
                            Ok("Trial stopped; your old setup is back".into())
                        });
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Approve…")).clicked() {
                        let disable = KNOWN_CONFLICTS
                            .iter()
                            .map(|(n, _)| (*n, s.paused.iter().any(|p| p == n)))
                            .collect();
                        self.dialog = Some(Dialog::Approve { disable });
                    }
                }
                (None, approved) => {
                    if approved {
                        ui.label(RichText::new("⚠ Approved, but the daemon is not running").color(Color32::from_rgb(230, 90, 80)));
                        ui.label("(check `systemctl --user status better-sound-patching`)");
                        if ui.button("Revert…").clicked() {
                            self.dialog = Some(Dialog::Revert);
                        }
                    } else {
                        ui.label(RichText::new("Your normal setup is in charge").color(Color32::GRAY));
                        if ui.add_enabled(!busy, egui::Button::new("Try the new system…")).clicked() {
                            let defaults = session::default_conflicts();
                            let pause = KNOWN_CONFLICTS
                                .iter()
                                .map(|(n, _)| (*n, defaults.iter().any(|d| d == n) && info.running.get(n).copied().unwrap_or(false)))
                                .collect();
                            self.dialog = Some(Dialog::Trial { pause });
                        }
                    }
                    if !info.paused_pending.is_empty() {
                        let names: Vec<_> = info.paused_pending.iter().map(|p| p.name.as_str()).collect();
                        ui.label(RichText::new(format!("{} still paused", names.join(", "))).color(Color32::from_rgb(230, 170, 60)));
                        if ui.button("Restore").clicked() {
                            self.background(|| Ok(format!("Restored: {}", session::restore_paused()?.join(", "))));
                        }
                    }
                }
            }
            if self.previewing && self.dirty() {
                ui.separator();
                ui.label(RichText::new("Your unsaved changes are playing live — Save to keep them").color(Color32::from_rgb(120, 190, 240)));
            }
            if busy {
                ui.spinner();
            }
            if let Some(err) = info.status.as_ref().and_then(|s| s.last_error.as_ref()) {
                ui.label(RichText::new(format!("daemon: {err}")).color(Color32::from_rgb(230, 90, 80)).small());
            }
        });
    }

    fn mixer(&mut self, ui: &mut egui::Ui, live: &Live) {
        ui.heading("Mixer");
        ui.add_space(4.0);
        let profile = self.current();
        let mut strips: Vec<(u32, String, &'static str)> = vec![];
        for n in &profile.nodes {
            if let NodeKind::Bus(b) = &n.kind
                && let Some(ln) = live.node_by_name(&b.name)
            {
                let icon = if b.kind == BusKind::Playback { "🔊" } else { "🎤" };
                strips.push((ln.id, graph::display_name(&self.labels, &b.name, &b.name), icon));
            }
        }
        let mut devices: Vec<(u32, String, &'static str)> = live
            .nodes
            .values()
            .filter(|n| n.is_hardware())
            .map(|n| (n.id, graph::display_name(&self.labels, &n.name, n.label()), if n.is_sink() { "🔊" } else { "🎤" }))
            .collect();
        devices.sort_by(|a, b| a.1.cmp(&b.1));

        *self.volume_ids.lock().unwrap() = strips.iter().chain(&devices).map(|s| s.0).collect();
        let vols = self.volumes.lock().unwrap().clone();

        if strips.is_empty() {
            ui.label(RichText::new("No virtual devices are running yet.").weak());
        }
        for (id, name, icon) in &strips {
            self.strip(ui, *id, &format!("{icon} {name}"), &vols);
        }
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Hardware devices").default_open(false).show(ui, |ui| {
            for (id, name, icon) in &devices {
                self.strip(ui, *id, &format!("{icon} {name}"), &vols);
            }
        });
    }

    fn strip(&mut self, ui: &mut egui::Ui, id: u32, label: &str, vols: &HashMap<u32, (f32, bool)>) {
        let (polled, muted) = vols.get(&id).copied().unwrap_or((1.0, false));
        // While dragging (and briefly after), show our value instead of the polled one.
        let mut v = match self.dragging.get(&id) {
            Some((v, t)) if t.elapsed() < Duration::from_millis(1500) => *v,
            _ => polled,
        };
        ui.label(label);
        ui.horizontal(|ui| {
            let mute_label = if muted { RichText::new("M").color(Color32::from_rgb(230, 90, 80)) } else { RichText::new("M") };
            if ui.selectable_label(muted, mute_label).on_hover_text("Mute").clicked() {
                wpctl(vec!["set-mute".into(), id.to_string(), "toggle".into()]);
                self.volumes.lock().unwrap().insert(id, (polled, !muted));
            }
            let r = ui.add(egui::Slider::new(&mut v, 0.0..=1.5).fixed_decimals(2).trailing_fill(true));
            if r.changed() {
                let last = self.dragging.get(&id).map(|d| d.1);
                self.dragging.insert(id, (v, Instant::now()));
                if last.is_none_or(|t| t.elapsed() > Duration::from_millis(40)) || r.drag_stopped() {
                    wpctl(vec!["set-volume".into(), id.to_string(), format!("{v:.3}")]);
                }
            }
            if r.drag_stopped() {
                wpctl(vec!["set-volume".into(), id.to_string(), format!("{v:.3}")]);
            }
        });
        ui.add_space(4.0);
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        egui::Modal::new(egui::Id::new("dialog")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            match &mut dialog {
                Dialog::Trial { pause } => {
                    ui.heading("Try the new system");
                    ui.label(
                        "Routing switches to your graph now. Nothing permanent changes: \
                         stop the trial, or simply reboot, and everything is exactly as before.",
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("Pause during the trial (relaunched afterwards):").strong());
                    for (name, on) in pause.iter_mut() {
                        let why = KNOWN_CONFLICTS.iter().find(|c| c.0 == *name).map(|c| c.1).unwrap_or("");
                        ui.checkbox(on, *name).on_hover_text(why);
                        ui.label(RichText::new(why).small().weak());
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Start trial").clicked() {
                            if self.dirty() {
                                self.save();
                            }
                            let names: Vec<String> = pause.iter().filter(|p| p.1).map(|p| p.0.to_owned()).collect();
                            self.background(move || {
                                session::start_trial(&names)?;
                                Ok("Trial started".into())
                            });
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::Approve { disable } => {
                    ui.heading("Make it permanent");
                    ui.label(
                        "Installs the daemon to ~/.local/bin and starts it at login as a systemd user service. \
                         The programs ticked below won't autostart anymore; their autostart files are moved aside, \
                         not deleted. \"Revert\" undoes all of this.",
                    );
                    for (name, on) in disable.iter_mut() {
                        ui.checkbox(on, format!("Stop autostarting {name}"));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Approve").clicked() {
                            let names: Vec<String> = disable.iter().filter(|p| p.1).map(|p| p.0.to_owned()).collect();
                            self.background(move || {
                                session::approve(&names)?;
                                Ok("Approved: the new system now starts at login".into())
                            });
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::Revert => {
                    ui.heading("Revert to your old setup?");
                    ui.label("Stops and removes the service and restores the autostart entries it moved aside.");
                    ui.horizontal(|ui| {
                        if ui.button("Revert").clicked() {
                            self.background(|| {
                                session::revert()?;
                                Ok("Reverted to your old setup".into())
                            });
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
            }
        });
        if keep {
            self.dialog = Some(dialog);
        }
    }
}

/// A fresh graph: the usual virtual devices, ready to wire up.
fn starter_graph() -> Graph {
    use crate::model::{BusSpec, Channels, GraphNode};
    let bus = |id, x, y, name: &str, kind, channels| GraphNode {
        id,
        pos: [x, y],
        kind: NodeKind::Bus(BusSpec { name: name.into(), kind, channels }),
    };
    Graph {
        nodes: vec![
            bus(1, 300.0, 50.0, "system-in", BusKind::Playback, Channels::Stereo),
            bus(2, 300.0, 250.0, "communication-in", BusKind::Playback, Channels::Stereo),
            bus(3, 650.0, 450.0, "system-out", BusKind::Microphone, Channels::Mono),
            GraphNode { id: 4, pos: [0.0, 60.0], kind: NodeKind::OtherAppsPlayback },
            GraphNode { id: 5, pos: [1000.0, 450.0], kind: NodeKind::OtherAppsRecording },
        ],
        wires: vec![model::Wire::new(4, 1), model::Wire::new(3, 5)],
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        // Closing without saving: don't leave unsaved edits playing.
        if self.previewing {
            let _ = ipc::request(&Request::Reload);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
            self.save();
        }
        let live = self.live.lock().unwrap().clone();
        let info = self.daemon.lock().unwrap().clone();

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(3.0);
            self.status_bar(ui, &info);
            let toast = self.toast.lock().unwrap().clone();
            if let Some((msg, err, t)) = toast {
                if t.elapsed() < Duration::from_secs(6) {
                    let color = if err { Color32::from_rgb(230, 90, 80) } else { Color32::from_rgb(120, 190, 240) };
                    ui.label(RichText::new(msg).color(color));
                    ctx.request_repaint_after(Duration::from_secs(1));
                }
            }
            ui.add_space(3.0);
        });
        egui::Panel::right("mixer").default_size(300.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.mixer(ui, &live));
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let profile = self.current();
            let selected = self.effective_selected(&info);
            let plan = planner::plan(&profile, &live, &selected);
            let resolved = planner::resolve(&profile, &live);
            let shows_levels = profile.nodes.iter().any(|n| n.kind.filter().is_some());
            self.want_levels.store(shows_levels && info.status.is_some(), Ordering::Relaxed);
            let readings = self.readings.lock().unwrap().clone();
            let mut viewer = graph::Viewer {
                live: &live,
                resolved: &resolved,
                active_slots: &plan.active_slots,
                readings: &readings,
                running: info.status.is_some(),
                select: None,
                fit_to: std::mem::take(&mut self.fit_pending).then(|| ui.max_rect()),
                defaults: (plan.default_sink, plan.default_source),
                labels: &mut self.labels,
                labels_changed: false,
                changed: false,
            };
            SnarlWidget::new()
                .id(egui::Id::new("graph"))
                .style(self.style)
                .show(&mut self.snarl, &mut viewer, ui);
            let (labels_changed, select) = (viewer.labels_changed, viewer.select.take());
            if let Some((switch, pin)) = select {
                self.select(switch, pin);
            }
            // Labels are global (not per profile), so they save right away.
            if labels_changed
                && let Err(e) = model::save_labels(&self.labels)
            {
                self.notify(format!("saving labels: {e:#}"), true);
            }
            ui.put(
                egui::Rect::from_min_size(ui.max_rect().left_top() + egui::vec2(8.0, 6.0), egui::vec2(420.0, 18.0)),
                egui::Label::new(RichText::new("Right-click to add nodes · drag from out to in to connect").small().weak()),
            );
        });
        self.dialogs(&ctx);
        self.sync_preview(&ctx, info.status.is_some());
        debug_screenshot(&ctx);
    }
}

/// `BSP_SCREENSHOT=out.ppm bsp gui` saves a picture of this window (only) once it has
/// settled, then quits. For checking the UI without screenshotting the whole desktop.
fn debug_screenshot(ctx: &egui::Context) {
    let Some(path) = std::env::var_os("BSP_SCREENSHOT") else { return };
    let frame = ctx.cumulative_frame_nr();
    let at: u64 = std::env::var("BSP_SCREENSHOT_FRAME").ok().and_then(|f| f.parse().ok()).unwrap_or(40);
    if frame == at {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
    }
    let shot = ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    if let Some(img) = shot {
        let mut out = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
        for p in &img.pixels {
            out.extend_from_slice(&[p.r(), p.g(), p.b()]);
        }
        let _ = std::fs::write(path, out);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    ctx.request_repaint();
}
