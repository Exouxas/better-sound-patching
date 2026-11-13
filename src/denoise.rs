//! Noise suppression with a neural network: DeepFilterNet 3 (`deep-filter-rt`, MIT or Apache-2.0,
//! model embedded) or RNNoise 0.2 (Xiph's `librnnoise`, BSD-3-Clause, the system library with the
//! models retrained in 2024). Both work on 10 ms frames of 48 kHz
//! audio, so audio is gathered into frames first, which adds one frame of delay on top of the
//! model's own.
//!
//! Everything is set up in [`Denoiser::new`] (off the realtime thread). RNNoise then runs without
//! allocating; DeepFilterNet's inference library may allocate small buffers per frame, which
//! costs far less than a frame lasts (about 0.15 ms per 10 ms on a desktop CPU).

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

/// RNNoise works on 16-bit sample values in floats.
const I16: f32 = 32768.0;

enum Engine {
    RnNoise {
        states: Vec<RnNoise>,
        /// How much of the dry signal is mixed back to limit the reduction (0 = none).
        mix: f32,
    },
    DeepFilter { df: Box<DfTract>, noisy: Array2<f32>, enhanced: Array2<f32> },
}

pub struct Denoiser {
    engine: Engine,
    ch: usize,
    /// Position in the current frame.
    pos: usize,
    input: [[f32; HOP]; MAX_CH],
    output: [[f32; HOP]; MAX_CH],
    /// The model's own delay in samples (a multiple of [`HOP`]).
    delay: usize,
    /// Recent input per channel, `delay + HOP` long, for the dry signal lined up with the model's
    /// output. `dry_at` is where the current frame starts.
    dry: Vec<Vec<f32>>,
    dry_at: usize,
    /// Switched off: how much of the dry signal plays (1 = only dry), easing towards the target
    /// over one frame so switching doesn't click. The model keeps running, so it's ready when
    /// switched back on, and the delay stays the same either way.
    bypass: f32,
    bypass_target: f32,
}

impl Denoiser {
    pub fn new(model: DenoiseModel, channels: usize, reduction_db: f32, enabled: bool) -> anyhow::Result<Denoiser> {
        let ch = channels.clamp(1, MAX_CH);
        let (engine, delay) = match model {
            // RNNoise's output is one frame behind its input.
            DenoiseModel::RnNoise => (Engine::RnNoise { states: (0..ch).map(|_| RnNoise::new()).collect::<anyhow::Result<_>>()?, mix: 0.0 }, HOP),
            DenoiseModel::DeepFilterNet => {
                let df = DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(ch))?;
                anyhow::ensure!(df.hop_size == HOP && df.sr as f32 == RATE, "unexpected DeepFilterNet frame size or rate");
                let delay = df.delay();
                anyhow::ensure!(delay % HOP == 0, "unexpected DeepFilterNet delay {delay}");
                (Engine::DeepFilter { df: Box::new(df), noisy: Array2::zeros((ch, HOP)), enhanced: Array2::zeros((ch, HOP)) }, delay)
            }
        };
        let off = if enabled { 0.0 } else { 1.0 };
        let mut d = Denoiser {
            engine,
            ch,
            pos: 0,
            input: [[0.0; HOP]; MAX_CH],
            output: [[0.0; HOP]; MAX_CH],
            delay,
            dry: vec![vec![0.0; delay + HOP]; ch],
            dry_at: 0,
            bypass: off,
            bypass_target: off,
        };
        d.set_reduction(reduction_db);
        Ok(d)
    }

    /// The most the noise is turned down, in dB; [`DENOISE_FULL_DB`] and above don't limit it.
    pub fn set_reduction(&mut self, db: f32) {
        let db = db.max(0.0);
        match &mut self.engine {
            Engine::RnNoise { mix, .. } => *mix = if db >= DENOISE_FULL_DB { 0.0 } else { 10f32.powf(-db / 20.0) },
            Engine::DeepFilter { df, .. } => df.set_atten_lim(db.min(DENOISE_FULL_DB)),
        }
    }

    /// Off passes the audio unchanged, but delayed the same, so switching doesn't move it in time.
    pub fn set_enabled(&mut self, on: bool) {
        self.bypass_target = if on { 0.0 } else { 1.0 };
    }

    /// `ins` and `outs` have one slice per channel, all the same length. The output is the input
    /// cleaned and delayed by one frame plus the model's own delay.
    pub fn process(&mut self, ins: &[&[f32]], outs: &mut [&mut [f32]]) {
        let ch = self.ch.min(ins.len()).min(outs.len());
        let n = (0..ch).map(|c| ins[c].len().min(outs[c].len())).min().unwrap_or(0);
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
        let len = self.delay + HOP;
        for c in 0..ch {
            self.dry[c][self.dry_at..self.dry_at + HOP].copy_from_slice(&self.input[c]);
        }
        // The dry sample lined up with output sample `j`.
        let start = (self.dry_at + len - self.delay) % len;
        let dry = |c: usize, j: usize, d: &Vec<Vec<f32>>| d[c][(start + j) % len];
        match &mut self.engine {
            Engine::RnNoise { states, mix } => {
                let (mut x, mut y) = ([0f32; HOP], [0f32; HOP]);
                for c in 0..ch {
                    for (x, v) in x.iter_mut().zip(&self.input[c]) {
                        *x = v * I16;
                    }
                    states[c].process(&mut y, &x);
                    for j in 0..HOP {
                        self.output[c][j] = y[j] / I16 * (1.0 - *mix) + dry(c, j, &self.dry) * *mix;
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
        for (model, at_least_db) in [(DenoiseModel::DeepFilterNet, 20.0), (DenoiseModel::RnNoise, 40.0)] {
            let mut d = Denoiser::new(model, 1, DENOISE_FULL_DB, true).unwrap();
            let noise = hiss(48000 * 6, 0.02);
            let out = run(&mut d, &noise);
            let db = 20.0 * (rms(&out[48000 * 5..]) / rms(&noise[48000 * 5..])).log10();
            assert!(db < -at_least_db, "{model:?}: hiss only {db:.1} dB down");
        }
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
