//! The tray icon: scroll over it to change the master volume, click it to open the editor.

use crate::engine::Cmd;
use crate::ipc::Request;
use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{MenuItem, Orientation, ToolTip};
use std::sync::mpsc;

/// One scroll notch (the host reports 120 per notch) changes the master by this much.
const STEP: f32 = 0.05;

pub struct Tray {
    master: f32,
    /// Scroll not yet turned into a step (touchpads send small amounts).
    scrolled: i32,
    engine: pipewire::channel::Sender<Cmd>,
}

pub type Handle = ksni::blocking::Handle<Tray>;

/// Show the tray icon. Returns `None` if the desktop has no tray.
pub fn spawn(engine: pipewire::channel::Sender<Cmd>) -> Option<Handle> {
    let tray = Tray { master: 1.0, scrolled: 0, engine };
    match tray.spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            log::warn!("no tray icon: {e}");
            None
        }
    }
}

/// Tell the tray the master volume changed elsewhere (the editor, or at start).
pub fn show_master(handle: &Handle, master: f32) {
    handle.update(|t| t.master = master);
}

impl Tray {
    fn set_master(&mut self, v: f32) {
        let v = (v.clamp(0.0, 1.0) * 100.0).round() / 100.0;
        if v == self.master {
            return;
        }
        self.master = v;
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

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn icon_name(&self) -> String {
        match self.master {
            m if m <= 0.0 => "audio-volume-muted",
            m if m < 0.34 => "audio-volume-low",
            m if m < 0.67 => "audio-volume-medium",
            _ => "audio-volume-high",
        }
        .into()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Better Sound Patching".into(),
            description: format!("Master volume {:.0}% · scroll to change", self.master * 100.0),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        if let Err(e) = crate::session::open_editor() {
            log::warn!("opening the editor: {e:#}");
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
                activate: Box::new(|t: &mut Self| t.activate(0, 0)),
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
