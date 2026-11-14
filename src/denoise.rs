//! Noise suppression with a neural network: DPDFNet-2 (see `dpdfnet.rs`), DeepFilterNet 3
//! (`deep-filter-rt`, MIT or Apache-2.0, model embedded) or RNNoise 0.2 (Xiph's `librnnoise`, BSD-3-Clause, the system library with the
//! models retrained in 2024). Both work on 10 ms frames of 48 kHz
//! audio, so audio is gathered into frames first, which adds one frame of delay on top of the
//! model's own.
//!
//! Everything is set up in [`Denoiser::new`] (off the realtime thread). RNNoise then runs without
//! allocating; DeepFilterNet's inference library may allocate small buffers per frame, which
//! costs far less than a frame lasts (about 0.15 ms per 10 ms on a desktop CPU).
//!
//! An optional voice gate ([`VoiceGate`]) silences the output between words. It goes by RNNoise's
//! guess, per frame, of whether someone is speaking; with the other models a separate RNNoise
//! runs just for that guess (about 0.04 ms per frame).

use crate::dpdfnet;
use crate::dsp::MAX_CH;
use crate::model::{DENOISE_FULL_DB, DenoiseModel};
use deep_filter_rt::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::Array2;
use std::ptr::NonNull;

/// The system's `librnnoise` (0.2 or newer).
mod ffi {
    use std::ffi::{c_int, c_void};

    #[repr(C)]
    pub struct DenoiseState {
        _private: [u8; 0],
    }

    #[link(name = "rnnoise")]
    unsafe extern "C" {
        pub fn rnnoise_get_frame_size() -> c_int;
        /// A null model means the built-in one.
        pub fn rnnoise_create(model: *mut c_void) -> *mut DenoiseState;
        pub fn rnnoise_destroy(st: *mut DenoiseState);
        /// Returns the probability that the frame has voice in it.
        pub fn rnnoise_process_frame(st: *mut DenoiseState, out: *mut f32, input: *const f32) -> f32;
    }
}

/// One channel's RNNoise state.
struct RnNoise(NonNull<ffi::DenoiseState>);

// SAFETY: the state is plain memory owned by this value alone; it's only used from one thread at a time.
unsafe impl Send for RnNoise {}

impl RnNoise {
    fn new() -> anyhow::Result<RnNoise> {
        // SAFETY: plain calls into the library; a null model selects the built-in one.
        unsafe {
            let frame = ffi::rnnoise_get_frame_size();
            anyhow::ensure!(frame as usize == HOP, "librnnoise works on frames of {frame} samples, not {HOP}");
            NonNull::new(ffi::rnnoise_create(std::ptr::null_mut())).map(RnNoise).ok_or_else(|| anyhow::anyhow!("rnnoise_create failed"))
        }
    }

    fn process(&mut self, out: &mut [f32; HOP], input: &[f32; HOP]) -> f32 {
        // SAFETY: both buffers hold exactly one frame, checked against the library in `new`.
        unsafe { ffi::rnnoise_process_frame(self.0.as_ptr(), out.as_mut_ptr(), input.as_ptr()) }
    }
}

impl Drop for RnNoise {
    fn drop(&mut self) {
        // SAFETY: made by rnnoise_create and freed only here.
        unsafe { ffi::rnnoise_destroy(self.0.as_ptr()) }
    }
}

/// Samples per frame (10 ms at 48 kHz), the same for both models.
pub const HOP: usize = 480;
/// The only sample rate the models work at. At any other rate the audio passes unchanged.
pub const RATE: f32 = 48000.0;

/// The longest model delay: DPDFNet with the most slack.
const MAX_DELAY: usize = dpdfnet::MODEL_DELAY + dpdfnet::MAX_LAG * HOP;

/// RNNoise works on 16-bit sample values in floats.
const I16: f32 = 32768.0;

