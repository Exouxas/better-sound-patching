//! Native GUI (egui): node editor, mixer and trial/approve controls.
//!
//! The GUI never changes routing itself. It watches PipeWire read-only, edits profile files,
//! and asks the daemon to apply them.

mod graph;

use crate::engine::{self, Cmd, EngineConfig};
use crate::ipc::{self, Request, Response, Status};
use crate::live::Live;
use crate::model::{self, BusKind, NodeKind, Profile};
use crate::planner;
use crate::session::{self, KNOWN_CONFLICTS, PausedApp};
use anyhow::{Result, anyhow};
use egui::{Color32, RichText};
use egui_snarl::Snarl;
use egui_snarl::ui::{NodeLayout, PinPlacement, SnarlStyle, SnarlWidget};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 860.0])
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
}

enum Dialog {
    NewProfile { name: String, duplicate: bool },
    DeleteProfile,
    Unsaved { target: String },
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

    profiles: Vec<String>,
    editing: Option<String>,
    saved: Profile,
    snarl: Snarl<NodeKind>,
    style: SnarlStyle,
    dialog: Option<Dialog>,
    dragging: HashMap<u32, (f32, Instant)>,
    labels: model::Labels,
    /// Last unsaved version of the active profile sent to the daemon for live preview.
    preview_sent: Option<Profile>,
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
            profiles: vec![],
            editing: None,
            saved: Profile::default(),
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
        app.refresh_profiles();
        let first = model::load_state()
            .active_profile
            .filter(|p| app.profiles.contains(p))
            .or_else(|| app.profiles.first().cloned());
        if let Some(p) = first {
            app.open_profile(&p);
        }
        app
    }

    fn refresh_profiles(&mut self) {
        self.profiles = model::list_profiles();
    }

    fn open_profile(&mut self, name: &str) {
        self.drop_preview();
        match model::load_profile(name) {
            Ok(p) => {
                self.snarl = graph::to_snarl(&p);
                // Compare against the normalized form (node ids are renumbered on load).
                self.saved = graph::from_snarl(&self.snarl);
                self.editing = Some(name.to_owned());
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn current(&self) -> Profile {
        graph::from_snarl(&self.snarl)
    }

    fn dirty(&self) -> bool {
        self.editing.is_some() && self.current() != self.saved
    }

    fn notify(&self, msg: impl Into<String>, error: bool) {
        *self.toast.lock().unwrap() = Some((msg.into(), error, Instant::now()));
    }

    fn save(&mut self) {
        let Some(name) = self.editing.clone() else { return };
        let profile = self.current();
        let dupes = duplicate_bus_names(&profile);
        if !dupes.is_empty() {
            self.notify(format!("Two virtual devices or EQs share a name: {}", dupes.join(", ")), true);
            return;
        }
        match model::save_profile(&name, &profile) {
            Ok(()) => {
                self.saved = profile;
                self.preview_sent = None;
                self.previewing = false;
                let status = self.daemon.lock().unwrap().status.clone();
                if status.and_then(|s| s.active_profile).as_deref() == Some(name.as_str()) {
                    match ipc::request(&Request::Reload) {
                        Ok(Response::Error(e)) => self.notify(e, true),
                        Err(e) => self.notify(format!("{e:#}"), true),
                        _ => self.notify(format!("Saved and applied {name}"), false),
                    }
                } else {
                    self.notify(format!("Saved {name}"), false);
                }
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn activate(&mut self, name: &str) {
        if self.daemon.lock().unwrap().status.is_some() {
            match ipc::request(&Request::Switch { profile: name.to_owned() }) {
                Ok(Response::Error(e)) => self.notify(e, true),
                Err(e) => self.notify(format!("{e:#}"), true),
                _ => self.notify(format!("Switched to {name}"), false),
            }
        } else {
            let _ = model::save_state(&model::AppState { active_profile: Some(name.to_owned()) });
            self.notify(format!("{name} will be used when the new system runs"), false);
        }
    }

    /// Edits to the active profile play live while the new system runs (about 30 updates a
    /// second while dragging). Save makes them permanent; leaving without saving reverts them.
    fn sync_preview(&mut self, ctx: &egui::Context, active: Option<&str>, daemon_running: bool) {
        if !daemon_running || self.editing.is_none() || active != self.editing.as_deref() {
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

    /// Throw away live, unsaved edits: the daemon goes back to the saved profile.
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

    fn top_bar(&mut self, ui: &mut egui::Ui, active: Option<String>) {
        ui.horizontal(|ui| {
            ui.heading("Better Sound Patching");
            ui.separator();
            ui.label("Profile");
            let current = self.editing.clone().unwrap_or_else(|| "—".into());
            let mut pick = None;
            egui::ComboBox::from_id_salt("profile")
                .selected_text(&current)
                .width(160.0)
                .show_ui(ui, |ui| {
                    for p in &self.profiles {
                        let label = if Some(p) == active.as_ref() { format!("★ {p}") } else { p.clone() };
                        if ui.selectable_label(Some(p) == self.editing.as_ref(), label).clicked() {
                            pick = Some(p.clone());
                        }
                    }
                });
            if let Some(p) = pick.filter(|p| Some(p) != self.editing.as_ref()) {
                if self.dirty() {
                    self.dialog = Some(Dialog::Unsaved { target: p });
                } else {
                    self.open_profile(&p);
                }
            }
            if ui.button("New").clicked() {
                self.dialog = Some(Dialog::NewProfile { name: String::new(), duplicate: false });
            }
            if ui.add_enabled(self.editing.is_some(), egui::Button::new("Duplicate")).clicked() {
                self.dialog = Some(Dialog::NewProfile { name: String::new(), duplicate: true });
            }
            if ui.add_enabled(self.editing.is_some(), egui::Button::new("Delete")).clicked() {
                self.dialog = Some(Dialog::DeleteProfile);
            }
            ui.separator();
            let dirty = self.dirty();
            if ui.add_enabled(dirty, egui::Button::new(if dirty { "Save*" } else { "Saved" })).on_hover_text("Ctrl+S").clicked() {
                self.save();
            }
            if let Some(name) = self.editing.clone() {
                if active.as_deref() == Some(name.as_str()) {
                    ui.label(RichText::new("★ active").color(Color32::from_rgb(240, 200, 80)));
                } else if ui.button("★ Make active").on_hover_text("Switch routing to this profile (instant)").clicked() {
                    if dirty {
                        self.save();
                    }
                    self.activate(&name);
                }
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, info: &DaemonInfo) {
        let busy = *self.busy.lock().unwrap();
        ui.horizontal(|ui| {
            match (&info.status, info.approved) {
                (Some(s), true) => {
                    ui.label(RichText::new("✔ New system active").color(Color32::from_rgb(80, 200, 120)));
                    ui.label(format!("— starts at login · profile {}", s.active_profile.as_deref().unwrap_or("none")));
                    if ui.add_enabled(!busy, egui::Button::new("Revert to old setup…")).clicked() {
                        self.dialog = Some(Dialog::Revert);
                    }
                }
                (Some(s), false) => {
                    ui.label(RichText::new("⚠ Trial running — not permanent").color(Color32::from_rgb(230, 170, 60)));
                    ui.label(format!(
                        "— profile {} · {} links · nothing permanent changed{}",
                        s.active_profile.as_deref().unwrap_or("none"),
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
            ui.label(RichText::new("No virtual devices in this profile are running yet.").weak());
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
                Dialog::NewProfile { name, duplicate } => {
                    ui.heading(if *duplicate { "Duplicate profile" } else { "New profile" });
                    let r = ui.text_edit_singleline(name);
                    r.request_focus();
                    let valid = model::valid_profile_name(name) && !self.profiles.contains(name);
                    ui.horizontal(|ui| {
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.add_enabled(valid, egui::Button::new("Create")).clicked() || (enter && valid) {
                            let profile = if *duplicate { self.current() } else { starter_profile() };
                            match model::save_profile(name, &profile) {
                                Ok(()) => {
                                    self.refresh_profiles();
                                    self.open_profile(&name.clone());
                                }
                                Err(e) => self.notify(format!("{e:#}"), true),
                            }
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::DeleteProfile => {
                    let name = self.editing.clone().unwrap_or_default();
                    ui.heading(format!("Delete {name}?"));
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            if let Err(e) = model::delete_profile(&name) {
                                self.notify(format!("{e:#}"), true);
                            }
                            self.refresh_profiles();
                            self.editing = None;
                            self.snarl = Snarl::new();
                            self.saved = Profile::default();
                            if let Some(p) = self.profiles.first().cloned() {
                                self.open_profile(&p);
                            }
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::Unsaved { target } => {
                    ui.heading("Unsaved changes");
                    ui.label(format!("Save changes to {} first?", self.editing.clone().unwrap_or_default()));
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            self.save();
                            self.open_profile(&target.clone());
                            keep = false;
                        }
                        if ui.button("Discard").clicked() {
                            self.open_profile(&target.clone());
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::Trial { pause } => {
                    ui.heading("Try the new system");
                    ui.label(
                        "Routing switches to your active profile now. Nothing permanent changes: \
                         stop the trial, or simply reboot, and everything is exactly as before.",
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("Pause during the trial (relaunched afterwards):").strong());
                    for (name, on) in pause.iter_mut() {
                        let why = KNOWN_CONFLICTS.iter().find(|c| c.0 == *name).map(|c| c.1).unwrap_or("");
                        ui.checkbox(on, *name).on_hover_text(why);
                        ui.label(RichText::new(why).small().weak());
                    }
                    if model::load_state().active_profile.is_none() {
                        ui.label(RichText::new("No profile is active yet — the current one will be used.").color(Color32::from_rgb(230, 170, 60)));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Start trial").clicked() {
                            if self.dirty() {
                                self.save();
                            }
                            if model::load_state().active_profile.is_none()
                                && let Some(p) = self.editing.clone()
                            {
                                let _ = model::save_state(&model::AppState { active_profile: Some(p) });
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

/// A fresh profile: the usual virtual devices plus fallbacks, ready to wire up.
fn starter_profile() -> Profile {
    use crate::model::{BusSpec, Channels, GraphNode};
    let bus = |id, x, y, name: &str, kind, channels| GraphNode {
        id,
        pos: [x, y],
        kind: NodeKind::Bus(BusSpec { name: name.into(), kind, channels }),
    };
    Profile {
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

fn duplicate_bus_names(p: &Profile) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut dupes = vec![];
    for n in &p.nodes {
        let name = match &n.kind {
            NodeKind::Bus(b) => &b.name,
            NodeKind::Eq(e) => &e.name,
            _ => continue,
        };
        if !seen.insert(name.clone()) {
            dupes.push(name.clone());
        }
    }
    dupes
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
        let active = info
            .status
            .as_ref()
            .and_then(|s| s.active_profile.clone())
            .or_else(|| model::load_state().active_profile);

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui, active.clone());
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
            if self.editing.is_none() {
                ui.centered_and_justified(|ui| ui.label("Create a profile to get started (New)."));
                return;
            }
            let profile = self.current();
            let plan = planner::plan(&profile, &live);
            let resolved = planner::resolve(&profile, &live);
            let mut viewer = graph::Viewer {
                live: &live,
                resolved: &resolved,
                active_slots: &plan.active_slots,
                defaults: (plan.default_sink, plan.default_source),
                labels: &mut self.labels,
                labels_changed: false,
                changed: false,
            };
            SnarlWidget::new()
                .id(egui::Id::new("graph"))
                .style(self.style)
                .show(&mut self.snarl, &mut viewer, ui);
            // Labels are global (not per profile), so they save right away.
            if viewer.labels_changed
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
        self.sync_preview(&ctx, active.as_deref(), info.status.is_some());
        debug_screenshot(&ctx);
    }
}

/// `BSP_SCREENSHOT=out.ppm bsp gui` saves a picture of this window (only) once it has
/// settled, then quits. For checking the UI without screenshotting the whole desktop.
fn debug_screenshot(ctx: &egui::Context) {
    let Some(path) = std::env::var_os("BSP_SCREENSHOT") else { return };
    let frame = ctx.cumulative_frame_nr();
    if frame == 40 {
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
