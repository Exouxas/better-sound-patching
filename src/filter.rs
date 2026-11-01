//! Processing nodes (EQ, levels, volume) as PipeWire filter nodes, running inside this process.
//!
//! pipewire-rs doesn't wrap `pw_filter`, so this is a small unsafe layer over the C API.
//! Audio is processed on PipeWire's realtime thread. Settings reach it lock-free in practice:
//! the main thread bumps a generation counter, and the realtime thread only `try_lock`s
//! (never blocks) to copy the new settings when the counter changed.

use crate::dsp::{MAX_CH, Meter, Params, Processor, Reading};
use crate::model::{Channels, FilterSpec};
use anyhow::{Result, bail};
use pipewire as pw;
use pw::spa::sys as spa_sys;
use pw::sys as pw_sys;
use std::ffi::{CString, c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct Shared {
    generation: AtomicU64,
    params: Mutex<Params>,
    meter: Meter,
}

/// Largest buffer we keep scratch space for. PipeWire quantums are far smaller.
const SCRATCH: usize = 8192;

/// Owned by the realtime thread once connected.
struct Rt {
    ins: Vec<*mut c_void>,
    outs: Vec<*mut c_void>,
    proc: Processor,
    shared: Arc<Shared>,
    generation: u64,
    rate: u32,
    params: Params,
    /// Stand-ins for missing buffers (silence in, discarded out).
    zeros: Vec<f32>,
    scratch: [Vec<f32>; MAX_CH],
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
            self.proc.set(self.params, rate as f32);
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
    let (n, ch) = (n as usize, rt.outs.len().min(MAX_CH));
    if n > SCRATCH {
        return;
    }
    let rt = &mut *rt;
    let mut ins: [&[f32]; MAX_CH] = [&[]; MAX_CH];
    let mut outs: [&mut [f32]; MAX_CH] = [&mut [], &mut []];
    let mut scratch = rt.scratch.iter_mut();
    for c in 0..ch {
        // SAFETY: PipeWire hands out buffers of at least `n` floats, valid for this cycle.
        unsafe {
            let inp = pw_sys::pw_filter_get_dsp_buffer(rt.ins[c], n as u32) as *const f32;
            ins[c] = if inp.is_null() { &rt.zeros[..n] } else { std::slice::from_raw_parts(inp, n) };
            let out = pw_sys::pw_filter_get_dsp_buffer(rt.outs[c], n as u32) as *mut f32;
            let spare = scratch.next().unwrap();
            outs[c] = if out.is_null() { &mut spare[..n] } else { std::slice::from_raw_parts_mut(out, n) };
        }
    }
    rt.proc.process(&ins[..ch], &mut outs[..ch], &rt.shared.meter);
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

pub struct FilterNode {
    filter: *mut pw_sys::pw_filter,
    _hook: Box<spa_sys::spa_hook>,
    _rt: Box<Rt>,
    shared: Arc<Shared>,
    pub spec: FilterSpec,
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

impl FilterNode {
    pub fn new(core: &pw::core::CoreRc, spec: &FilterSpec) -> Result<FilterNode> {
        let channels: &[&str] = match spec.channels() {
            Channels::Mono => &["MONO"],
            Channels::Stereo => &["FL", "FR"],
        };
        let what = match spec {
            FilterSpec::Eq(_) => "EQ",
            FilterSpec::Levels(_) => "Levels",
            FilterSpec::Volume(_) => "Volume",
        };
        let description = format!("{what} {}", spec.name());
        let params = Params::from_spec(spec);
        let shared = Arc::new(Shared { generation: AtomicU64::new(1), params: Mutex::new(params), meter: Meter::new() });
        let rt = Box::new(Rt {
            ins: vec![],
            outs: vec![],
            proc: Processor::new(params),
            shared: shared.clone(),
            generation: 0,
            rate: 0,
            params,
            zeros: vec![0.0; SCRATCH],
            scratch: [vec![0.0; SCRATCH], vec![0.0; SCRATCH]],
        });
        let name = CString::new(spec.name())?;
        unsafe {
            let filter = pw_sys::pw_filter_new(
                core.as_raw_ptr(),
                name.as_ptr(),
                props(&[
                    ("media.type", "Audio"),
                    ("media.category", "Filter"),
                    ("media.role", "DSP"),
                    ("node.name", spec.name()),
                    ("node.description", &description),
                    ("node.autoconnect", "false"),
                ]),
            );
            if filter.is_null() {
                bail!("could not create filter {}", spec.name());
            }
            // From here on `node` owns the filter, so every early return cleans it up.
            let mut node = FilterNode { filter, _hook: Box::new(std::mem::zeroed()), _rt: rt, shared, spec: spec.clone() };
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
                        bail!("could not add {prefix}_{ch} to {}", spec.name());
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
                bail!("could not connect filter {} ({res})", spec.name());
            }
            Ok(node)
        }
    }

    /// Can this node take `spec` by retuning, or does it need rebuilding?
    pub fn compatible(&self, spec: &FilterSpec) -> bool {
        self.spec.compatible(spec)
    }

    /// Levels since the last call.
    pub fn reading(&self) -> Reading {
        self.shared.meter.take()
    }

    /// Retune live. Takes effect on the next audio cycle.
    pub fn update(&mut self, spec: &FilterSpec) {
        if self.spec == *spec {
            return;
        }
        *self.shared.params.lock().unwrap() = Params::from_spec(spec);
        self.shared.generation.fetch_add(1, Ordering::Release);
        self.spec = spec.clone();
    }
}

impl Drop for FilterNode {
    fn drop(&mut self) {
        // Destroying the filter stops processing (synchronously with the data thread) and
        // removes our listener, so the Rt box and hook can be freed afterwards.
        unsafe { pw_sys::pw_filter_destroy(self.filter) };
    }
}
