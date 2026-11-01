//! A native EQ as a PipeWire filter node, running inside this process.
//!
//! pipewire-rs doesn't wrap `pw_filter`, so this is a small unsafe layer over the C API.
//! Audio is processed on PipeWire's realtime thread. Settings reach it lock-free in practice:
//! the main thread bumps a generation counter, and the realtime thread only `try_lock`s
//! (never blocks) to copy the new settings when the counter changed.

use crate::dsp::{Chain, EqParams};
use crate::model::{Channels, EqSpec};
use anyhow::{Result, bail};
use pipewire as pw;
use pw::spa::sys as spa_sys;
use pw::sys as pw_sys;
use std::ffi::{CString, c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct Shared {
    generation: AtomicU64,
    params: Mutex<EqParams>,
}

/// Owned by the realtime thread once connected.
struct Rt {
    ins: Vec<*mut c_void>,
    outs: Vec<*mut c_void>,
    chains: Vec<Chain>,
    shared: Arc<Shared>,
    generation: u64,
    rate: u32,
    params: EqParams,
}

impl Rt {
    fn refresh(&mut self, rate: u32) {
        let generation = self.shared.generation.load(Ordering::Acquire);
        let mut retune = rate != self.rate;
        if generation != self.generation
            && let Ok(p) = self.shared.params.try_lock()
        {
            self.params = *p;
            self.generation = generation;
            retune = true;
        }
        if retune {
            self.rate = rate;
            for c in &mut self.chains {
                c.set(&self.params, rate as f64);
            }
        }
    }
}

unsafe extern "C" fn on_process(data: *mut c_void, position: *mut spa_sys::spa_io_position) {
    // SAFETY: `data` is the Box<Rt> we registered; only this thread touches it after connect.
    let rt = unsafe { &mut *(data as *mut Rt) };
    if position.is_null() {
        return;
    }
    let (n, rate) = unsafe { ((*position).clock.duration as u32, (*position).clock.rate.denom) };
    rt.refresh(if rate == 0 { 48000 } else { rate });
    for ch in 0..rt.outs.len() {
        unsafe {
            let out = pw_sys::pw_filter_get_dsp_buffer(rt.outs[ch], n) as *mut f32;
            if out.is_null() {
                continue;
            }
            let out = std::slice::from_raw_parts_mut(out, n as usize);
            let inp = pw_sys::pw_filter_get_dsp_buffer(rt.ins[ch], n) as *const f32;
            if inp.is_null() {
                out.fill(0.0);
                continue;
            }
            let inp = std::slice::from_raw_parts(inp, n as usize);
            rt.chains[ch].process(inp, out);
        }
    }
}

static EVENTS: pw_sys::pw_filter_events = pw_sys::pw_filter_events {
    version: pw_sys::PW_VERSION_FILTER_EVENTS,
    destroy: None,
    state_changed: None,
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
};

pub struct EqNode {
    filter: *mut pw_sys::pw_filter,
    _hook: Box<spa_sys::spa_hook>,
    _rt: Box<Rt>,
    shared: Arc<Shared>,
    pub spec: EqSpec,
}

fn props(pairs: &[(&str, &str)]) -> *mut pw_sys::pw_properties {
    unsafe {
        let p = pw_sys::pw_properties_new(std::ptr::null::<c_char>());
        for (k, v) in pairs {
            let (k, v) = (CString::new(*k).unwrap(), CString::new(*v).unwrap_or_default());
            pw_sys::pw_properties_set(p, k.as_ptr(), v.as_ptr());
        }
        p
    }
}

impl EqNode {
    pub fn new(core: &pw::core::CoreRc, spec: &EqSpec) -> Result<EqNode> {
        let channels: &[&str] = match spec.channels {
            Channels::Mono => &["MONO"],
            Channels::Stereo => &["FL", "FR"],
        };
        let description = format!("EQ {}", spec.name);
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(1),
            params: Mutex::new(EqParams::from_spec(spec.preamp_db, &spec.bands)),
        });
        let rt = Box::new(Rt {
            ins: vec![],
            outs: vec![],
            chains: vec![Chain::default(); channels.len()],
            shared: shared.clone(),
            generation: 0,
            rate: 0,
            params: EqParams::from_spec(0.0, &[]),
        });
        let name = CString::new(spec.name.as_str())?;
        unsafe {
            let filter = pw_sys::pw_filter_new(
                core.as_raw_ptr(),
                name.as_ptr(),
                props(&[
                    ("media.type", "Audio"),
                    ("media.category", "Filter"),
                    ("media.role", "DSP"),
                    ("node.name", &spec.name),
                    ("node.description", &description),
                    ("node.autoconnect", "false"),
                ]),
            );
            if filter.is_null() {
                bail!("could not create filter {}", spec.name);
            }
            // From here on `node` owns the filter, so every early return cleans it up.
            let mut node = EqNode { filter, _hook: Box::new(std::mem::zeroed()), _rt: rt, shared, spec: spec.clone() };
            for ch in channels {
                for (dir, prefix) in [(spa_sys::SPA_DIRECTION_INPUT, "input"), (spa_sys::SPA_DIRECTION_OUTPUT, "output")] {
                    let port = pw_sys::pw_filter_add_port(
                        filter,
                        dir,
                        pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                        0,
                        props(&[
                            ("format.dsp", "32 bit float mono audio"),
                            ("port.name", &format!("{prefix}_{ch}")),
                            ("audio.channel", ch),
                        ]),
                        std::ptr::null_mut(),
                        0,
                    );
                    if port.is_null() {
                        bail!("could not add {prefix}_{ch} to {}", spec.name);
                    }
                    if dir == spa_sys::SPA_DIRECTION_INPUT {
                        node._rt.ins.push(port);
                    } else {
                        node._rt.outs.push(port);
                    }
                }
            }
            // The Rt box never moves (it's boxed), so handing out its address is fine.
            let data = &mut *node._rt as *mut Rt as *mut c_void;
            pw_sys::pw_filter_add_listener(filter, &mut *node._hook, &EVENTS, data);
            let res = pw_sys::pw_filter_connect(
                filter,
                pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
                std::ptr::null_mut(),
                0,
            );
            if res < 0 {
                bail!("could not connect filter {} ({res})", spec.name);
            }
            Ok(node)
        }
    }

    /// Can this node take `spec` by retuning, or does it need rebuilding?
    pub fn compatible(&self, spec: &EqSpec) -> bool {
        self.spec.name == spec.name && self.spec.channels == spec.channels
    }

    /// Retune live. Takes effect on the next audio cycle.
    pub fn update(&mut self, spec: &EqSpec) {
        if self.spec == *spec {
            return;
        }
        *self.shared.params.lock().unwrap() = EqParams::from_spec(spec.preamp_db, &spec.bands);
        self.shared.generation.fetch_add(1, Ordering::Release);
        self.spec = spec.clone();
    }
}

impl Drop for EqNode {
    fn drop(&mut self) {
        // Destroying the filter stops processing (synchronously with the data thread) and
        // removes our listener, so the Rt box and hook can be freed afterwards.
        unsafe { pw_sys::pw_filter_destroy(self.filter) };
    }
}
