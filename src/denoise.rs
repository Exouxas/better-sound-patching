//! Noise suppression with a neural network: DeepFilterNet 3 (`deep-filter-rt`, MIT or Apache-2.0,
//! model embedded) or RNNoise (`nnnoiseless`, BSD-3-Clause). Both work on 10 ms frames of 48 kHz
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
use nnnoiseless::DenoiseState;

/// Samples per frame (10 ms at 48 kHz), the same for both models.
pub const HOP: usize = 480;
/// The only sample rate the models work at. At any other rate the audio passes unchanged.
pub const RATE: f32 = 48000.0;

/// RNNoise works on 16-bit sample values in floats.
const I16: f32 = 32768.0;

enum Engine {
    RnNoise {
        states: Vec<Box<DenoiseState<'static>>>,
        /// The previous input frame per channel: RNNoise's output is one frame behind its input,
        /// and this is what gets mixed back in to limit the reduction.
        dry: [[f32; HOP]; MAX_CH],
        /// How much of the dry signal is mixed back (0 = none).
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
}

impl Denoiser {
    pub fn new(model: DenoiseModel, channels: usize, reduction_db: f32) -> anyhow::Result<Denoiser> {
        let ch = channels.clamp(1, MAX_CH);
        let engine = match model {
            DenoiseModel::RnNoise => Engine::RnNoise { states: (0..ch).map(|_| DenoiseState::new()).collect(), dry: [[0.0; HOP]; MAX_CH], mix: 0.0 },
            DenoiseModel::DeepFilterNet => {
                let df = DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(ch))?;
                anyhow::ensure!(df.hop_size == HOP && df.sr as f32 == RATE, "unexpected DeepFilterNet frame size or rate");
                Engine::DeepFilter { df: Box::new(df), noisy: Array2::zeros((ch, HOP)), enhanced: Array2::zeros((ch, HOP)) }
            }
        };
        let mut d = Denoiser { engine, ch, pos: 0, input: [[0.0; HOP]; MAX_CH], output: [[0.0; HOP]; MAX_CH] };
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
        match &mut self.engine {
            Engine::RnNoise { states, dry, mix } => {
                let (mut x, mut y) = ([0f32; HOP], [0f32; HOP]);
                for c in 0..ch {
                    for (x, v) in x.iter_mut().zip(&self.input[c]) {
                        *x = v * I16;
                    }
                    states[c].process_frame(&mut y, &x);
                    for ((o, y), d) in self.output[c].iter_mut().zip(&y).zip(&dry[c]) {
                        *o = y / I16 * (1.0 - *mix) + d * *mix;
                    }
                    dry[c] = self.input[c];
                }
            }
            Engine::DeepFilter { df, noisy, enhanced } => {
                for c in 0..ch {
                    noisy.row_mut(c).as_slice_mut().unwrap().copy_from_slice(&self.input[c]);
                }
                let ok = df.process(noisy.view(), enhanced.view_mut()).is_ok();
                for c in 0..ch {
                    let src = if ok { enhanced.row(c) } else { noisy.row(c) };
                    self.output[c].copy_from_slice(src.as_slice().unwrap());
                }
            }
        }
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
            let mut d = Denoiser::new(model, 1, 0.0).unwrap();
            let input = hiss(48000, 0.1);
            let out = run(&mut d, &input);
            let k = delay(model);
            let err = (k..input.len()).map(|i| (out[i] - input[i - k]).abs()).fold(0f32, f32::max);
            assert!(err < 1e-3, "{model:?}: off by {err} at a delay of {k}");
        }
    }

    #[test]
    fn both_models_remove_hiss() {
        // RNNoise adapts to steady hiss over seconds and stays weak against it; DeepFilterNet
        // removes nearly all of it.
        for (model, at_least_db) in [(DenoiseModel::DeepFilterNet, 20.0), (DenoiseModel::RnNoise, 6.0)] {
            let mut d = Denoiser::new(model, 1, DENOISE_FULL_DB).unwrap();
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
            let mut d = Denoiser::new(model, 1, 6.0).unwrap();
            let out = run(&mut d, &noise);
            let db = 20.0 * (rms(&out[48000 * 5..]) / rms(&noise[48000 * 5..])).log10();
            assert!(db > -6.5, "{model:?}: limited to 6 dB but {db:.1} dB down");
        }
    }

    #[test]
    fn stereo_and_silence_stay_finite() {
        for model in DenoiseModel::ALL {
            let mut d = Denoiser::new(model, 2, DENOISE_FULL_DB).unwrap();
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
