//! The tray icon: scroll over it to change the master volume, click it to open the editor.

use crate::engine::Cmd;
use crate::ipc::Request;
use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{MenuItem, Orientation, ToolTip};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};

/// One scroll notch (the host reports 120 per notch) changes the master by this much.
const STEP: f32 = 0.05;

pub struct Tray {
    master: f32,
    /// Scroll not yet turned into a step (touchpads send small amounts).
    scrolled: i32,
    engine: pipewire::channel::Sender<Cmd>,
    /// The master volume the icon shows, as `f32` bits. Shared with the updater thread so a
    /// change the tray made itself isn't sent back to it.
    shown: Arc<AtomicU32>,
}

/// Show the tray icon. Returns `None` if there's no session bus to put it on; otherwise a
/// function that shows a new master volume on the icon.
///
/// The daemon can start before the desktop's tray does (a user service that comes up before
/// Plasma), so a missing tray isn't an error: the icon waits and registers once the tray
/// appears, and again if the tray restarts.
///
/// The function never waits: updating a ksni tray blocks until its D-Bus service has handled
/// the update, and that can hang while the service is busy (a scroll that came in at the same
/// moment). A dedicated thread does the update instead, so the caller (the PipeWire loop) can
/// never get stuck behind the tray.
pub fn spawn(engine: pipewire::channel::Sender<Cmd>) -> Option<Box<dyn Fn(f32)>> {
    let shown = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    let tray = Tray { master: 1.0, scrolled: 0, engine, shown: shown.clone() };
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(h) => h,
        Err(e) => {
            log::warn!("no tray icon: {e}");
            return None;
        }
    };
    let (tx, rx) = mpsc::channel::<f32>();
    let spawned = std::thread::Builder::new().name("tray-update".into()).spawn(move || {
        while let Ok(mut master) = rx.recv() {
            // Only the latest value matters.
            while let Ok(newer) = rx.try_recv() {
                master = newer;
            }
            if shown.load(Ordering::Relaxed) == master.to_bits() {
                continue;
            }
            shown.store(master.to_bits(), Ordering::Relaxed);
            handle.update(|t| t.master = master);
        }
    });
    if let Err(e) = spawned {
        log::warn!("no tray icon: {e}");
        return None;
    }
    Some(Box::new(move |m| {
        let _ = tx.send(m);
    }))
}

fn open() {
    if let Err(e) = crate::session::open_editor(ksni::current_xdg_activation_token()) {
        log::warn!("opening the editor: {e:#}");
    }
}

impl Tray {
    fn set_master(&mut self, v: f32) {
        let v = (v.clamp(0.0, 1.0) * 100.0).round() / 100.0;
        if v == self.master {
            return;
        }
        self.master = v;
        self.shown.store(v.to_bits(), Ordering::Relaxed);
        let (reply, _) = mpsc::channel();
        let _ = self.engine.send(Cmd::Ipc(Request::Master { volume: v }, reply));
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        crate::hotkeys::APP_ID.into()
    }

    fn title(&self) -> String {
        "Better Sound Patching".into()
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::info!("tray not available yet, the icon will appear when it is ({reason:?})");
        true
    }

