//! DPDFNet-2 noise suppression (Ceva, Apache-2.0, `models/`), run with tract on a worker thread.
//!
//! The model takes one 10 ms spectrum frame and its recurrent state, and returns the cleaned
//! frame and the new state. Its output belongs to the input of 4 frames earlier, and the STFT
//! adds one more: [`MODEL_DELAY`]. The streaming STFT and the state set-up follow HushMic's
//! `hushmic-denoiser` (MIT or Apache-2.0, <https://github.com/Fovty/HushMic>).
//!
//! A frame takes about 2 ms (up to 5), too long for PipeWire's realtime thread, and loading the
//! model about 1.5 s. So [`Worker`] runs it on its own thread: the realtime thread hands frames
//! over through wait-free queues and takes results back [`Worker::lag`] frames later. Whatever
//! isn't ready in time (while loading, or if the thread falls behind) the caller replaces with the
//! unprocessed signal at the same delay.

use crate::denoise::HOP;
use crate::dsp::MAX_CH;
use anyhow::{Context, Result};
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tract_onnx::prelude::*;

static MODEL: &[u8] = include_bytes!("../models/dpdfnet2_48khz_hr.onnx");

const N_FFT: usize = 2 * HOP;
const BINS: usize = HOP + 1;
const SPEC: usize = 2 * BINS;

/// Delay of the model's output against its input, in samples: 4 frames in the model, 1 in the STFT.
pub const MODEL_DELAY: usize = 5 * HOP;

/// Vorbis window: constant overlap-add at 50% overlap, so analysis and synthesis reconstruct exactly.
fn window() -> Vec<f32> {
    (0..N_FFT)
        .map(|n| {
            let s = (std::f32::consts::FRAC_PI_2 * (n as f32 + 0.5) / HOP as f32).sin();
            (std::f32::consts::FRAC_PI_2 * s * s).sin()
        })
        .collect()
}

/// One channel's streaming STFT and model state.
struct Channel {
    run: TypedSimpleState,
    state: Tensor,
    ring: Vec<f32>,
    ola: Vec<f32>,
}

/// The model, processing frames as they come (synchronously).
pub struct Dpdf {
    channels: Vec<Channel>,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
}

fn meta<'a>(proto: &'a tract_onnx::pb::ModelProto, key: &str) -> Option<&'a str> {
    proto.metadata_props.iter().find(|p| p.key == key).map(|p| p.value.as_str())
}

fn floats(s: &str) -> Vec<f32> {
    s.split(',').filter_map(|t| t.trim().parse().ok()).collect()
}

impl Dpdf {
    /// Loads the embedded model (about 1.5 s): keep it off the realtime thread.
    pub fn new(channels: usize) -> Result<Dpdf> {
        let onnx = tract_onnx::onnx();
        let proto = onnx.proto_model_for_read(&mut &MODEL[..]).context("reading DPDFNet")?;
        let get = |k: &str| meta(&proto, k).with_context(|| format!("DPDFNet model without {k}"));
        let num = |k: &str| -> Result<usize> { Ok(get(k)?.trim().parse()?) };
        anyhow::ensure!(num("n_fft")? == N_FFT && num("hop_length")? == HOP && num("sample_rate")? == 48000, "unexpected DPDFNet framing");
        // The state starts with the feature normalisation's running means, then zeros.
        let (size, erb_n, spec_n) = (num("state_size")?, num("erb_norm_state_size")?, num("spec_norm_state_size")?);
        let (erb, spec) = (floats(get("erb_norm_init")?), floats(get("spec_norm_init")?));
        anyhow::ensure!(erb.len() == erb_n && spec.len() == spec_n && erb_n + spec_n <= size, "unexpected DPDFNet state");
        let mut init = vec![0f32; size];
        init[..erb_n].copy_from_slice(&erb);
        init[erb_n..erb_n + spec_n].copy_from_slice(&spec);

        let plan = onnx.model_for_read(&mut &MODEL[..])?.into_optimized()?.into_runnable()?;
        let mut planner = FftPlanner::new();
        Ok(Dpdf {
            channels: (0..channels.clamp(1, MAX_CH))
                .map(|_| {
                    Ok(Channel {
                        run: TypedSimpleState::new(&plan)?,
                        state: Tensor::from_shape(&[size], &init)?,
                        ring: vec![0.0; N_FFT],
                        ola: vec![0.0; N_FFT],
                    })
                })
                .collect::<Result<_>>()?,
            window: window(),
            fft: planner.plan_fft_forward(N_FFT),
            ifft: planner.plan_fft_inverse(N_FFT),
            scratch: vec![Complex32::default(); N_FFT],
        })
    }