enum Engine {
    RnNoise(Vec<RnNoise>),
    DeepFilter { df: Box<DfTract>, noisy: Array2<f32>, enhanced: Array2<f32> },
    DpdfNet(Box<dpdfnet::Worker>),
}

/// Voice gate settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateParams {
    /// Frames RNNoise rates less likely than this to hold speech are silenced (0–1).
    pub threshold: f32,
    /// How long the gate stays open after the voice stops, in ms.
    pub hold_ms: f32,
}

/// Silences the output between words.
///
/// The voice guess is for the newest input frame, which is ahead of the output frame being made
/// by the model's delay. That gives the gate a look-ahead for free: it opens that much before
/// the voice arrives in the output, so the start of a word isn't cut, without adding delay.
struct VoiceGate {
    params: Option<GateParams>,
    /// Input frames seen, and the last one rated as voice.
    frame: u64,
    last_voice: Option<u64>,
    /// Current gain, 1 = open.
    gain: f32,
}

impl VoiceGate {
    /// Fades in over 5 ms and out over 50 ms.
    const OPEN_STEP: f32 = 1.0 / 240.0;
    const CLOSE_STEP: f32 = 1.0 / 2400.0;

    fn new() -> Self {
        VoiceGate { params: None, frame: 0, last_voice: None, gain: 1.0 }
    }

    /// One frame: `voice` is the guess for the newest input frame, which is `lead` frames ahead of
    /// `out`. Returns the lowest gain used.
    fn run(&mut self, voice: f32, lead: u64, out: &mut [[f32; HOP]]) -> f32 {
        self.frame += 1;
        let open = match self.params {
            None => true,
            Some(p) => {
                if voice >= p.threshold {
                    self.last_voice = Some(self.frame);
                }
                let hold = (p.hold_ms.max(0.0) / 10.0).ceil() as u64;
                let shown = self.frame.saturating_sub(lead);
                self.last_voice.is_some_and(|v| v + hold >= shown)
            }
        };
        if open && self.gain == 1.0 {
            return 1.0;
        }
        let mut lowest = self.gain;
        for j in 0..HOP {
            self.gain = if open { (self.gain + Self::OPEN_STEP).min(1.0) } else { (self.gain - Self::CLOSE_STEP).max(0.0) };
            lowest = lowest.min(self.gain);
            for o in out.iter_mut() {
                o[j] *= self.gain;
            }
        }
        lowest
    }
}

pub struct Denoiser {
    engine: Engine,
    ch: usize,
    /// Position in the current frame.
    pos: usize,
    input: [[f32; HOP]; MAX_CH],
    output: [[f32; HOP]; MAX_CH],
    /// The model's own delay in samples (a multiple of [`HOP`]). Only DPDFNet's changes, when
    /// PipeWire's buffers grow.
    delay: usize,
    /// Recent input per channel, longer than the longest delay, for the dry signal lined up with
    /// the model's output. `dry_at` is where the current frame starts.
    dry: Vec<Vec<f32>>,
    dry_at: usize,
    /// How much of the dry signal is mixed back to limit the reduction (0 = none), where the model
    /// doesn't do that itself.
    mix: f32,
    /// Switched off: how much of the dry signal plays (1 = only dry), easing towards the target
    /// over one frame so switching doesn't click. The model keeps running, so it's ready when
    /// switched back on, and the delay stays the same either way.
    bypass: f32,
    bypass_target: f32,
    gate: VoiceGate,
    /// RNNoise run only to tell voice from noise, for the models that aren't RNNoise.
    detector: Option<RnNoise>,
    /// Since the last [`Denoiser::take_activity`]: the gate's lowest gain and the highest voice guess.
    gate_min: f32,
    voice_max: f32,
}