    fn watcher_online(&self) {
        log::info!("tray is back, showing the icon");
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        [16, 22, 24, 32, 48, 64]
            .into_iter()
            .map(|size| ksni::Icon { width: size as i32, height: size as i32, data: icon(size, self.master) })
            .collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Better Sound Patching".into(),
            description: format!("Master volume {:.0}% · scroll to change", self.master * 100.0),
            ..Default::default()
        }
    }

    /// Click: close the editor if it's open, else open it.
    fn activate(&mut self, _x: i32, _y: i32) {
        if crate::gui::single::ask(crate::gui::single::Ask::Close) == crate::gui::single::Answer::Nobody {
            open();
        }
    }

    fn scroll(&mut self, delta: i32, orientation: Orientation) {
        if !matches!(orientation, Orientation::Vertical) {
            return;
        }
        self.scrolled += delta;
        let steps = self.scrolled / 120;
        if steps != 0 {
            self.scrolled -= steps * 120;
            self.set_master(self.master + steps as f32 * STEP);
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open editor".into(),
                icon_name: "audio-card".into(),
                activate: Box::new(|_: &mut Self| {
                    if crate::gui::single::ask(crate::gui::single::Ask::Show) != crate::gui::single::Answer::Done {
                        open();
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: format!("Master volume {:.0}%", self.master * 100.0),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if self.master > 0.0 { "Mute".into() } else { "Unmute (100%)".into() },
                activate: Box::new(|t: &mut Self| {
                    let v = if t.master > 0.0 { 0.0 } else { 1.0 };
                    t.set_master(v);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Our own icon, so it isn't mistaken for the desktop's volume icon: the app's patch-cable
/// logo in a rounded square, with the master volume as a bar along the bottom. Grey when muted.
/// ARGB32 rows, big-endian, as the tray expects.
pub fn icon(size: u32, master: f32) -> Vec<u8> {
    type P = (f32, f32);
    const GREEN: [f32; 3] = [80.0, 200.0, 120.0];
    const GREY: [f32; 3] = [150.0, 155.0, 160.0];
    let muted = master <= 0.0;
    let ink = if muted { GREY } else { GREEN };

    fn seg_dist(p: P, a: P, b: P) -> f32 {
        let (abx, aby) = (b.0 - a.0, b.1 - a.1);
        let t = (((p.0 - a.0) * abx + (p.1 - a.1) * aby) / (abx * abx + aby * aby)).clamp(0.0, 1.0);
        ((p.0 - a.0 - t * abx).powi(2) + (p.1 - a.1 - t * aby).powi(2)).sqrt()
    }
    /// Signed distance to a rounded rectangle (centre, half size, corner radius).
    fn round_rect(p: P, c: P, half: P, r: f32) -> f32 {
        let (qx, qy) = ((p.0 - c.0).abs() - half.0 + r, (p.1 - c.1).abs() - half.1 + r);
        (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
    }
    // The cable: up from the left socket, over the top, down to the right socket.
    let (a, b) = ((0.25, 0.58), (0.75, 0.58));
    let bez = |t: f32, p0: P, p1: P, p2: P, p3: P| -> P {
        let u = 1.0 - t;
        (
            u * u * u * p0.0 + 3.0 * u * u * t * p1.0 + 3.0 * u * t * t * p2.0 + t * t * t * p3.0,
            u * u * u * p0.1 + 3.0 * u * u * t * p1.1 + 3.0 * u * t * t * p2.1 + t * t * t * p3.1,
        )
    };
    let mut cable: Vec<P> = (0..=16).map(|i| bez(i as f32 / 16.0, a, (0.38, 0.58), (0.38, 0.24), (0.5, 0.24))).collect();
    cable.extend((1..=16).map(|i| bez(i as f32 / 16.0, (0.5, 0.24), (0.62, 0.24), (0.62, 0.58), b)));
    let bar = (0.22, 0.78, 0.735, 0.835); // x0, x1, y0, y1
    let level_x = bar.0 + (bar.1 - bar.0) * master.clamp(0.0, 1.0);

    // Layers, bottom to top: (colour, opacity, is this sample inside?).
    let layers: [([f32; 3], f32, &dyn Fn(P) -> bool); 6] = [
        (ink, 0.16, &|p| round_rect(p, (0.5, 0.5), (0.45, 0.45), 0.2) <= 0.0),
        (ink, 0.95, &|p| round_rect(p, (0.5, 0.5), (0.45, 0.45), 0.2).abs() <= 0.035),
        (GREY, 0.45, &|p| round_rect(p, (0.5, (bar.2 + bar.3) / 2.0), ((bar.1 - bar.0) / 2.0, (bar.3 - bar.2) / 2.0), 0.05) <= 0.0),
        (ink, 1.0, &|p| p.0 <= level_x && round_rect(p, (0.5, (bar.2 + bar.3) / 2.0), ((bar.1 - bar.0) / 2.0, (bar.3 - bar.2) / 2.0), 0.05) <= 0.0),
        (ink, 1.0, &|p| cable.windows(2).any(|w| seg_dist(p, w[0], w[1]) <= 0.045)),
        (ink, 1.0, &|p| seg_dist(p, a, a) <= 0.085 || seg_dist(p, b, b) <= 0.085),
    ];

    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut r, mut g, mut bl, mut al) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for (color, opacity, inside) in &layers {
                let mut hits = 0;
                for sy in 0..SS {
                    for sx in 0..SS {
                        let p = ((x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32, (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32);
                        if inside(p) {
                            hits += 1;
                        }
                    }
                }
                let src = opacity * hits as f32 / (SS * SS) as f32;
                if src <= 0.0 {
                    continue;
                }
                // "Over", in straight alpha.
                let out_a = src + al * (1.0 - src);
                let mix = |c: f32, d: f32| (c * src + d * al * (1.0 - src)) / out_a;
                (r, g, bl) = (mix(color[0], r), mix(color[1], g), mix(color[2], bl));
                al = out_a;
            }
            out.extend_from_slice(&[(al * 255.0).round() as u8, r.round() as u8, g.round() as u8, bl.round() as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn icon_has_the_right_size_and_isnt_empty() {
        for size in [16, 22, 64] {
            let data = super::icon(size, 0.6);
            assert_eq!(data.len(), (size * size * 4) as usize);
            assert!(data.chunks(4).any(|px| px[0] == 255), "some pixels are opaque");
            assert!(data.chunks(4).any(|px| px[0] == 0), "corners are transparent");
        }
    }
}