    /// One frame in, one out ([`MODEL_DELAY`] behind). On a model error the frame comes out silent
    /// and the state stays as it was.
    pub fn process(&mut self, input: &[[f32; HOP]; MAX_CH], output: &mut [[f32; HOP]; MAX_CH]) {
        for (c, ch) in self.channels.iter_mut().enumerate() {
            // Analysis: the last two frames, windowed.
            ch.ring.copy_within(HOP.., 0);
            ch.ring[HOP..].copy_from_slice(&input[c]);
            for ((s, x), w) in self.scratch.iter_mut().zip(&ch.ring).zip(&self.window) {
                *s = Complex32::new(x * w, 0.0);
            }
            self.fft.process(&mut self.scratch);
            let mut spec = [0f32; SPEC];
            for k in 0..BINS {
                spec[2 * k] = self.scratch[k].re;
                spec[2 * k + 1] = self.scratch[k].im;
            }
            let enhanced = Tensor::from_shape(&[1, 1, BINS, 2], &spec)
                .and_then(|t| ch.run.run(tvec!(t.into_tvalue(), ch.state.clone().into_tvalue())))
                .and_then(|mut out| {
                    anyhow::ensure!(out.len() == 2, "DPDFNet gave {} outputs", out.len());
                    let state = out.pop().unwrap().into_tensor();
                    let spec_e = out.pop().unwrap().into_tensor();
                    anyhow::ensure!(spec_e.len() == SPEC && state.len() == ch.state.len(), "unexpected DPDFNet output");
                    ch.state = state;
                    let mut e = [0f32; SPEC];
                    e.copy_from_slice(spec_e.try_as_plain_ram()?.as_slice::<f32>()?);
                    Ok(e)
                })
                .unwrap_or([0f32; SPEC]);
            // Synthesis: the full spectrum (mirrored), inverse FFT, window, overlap-add.
            for k in 0..BINS {
                self.scratch[k] = Complex32::new(enhanced[2 * k], enhanced[2 * k + 1]);
            }
            for k in BINS..N_FFT {
                self.scratch[k] = self.scratch[N_FFT - k].conj();
            }
            self.ifft.process(&mut self.scratch);
            ch.ola.copy_within(HOP.., 0);
            ch.ola[HOP..].fill(0.0);
            for ((o, s), w) in ch.ola.iter_mut().zip(&self.scratch).zip(&self.window) {
                *o += s.re / N_FFT as f32 * w;
            }
            output[c].copy_from_slice(&ch.ola[..HOP]);
        }
    }
}

#[derive(Clone, Copy)]
struct Frame {
    index: u64,
    audio: [[f32; HOP]; MAX_CH],
}

struct Shared {
    stop: AtomicBool,
    /// The newest frame the realtime thread has handed over plus one.
    next: AtomicU64,
    /// Frames the realtime thread waits for a result.
    lag: AtomicU64,
}

/// The realtime side of the worker thread.
pub struct Worker {
    to_worker: rtrb::Producer<Frame>,
    from_worker: rtrb::Consumer<Frame>,
    thread: std::thread::Thread,
    shared: Arc<Shared>,
    next: u64,
    lag: u64,
}

/// The fewest frames of slack: a frame period (10 ms) for the worker.
pub const MIN_LAG: usize = 2;
/// The most frames of slack, for PipeWire buffers up to 3360 samples (70 ms).
pub const MAX_LAG: usize = 8;

impl Worker {
    /// Starts the thread, which loads the model first. Until then nothing comes back.
    pub fn start(channels: usize) -> Result<Worker> {
        let (to_worker, mut inbox) = rtrb::RingBuffer::<Frame>::new(MAX_LAG * 4);
        let (mut outbox, from_worker) = rtrb::RingBuffer::<Frame>::new(MAX_LAG * 4);
        let shared = Arc::new(Shared { stop: AtomicBool::new(false), next: AtomicU64::new(0), lag: AtomicU64::new(MIN_LAG as u64) });
        let s = shared.clone();
        let handle = std::thread::Builder::new().name("dpdfnet".into()).spawn(move || {
            let mut model = match Dpdf::new(channels) {
                Ok(m) => m,
                Err(e) => {
                    log::error!("DPDFNet: {e:#}");
                    return;
                }
            };
            let mut out = Frame { index: 0, audio: [[0.0; HOP]; MAX_CH] };
            while !s.stop.load(Ordering::Relaxed) {
                let Ok(frame) = inbox.pop() else {
                    std::thread::park_timeout(Duration::from_millis(5));
                    continue;
                };
                // Too late to be used: skip it (only after loading or a long stall).
                if frame.index + s.lag.load(Ordering::Relaxed) < s.next.load(Ordering::Relaxed) {
                    continue;
                }
                model.process(&frame.audio, &mut out.audio);
                out.index = frame.index;
                let _ = outbox.push(out);
            }
        })?;
        Ok(Worker { to_worker, from_worker, thread: handle.thread().clone(), shared, next: 0, lag: MIN_LAG as u64 })
    }

    /// Frames between handing one over and taking its result.
    pub fn lag(&self) -> usize {
        self.lag as usize
    }

    /// Enough slack for PipeWire buffers of `quantum` samples: whole buffers are handed over at
    /// once, and the worker still gets a frame period after that.
    pub fn fit_quantum(&mut self, quantum: usize) {
        let lag = (quantum + HOP).div_ceil(HOP).clamp(MIN_LAG, MAX_LAG) as u64;
        if lag > self.lag {
            self.lag = lag;
            self.shared.lag.store(lag, Ordering::Relaxed);
        }
    }