impl Denoiser {
    pub fn new(model: DenoiseModel, channels: usize, reduction_db: f32, enabled: bool) -> anyhow::Result<Denoiser> {
        let ch = channels.clamp(1, MAX_CH);
        let (engine, delay) = match model {
            // RNNoise's output is one frame behind its input.
            DenoiseModel::RnNoise => (Engine::RnNoise((0..ch).map(|_| RnNoise::new()).collect::<anyhow::Result<_>>()?), HOP),
            DenoiseModel::DpdfNet => {
                let w = dpdfnet::Worker::start(ch)?;
                let delay = dpdfnet::MODEL_DELAY + w.lag() * HOP;
                (Engine::DpdfNet(Box::new(w)), delay)
            }
            DenoiseModel::DeepFilterNet => {
                let df = DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(ch))?;
                anyhow::ensure!(df.hop_size == HOP && df.sr as f32 == RATE, "unexpected DeepFilterNet frame size or rate");
                let delay = df.delay();
                anyhow::ensure!(delay % HOP == 0, "unexpected DeepFilterNet delay {delay}");
                (Engine::DeepFilter { df: Box::new(df), noisy: Array2::zeros((ch, HOP)), enhanced: Array2::zeros((ch, HOP)) }, delay)
            }
        };
        let detector = if matches!(engine, Engine::RnNoise(_)) { None } else { Some(RnNoise::new()?) };
        let off = if enabled { 0.0 } else { 1.0 };
        let mut d = Denoiser {
            engine,
            ch,
            pos: 0,
            input: [[0.0; HOP]; MAX_CH],
            output: [[0.0; HOP]; MAX_CH],
            delay,
            dry: vec![vec![0.0; MAX_DELAY + HOP]; ch],
            dry_at: 0,
            mix: 0.0,
            bypass: off,
            bypass_target: off,
            gate: VoiceGate::new(),
            detector,
            gate_min: 1.0,
            voice_max: 0.0,
        };
        d.set_reduction(reduction_db);
        Ok(d)
    }

    /// The most the noise is turned down, in dB; [`DENOISE_FULL_DB`] and above don't limit it.
    pub fn set_reduction(&mut self, db: f32) {
        let db = db.max(0.0);
        match &mut self.engine {
            Engine::DeepFilter { df, .. } => df.set_atten_lim(db.min(DENOISE_FULL_DB)),
            _ => self.mix = if db >= DENOISE_FULL_DB { 0.0 } else { 10f32.powf(-db / 20.0) },
        }
    }

    /// Off passes the audio unchanged, but delayed the same, so switching doesn't move it in time.
    pub fn set_enabled(&mut self, on: bool) {
        self.bypass_target = if on { 0.0 } else { 1.0 };
    }

    /// `None` lets everything through.
    pub fn set_gate(&mut self, gate: Option<GateParams>) {
        self.gate.params = gate;
    }

    /// The gate's lowest gain (1 = open) and the highest voice guess (0–1) since the last call.
    pub fn take_activity(&mut self) -> (f32, f32) {
        let a = (self.gate_min, self.voice_max);
        (self.gate_min, self.voice_max) = (1.0, 0.0);
        a
    }

    /// `ins` and `outs` have one slice per channel, all the same length. The output is the input
    /// cleaned and delayed by one frame plus the model's own delay.
    pub fn process(&mut self, ins: &[&[f32]], outs: &mut [&mut [f32]]) {
        let ch = self.ch.min(ins.len()).min(outs.len());
        let n = (0..ch).map(|c| ins[c].len().min(outs[c].len())).min().unwrap_or(0);
        if let Engine::DpdfNet(w) = &mut self.engine {
            w.fit_quantum(n);
            self.delay = dpdfnet::MODEL_DELAY + w.lag() * HOP;
        }
        for i in 0..n {
            for c in 0..ch {
                outs[c][i] = self.output[c][self.pos];
                self.input[c][self.pos] = ins[c][i];
            }
            self.pos += 1;
            if self.pos == HOP {
                self.pos = 0;
                self.run_frame();
            }
        }
    }

    fn run_frame(&mut self) {
        let ch = self.ch;
        let len = MAX_DELAY + HOP;
        for c in 0..ch {
            self.dry[c][self.dry_at..self.dry_at + HOP].copy_from_slice(&self.input[c]);
        }
        // The dry sample lined up with output sample `j`.
        let start = (self.dry_at + len - self.delay) % len;
        let dry = |c: usize, j: usize, d: &Vec<Vec<f32>>| d[c][(start + j) % len];
        let mix = &self.mix;
        let (mut x, mut y) = ([0f32; HOP], [0f32; HOP]);
        let mut voice = 0f32;
        if let Some(det) = &mut self.detector {
            for (j, x) in x.iter_mut().enumerate() {
                *x = (0..ch).map(|c| self.input[c][j]).sum::<f32>() / ch as f32 * I16;
            }
            voice = det.process(&mut y, &x);
        }
        match &mut self.engine {
            Engine::RnNoise(states) => {
                for c in 0..ch {
                    for (x, v) in x.iter_mut().zip(&self.input[c]) {
                        *x = v * I16;
                    }
                    voice = voice.max(states[c].process(&mut y, &x));
                    for j in 0..HOP {
                        self.output[c][j] = y[j] / I16 * (1.0 - *mix) + dry(c, j, &self.dry) * *mix;
                    }
                }
            }
            Engine::DpdfNet(worker) => {
                let mut wet = [[0f32; HOP]; MAX_CH];
                let ok = worker.exchange(&self.input, &mut wet);
                for c in 0..ch {
                    for j in 0..HOP {
                        let d = dry(c, j, &self.dry);
                        self.output[c][j] = if ok { wet[c][j] * (1.0 - *mix) + d * *mix } else { d };
                    }
                }
            }
            Engine::DeepFilter { df, noisy, enhanced } => {
                for c in 0..ch {
                    noisy.row_mut(c).as_slice_mut().unwrap().copy_from_slice(&self.input[c]);
                }
                let ok = df.process(noisy.view(), enhanced.view_mut()).is_ok();
                for c in 0..ch {
                    for j in 0..HOP {
                        self.output[c][j] = if ok { enhanced[[c, j]] } else { dry(c, j, &self.dry) };
                    }
                }
            }
        }
        let gain = self.gate.run(voice, (self.delay / HOP) as u64, &mut self.output[..ch]);
        self.gate_min = self.gate_min.min(gain);
        self.voice_max = self.voice_max.max(if voice.is_finite() { voice } else { 0.0 });
        if self.bypass > 0.0 || self.bypass_target > 0.0 {
            let step = 1.0 / HOP as f32;
            for j in 0..HOP {
                self.bypass = if self.bypass < self.bypass_target {
                    (self.bypass + step).min(self.bypass_target)
                } else {
                    (self.bypass - step).max(self.bypass_target)
                };
                for c in 0..ch {
                    let o = &mut self.output[c][j];
                    *o += (dry(c, j, &self.dry) - *o) * self.bypass;
                }
            }
        }
        self.dry_at = (self.dry_at + HOP) % len;
        for o in self.output[..ch].iter_mut().flatten() {
            if !o.is_finite() {
                *o = 0.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hiss(n: usize, amp: f32) -> Vec<f32> {
        let mut seed = 1u32;
        (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                amp * ((seed >> 8) as f32 / 16777216.0 * 2.0 - 1.0)
            })
            .collect()
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// Run in odd-sized blocks, as PipeWire quantums aren't multiples of a frame.
    fn run(d: &mut Denoiser, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; input.len()];
        for (i, o) in input.chunks(333).zip(out.chunks_mut(333)) {
            d.process(&[i], &mut [o]);
        }
        out
    }

    /// Output delay in samples: the frame we gather plus the model's own.
    fn delay(model: DenoiseModel) -> usize {
        HOP + match model {
            DenoiseModel::RnNoise => HOP,
            DenoiseModel::DeepFilterNet => 1440,
            DenoiseModel::DpdfNet => dpdfnet::MODEL_DELAY + dpdfnet::MIN_LAG * HOP,
        }
    }

    #[test]
    fn delay_matches_what_the_node_shows() {
        for model in DenoiseModel::ALL {
            assert_eq!(delay(model) as u32 * 1000 / RATE as u32, model.delay_ms(), "{model:?}");
            // With no reduction the output is exactly the input, delayed.
            let mut d = Denoiser::new(model, 1, 0.0, true).unwrap();
            let input = hiss(48000, 0.1);
            let out = run(&mut d, &input);
            let k = delay(model);
            let err = (k..input.len()).map(|i| (out[i] - input[i - k]).abs()).fold(0f32, f32::max);
            assert!(err < 1e-3, "{model:?}: off by {err} at a delay of {k}");
        }
    }

    #[test]
    fn switching_off_passes_the_input_with_the_same_delay_and_no_jump() {
        for model in DenoiseModel::ALL {
            let mut d = Denoiser::new(model, 1, DENOISE_FULL_DB, false).unwrap();
            let input = hiss(48000, 0.1);
            let out = run(&mut d, &input);
            let k = delay(model);
            let err = (k..input.len()).map(|i| (out[i] - input[i - k]).abs()).fold(0f32, f32::max);
            assert!(err < 1e-6, "{model:?}: off but changed by {err}");
            // Switching fades over a frame. A smooth 100 Hz tone, which the models remove, moves
            // at most 0.007 per sample; switching without a fade would jump by up to 0.5.
            let tone: Vec<f32> = (0..48000).map(|i| 0.5 * (i as f32 * 100.0 * std::f32::consts::TAU / RATE).sin()).collect();
            let mut d = Denoiser::new(model, 1, DENOISE_FULL_DB, false).unwrap();
            let mut joined = run(&mut d, &tone);
            for on in [true, false] {
                d.set_enabled(on);
                joined.extend(run(&mut d, &tone));
            }
            let jump = joined.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
            assert!(jump < 0.05, "{model:?}: jump of {jump}");
        }
    }

    #[test]
    fn both_models_remove_hiss() {
        // RNNoise 0.2 takes it down 50 dB or more; the 2018 model managed about 10.
        // DPDFNet runs on its own thread at real-time pace; its model has its own test, and the
        // thread `dpdfnet_keeps_up_in_real_time`.
        for (model, at_least_db) in [(DenoiseModel::DeepFilterNet, 20.0), (DenoiseModel::RnNoise, 40.0)] {
            let mut d = Denoiser::new(model, 1, DENOISE_FULL_DB, true).unwrap();
            let noise = hiss(48000 * 6, 0.02);
            let out = run(&mut d, &noise);
            let db = 20.0 * (rms(&out[48000 * 5..]) / rms(&noise[48000 * 5..])).log10();
            assert!(db < -at_least_db, "{model:?}: hiss only {db:.1} dB down");
        }
    }

    /// Fed at the pace PipeWire would, after loading DPDFNet's thread keeps up and removes hiss.
    /// Release builds only: debug builds run the model far slower than real time.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn dpdfnet_keeps_up_in_real_time() {
        let mut d = Denoiser::new(DenoiseModel::DpdfNet, 1, DENOISE_FULL_DB, true).unwrap();
        let block = 256;
        let noise = hiss(block * 2000, 0.02);
        let mut out = vec![0f32; noise.len()];
        let start = std::time::Instant::now();
        for (k, (i, o)) in noise.chunks(block).zip(out.chunks_mut(block)).enumerate() {
            let due = start + std::time::Duration::from_secs_f64(k as f64 * block as f64 / RATE as f64);
            std::thread::sleep(due.saturating_duration_since(std::time::Instant::now()));
            d.process(&[i], &mut [o]);
        }
        // The last 3 s: model loaded (about 1.5 s) and warmed up.
        let tail = noise.len() - 48000 * 3;
        let db = 20.0 * (rms(&out[tail..]) / rms(&noise[tail..])).log10();
        assert!(db < -20.0, "hiss only {db:.1} dB down");
    }

    #[test]
    fn a_reduction_limit_lets_some_noise_through() {
        for model in DenoiseModel::ALL {
            let noise = hiss(48000 * 6, 0.02);
            let mut d = Denoiser::new(model, 1, 6.0, true).unwrap();
            let out = run(&mut d, &noise);
            let db = 20.0 * (rms(&out[48000 * 5..]) / rms(&noise[48000 * 5..])).log10();
            assert!(db > -6.5, "{model:?}: limited to 6 dB but {db:.1} dB down");
        }
    }

    #[test]
    fn the_gate_opens_ahead_of_the_voice_and_holds_after_it() {
        let gate = GateParams { threshold: 0.5, hold_ms: 100.0 };
        let mut g = VoiceGate::new();
        g.params = Some(gate);
        // Voice in input frames 20..40, output 3 frames behind (as DeepFilterNet).
        let lead = 3;
        let mut gains = vec![];
        for f in 1..=100u64 {
            let mut out = [[1.0; HOP]];
            g.run(if (20..40).contains(&f) { 0.9 } else { 0.1 }, lead, &mut out);
            gains.push(out[0]);
        }
        let gain_at = |frame: u64, j: usize| gains[frame as usize - 1][j];
        // Starts closing at once, closed after 50 ms.
        assert!(gain_at(1, HOP - 1) < 1.0 && gain_at(6, 0) == 0.0);
        // Output frame k shows input frame k - 3. Voice arrives in the output in frame 23 and the
        // gate is already fully open from the frame before (opened 30 ms early).
        assert_eq!(gain_at(19, HOP - 1), 0.0);
        assert!(gain_at(20, HOP - 1) == 1.0 && gain_at(22, 0) == 1.0);
        // Last voice shows in output frame 42; held 10 frames, then fades.
        assert_eq!(gain_at(52, HOP - 1), 1.0);
        assert!(gain_at(53, HOP - 1) < 1.0);
        assert_eq!(gain_at(58, 0), 0.0);
        // No jumps anywhere.
        let all: Vec<f32> = gains.iter().flatten().copied().collect();
        let jump = all.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        assert!(jump <= VoiceGate::OPEN_STEP + 1e-6, "jump {jump}");
        // Switched off: opens again.
        g.params = None;
        let mut out = [[1.0; HOP]];
        g.run(0.0, lead, &mut out);
        assert!(out[0][HOP - 1] > 0.9);
    }

    #[test]
    fn the_gate_silences_noise_with_every_model() {
        for model in DenoiseModel::ALL {
            // No reduction, so only the gate can take the hiss away.
            let mut d = Denoiser::new(model, 1, 0.0, true).unwrap();
            d.set_gate(Some(GateParams { threshold: 0.8, hold_ms: 200.0 }));
            let noise = hiss(48000 * 2, 0.02);
            let out = run(&mut d, &noise);
            let (gate, voice) = d.take_activity();
            assert!(out[48000..].iter().all(|v| *v == 0.0), "{model:?}: gate didn't close (voice {voice})");
            assert_eq!(gate, 0.0, "{model:?}");
            // Switching the node off still lets everything through.
            d.set_enabled(false);
            let out = run(&mut d, &noise);
            assert!(rms(&out[48000..]) > 0.01, "{model:?}");
        }
    }

    #[test]
    fn stereo_and_silence_stay_finite() {
        for model in DenoiseModel::ALL {
            let mut d = Denoiser::new(model, 2, DENOISE_FULL_DB, true).unwrap();
            let n = 256 * 200;
            let zeros = vec![0.0; n];
            let (mut l, mut r) = (vec![1.0; n], vec![1.0; n]);
            for k in (0..n).step_by(256) {
                d.process(&[&zeros[k..k + 256], &zeros[k..k + 256]], &mut [&mut l[k..k + 256], &mut r[k..k + 256]]);
            }
            assert!(l.iter().chain(&r).all(|v| v.is_finite() && v.abs() < 1e-3), "{model:?}");
        }
    }
}