    /// Hands over the next input frame and takes the result for the one [`Self::lag`] frames
    /// before it. False if that isn't there (the caller plays the unprocessed signal instead).
    /// Wait-free: called on the realtime thread.
    pub fn exchange(&mut self, input: &[[f32; HOP]; MAX_CH], output: &mut [[f32; HOP]; MAX_CH]) -> bool {
        let _ = self.to_worker.push(Frame { index: self.next, audio: *input });
        self.next += 1;
        self.shared.next.store(self.next, Ordering::Relaxed);
        self.thread.unpark();
        let Some(want) = self.next.checked_sub(self.lag + 1) else { return false };
        while let Ok(f) = self.from_worker.peek() {
            if f.index < want {
                let _ = self.from_worker.pop();
            } else if f.index == want {
                *output = self.from_worker.pop().unwrap().audio;
                return true;
            } else {
                break;
            }
        }
        false
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // The thread ends by itself (after loading, if it still is); don't wait for it.
        self.shared.stop.store(true, Ordering::Relaxed);
        self.thread.unpark();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hiss(n: usize, amp: f32) -> Vec<f32> {
        let mut seed = 7u32;
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

    #[test]
    fn stft_alone_reconstructs_one_frame_later() {
        // Bypass the model: analysis straight into synthesis.
        let w = window();
        let input = hiss(HOP * 20, 0.3);
        let mut planner = FftPlanner::new();
        let (fft, ifft) = (planner.plan_fft_forward(N_FFT), planner.plan_fft_inverse(N_FFT));
        let (mut ring, mut ola, mut out) = (vec![0f32; N_FFT], vec![0f32; N_FFT], vec![]);
        let mut s = vec![Complex32::default(); N_FFT];
        for frame in input.chunks(HOP) {
            ring.copy_within(HOP.., 0);
            ring[HOP..].copy_from_slice(frame);
            for ((s, x), w) in s.iter_mut().zip(&ring).zip(&w) {
                *s = Complex32::new(x * w, 0.0);
            }
            fft.process(&mut s);
            for k in BINS..N_FFT {
                s[k] = s[N_FFT - k].conj();
            }
            ifft.process(&mut s);
            ola.copy_within(HOP.., 0);
            ola[HOP..].fill(0.0);
            for ((o, s), w) in ola.iter_mut().zip(&s).zip(&w) {
                *o += s.re / N_FFT as f32 * w;
            }
            out.extend_from_slice(&ola[..HOP]);
        }
        let err = (2 * HOP..input.len()).map(|i| (out[i] - input[i - HOP]).abs()).fold(0f32, f32::max);
        assert!(err < 1e-4, "{err}");
    }

    /// The output belongs to the input [`MODEL_DELAY`] earlier, so the dry signal mixed back for
    /// the reduction limit and the On switch lines up with it.
    #[test]
    fn output_lines_up_with_the_model_delay() {
        let mut m = Dpdf::new(1).unwrap();
        // Voice-like: a 150 Hz buzz with harmonics, in syllables of 200 ms, over light hiss.
        let n = 48000 * 3;
        let noise = hiss(n, 0.005);
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / 48000.0;
                let on = if (t * 5.0) as u32 % 2 == 0 { 1.0 } else { 0.0 };
                on * 0.2 * (1..=12).map(|h| (h as f32 * 150.0 * std::f32::consts::TAU * t).sin() / h as f32).sum::<f32>() + noise[i]
            })
            .collect();
        let mut out = vec![];
        let (mut i, mut o) = ([[0f32; HOP]; MAX_CH], [[0f32; HOP]; MAX_CH]);
        for frame in input.chunks(HOP) {
            i[0].copy_from_slice(frame);
            m.process(&i, &mut o);
            out.extend_from_slice(&o[0]);
        }
        let corr = |lag: usize| (48000..n - 4000).map(|k| input[k] as f64 * out[k + lag] as f64).sum::<f64>();
        let best = (0..4000).max_by(|&a, &b| corr(a).total_cmp(&corr(b))).unwrap();
        assert_eq!(best, MODEL_DELAY, "output is {best} samples behind");
    }

    #[test]
    fn model_removes_hiss() {
        let mut m = Dpdf::new(1).unwrap();
        let noise = hiss(48000 * 2, 0.02);
        let mut out = vec![];
        let (mut i, mut o) = ([[0f32; HOP]; MAX_CH], [[0f32; HOP]; MAX_CH]);
        for frame in noise.chunks(HOP) {
            i[0].copy_from_slice(frame);
            m.process(&i, &mut o);
            out.extend_from_slice(&o[0]);
        }
        let db = 20.0 * (rms(&out[48000..]) / rms(&noise[48000..])).log10();
        assert!(db < -20.0, "hiss only {db:.1} dB down");
    }
}
