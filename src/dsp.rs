//! Audio processing for the nodes that run inside the daemon: equalizer (biquads from the RBJ
//! audio EQ cookbook), level meter with gate and limiter, and volume. No PipeWire here, so it's
//! unit-testable. Everything that runs per sample is allocation-free.

use crate::model::{Band, BandKind, FilterSpec, MAX_BANDS};
use std::f64::consts::PI;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coeffs {
    pub const IDENTITY: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    pub fn new(band: &Band, rate: f64) -> Coeffs {
        if !band.enabled || rate <= 0.0 {
            return Coeffs::IDENTITY;
        }
        let f = (band.freq as f64).clamp(10.0, rate * 0.49);
        let q = (band.q as f64).clamp(0.05, 40.0);
        let a = 10f64.powf(band.gain_db as f64 / 40.0);
        let w0 = 2.0 * PI * f / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let sa = 2.0 * a.sqrt() * alpha;
        let (b0, b1, b2, a0, a1, a2) = match band.kind {
            BandKind::Bell => (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a),
            BandKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - sa),
                (a + 1.0) + (a - 1.0) * cos + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - sa,
            ),
            BandKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - sa),
                (a + 1.0) - (a - 1.0) * cos + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - sa,
            ),
            BandKind::LowPass => {
                ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            BandKind::HighPass => {
                ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            BandKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        };
        Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// Gain of this filter at `freq` Hz, as a linear factor.
    pub fn magnitude(&self, freq: f64, rate: f64) -> f64 {
        let w = 2.0 * PI * freq / rate;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        // H(e^jw) = (b0 + b1 e^-jw + b2 e^-2jw) / (1 + a1 e^-jw + a2 e^-2jw)
        let (nr, ni) = (self.b0 + self.b1 * c1 + self.b2 * c2, -(self.b1 * s1 + self.b2 * s2));
        let (dr, di) = (1.0 + self.a1 * c1 + self.a2 * c2, -(self.a1 * s1 + self.a2 * s2));
        ((nr * nr + ni * ni) / (dr * dr + di * di)).sqrt()
    }
}

/// Total response of an EQ (preamp plus every band) at `freq` Hz, in dB. For drawing the curve.
pub fn response_db(preamp_db: f32, bands: &[Band], freq: f64) -> f64 {
    const RATE: f64 = 48000.0;
    let mut db = preamp_db as f64;
    for b in bands {
        db += 20.0 * Coeffs::new(b, RATE).magnitude(freq, RATE).max(1e-9).log10();
    }
    db
}

/// Fixed-size EQ settings that can be copied into the realtime thread without allocating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqParams {
    pub preamp_db: f32,
    pub n: usize,
    pub bands: [Band; MAX_BANDS],
}

impl EqParams {
    pub fn from_spec(preamp_db: f32, bands: &[Band]) -> Self {
        let off = Band { kind: BandKind::Bell, freq: 1000.0, gain_db: 0.0, q: 1.0, enabled: false };
        let mut p = EqParams { preamp_db, n: bands.len().min(MAX_BANDS), bands: [off; MAX_BANDS] };
        p.bands[..p.n].copy_from_slice(&bands[..p.n]);
        p
    }
}

/// One channel's filter chain, with state. Transposed direct form II in f64.
#[derive(Clone)]
pub struct Chain {
    gain: f64,
    n: usize,
    coeffs: [Coeffs; MAX_BANDS],
    state: [[f64; 2]; MAX_BANDS],
}

impl Default for Chain {
    fn default() -> Self {
        Chain { gain: 1.0, n: 0, coeffs: [Coeffs::IDENTITY; MAX_BANDS], state: [[0.0; 2]; MAX_BANDS] }
    }
}

impl Chain {
    /// Retune. Filter state is kept, so changing settings while playing doesn't click much.
    pub fn set(&mut self, p: &EqParams, rate: f64) {
        self.gain = 10f64.powf(p.preamp_db as f64 / 20.0);
        self.n = p.n;
        for k in 0..p.n {
            self.coeffs[k] = Coeffs::new(&p.bands[k], rate);
        }
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        for (x, y) in input.iter().zip(output.iter_mut()) {
            let mut v = *x as f64 * self.gain;
            for k in 0..self.n {
                let c = &self.coeffs[k];
                let s = &mut self.state[k];
                let out = c.b0 * v + s[0];
                s[0] = c.b1 * v - c.a1 * out + s[1];
                s[1] = c.b2 * v - c.a2 * out;
                v = out;
            }
            // Denormals and runaway values would otherwise poison the chain forever.
            *y = if v.is_finite() { v as f32 } else { 0.0 };
        }
        for s in self.state[..self.n].iter_mut().flatten() {
            if !s.is_finite() || s.abs() < 1e-30 {
                *s = 0.0;
            }
        }
    }
}

/// Levels measured on the realtime thread, collected (and reset) by whoever displays them.
/// All values are linear factors.
#[derive(Default)]
pub struct Meter {
    in_peak: AtomicU32,
    out_peak: AtomicU32,
    /// Lowest gate gain since the last read (1 = open, 0 = closed).
    gate: AtomicU32,
    /// Lowest limiter gain since the last read (1 = not limiting).
    limit: AtomicU32,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reading {
    pub in_peak: f32,
    pub out_peak: f32,
    pub gate: f32,
    pub limit: f32,
}

impl Meter {
    pub fn new() -> Self {
        let m = Meter::default();
        m.gate.store(1f32.to_bits(), Ordering::Relaxed);
        m.limit.store(1f32.to_bits(), Ordering::Relaxed);
        m
    }
    fn update(cell: &AtomicU32, v: f32, keep_max: bool) {
        let _ = cell.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            let o = f32::from_bits(old);
            let better = if keep_max { v > o } else { v < o };
            better.then_some(v.to_bits())
        });
    }
    pub fn record(&self, in_peak: f32, out_peak: f32, gate: f32, limit: f32) {
        Self::update(&self.in_peak, in_peak, true);
        Self::update(&self.out_peak, out_peak, true);
        Self::update(&self.gate, gate, false);
        Self::update(&self.limit, limit, false);
    }
    /// Everything since the last call.
    pub fn take(&self) -> Reading {
        let get = |c: &AtomicU32, reset: f32| f32::from_bits(c.swap(reset.to_bits(), Ordering::Relaxed));
        Reading { in_peak: get(&self.in_peak, 0.0), out_peak: get(&self.out_peak, 0.0), gate: get(&self.gate, 1.0), limit: get(&self.limit, 1.0) }
    }
}

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn lin_to_db(v: f32) -> f32 {
    20.0 * v.max(1e-6).log10()
}

/// One-pole smoothing coefficient reaching ~63% in `seconds`.
fn coef(seconds: f32, rate: f32) -> f32 {
    1.0 - (-1.0 / (seconds * rate).max(1.0)).exp()
}

/// Copyable settings handed to the realtime thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Params {
    Eq(EqParams),
    Levels { gate: Option<f32>, limit: Option<f32> },
    Volume { gain: f32 },
}

impl Params {
    pub fn from_spec(spec: &FilterSpec) -> Params {
        match spec {
            FilterSpec::Eq(e) => Params::Eq(EqParams::from_spec(e.preamp_db, &e.bands)),
            FilterSpec::Levels(l) => Params::Levels {
                gate: l.gate_on.then(|| db_to_lin(l.gate_db)),
                limit: l.limit_on.then(|| db_to_lin(l.limit_db)),
            },
            FilterSpec::Volume(v) => Params::Volume { gain: v.gain.clamp(0.0, crate::model::MAX_VOLUME) },
        }
    }
}

/// Processing state for one node (all channels).
pub struct Processor {
    params: Params,
    rate: f32,
    chains: [Chain; MAX_CH],
    /// Gate: envelope, gain, hold countdown (samples).
    env: f32,
    gate_g: f32,
    hold: u32,
    limit_g: f32,
    /// Volume: smoothed gain.
    vol_g: f32,
}

pub const MAX_CH: usize = 2;

impl Processor {
    pub fn new(params: Params) -> Self {
        let vol_g = if let Params::Volume { gain } = params { gain } else { 1.0 };
        Processor { params, rate: 0.0, chains: Default::default(), env: 0.0, gate_g: 1.0, hold: 0, limit_g: 1.0, vol_g }
    }

    pub fn set(&mut self, params: Params, rate: f32) {
        self.params = params;
        self.rate = rate;
        if let Params::Eq(p) = &params {
            for c in &mut self.chains {
                c.set(p, rate as f64);
            }
        }
    }

    /// `ins` and `outs` have one slice per channel, all the same length.
    pub fn process(&mut self, ins: &[&[f32]], outs: &mut [&mut [f32]], meter: &Meter) {
        let ch = ins.len().min(outs.len()).min(MAX_CH);
        let n = (0..ch).map(|c| ins[c].len().min(outs[c].len())).min().unwrap_or(0);
        let peak = |bufs: &[&[f32]]| bufs.iter().flat_map(|b| b.iter()).fold(0f32, |m, v| m.max(v.abs()));
        let in_peak = peak(ins);
        let (mut min_gate, mut min_limit) = (1f32, 1f32);
        match self.params {
            Params::Eq(_) => {
                for c in 0..ch {
                    self.chains[c].process(&ins[c][..n], &mut outs[c][..n]);
                }
            }
            Params::Volume { gain } => {
                let k = coef(0.01, self.rate);
                for i in 0..n {
                    self.vol_g += (gain - self.vol_g) * k;
                    for c in 0..ch {
                        outs[c][i] = ins[c][i] * self.vol_g;
                    }
                }
            }
            Params::Levels { gate, limit } => {
                let env_rel = 1.0 - coef(0.02, self.rate);
                let (open_k, close_k, lim_rel) = (coef(0.001, self.rate), coef(0.08, self.rate), coef(0.1, self.rate));
                let hold = (0.15 * self.rate) as u32;
                for i in 0..n {
                    let x = (0..ch).fold(0f32, |m, c| m.max(ins[c][i].abs()));
                    // Gate: opens at the threshold, closes 4 dB below it after a short hold, so
                    // the ends of words aren't chopped off.
                    let target = match gate {
                        None => 1.0,
                        Some(thr) => {
                            self.env = x.max(self.env * env_rel);
                            if self.env >= thr {
                                self.hold = hold;
                            } else if self.env < thr * 0.63 && self.hold > 0 {
                                self.hold -= 1;
                            }
                            if self.hold > 0 { 1.0 } else { 0.0 }
                        }
                    };
                    let k = if target > self.gate_g { open_k } else { close_k };
                    self.gate_g += (target - self.gate_g) * k;
                    min_gate = min_gate.min(self.gate_g);
                    // Limiter: instant attack (never lets a sample over the ceiling), smooth release.
                    let y = x * self.gate_g;
                    if let Some(ceiling) = limit {
                        let want = if y > ceiling { ceiling / y } else { 1.0 };
                        if want < self.limit_g {
                            self.limit_g = want;
                        } else {
                            self.limit_g += (want - self.limit_g) * lim_rel;
                        }
                    } else {
                        self.limit_g = 1.0;
                    }
                    min_limit = min_limit.min(self.limit_g);
                    let g = self.gate_g * self.limit_g;
                    for c in 0..ch {
                        outs[c][i] = ins[c][i] * g;
                    }
                }
            }
        }
        let out_peak = outs.iter().take(ch).flat_map(|b| b[..n].iter()).fold(0f32, |m, v| m.max(v.abs()));
        meter.record(in_peak, out_peak, min_gate, min_limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bell(freq: f32, gain_db: f32, q: f32) -> Band {
        Band { kind: BandKind::Bell, freq, gain_db, q, enabled: true }
    }

    #[test]
    fn bell_has_its_gain_at_the_center_and_none_far_away() {
        let b = [bell(1000.0, 6.0, 1.0)];
        assert!((response_db(0.0, &b, 1000.0) - 6.0).abs() < 0.01);
        assert!(response_db(0.0, &b, 20.0).abs() < 0.1);
    }

    #[test]
    fn shelves_and_passes_behave() {
        let low = Band { kind: BandKind::LowShelf, freq: 200.0, gain_db: -6.0, q: 0.707, enabled: true };
        assert!((response_db(0.0, &[low], 20.0) + 6.0).abs() < 0.2);
        assert!(response_db(0.0, &[low], 10000.0).abs() < 0.1);
        let hp = Band { kind: BandKind::HighPass, freq: 100.0, gain_db: 0.0, q: 0.707, enabled: true };
        assert!(response_db(0.0, &[hp], 20.0) < -20.0);
        assert!(response_db(0.0, &[hp], 5000.0).abs() < 0.1);
    }

    #[test]
    fn processing_a_sine_matches_the_predicted_response() {
        let rate = 48000.0;
        let b = [bell(1000.0, 12.0, 2.0)];
        let mut chain = Chain::default();
        chain.set(&EqParams::from_spec(-3.0, &b), rate);
        let input: Vec<f32> = (0..48000).map(|i| (2.0 * PI * 1000.0 * i as f64 / rate).sin() as f32).collect();
        let mut output = vec![0.0; input.len()];
        // Process in PipeWire-sized blocks to exercise state carry-over.
        for (i, o) in input.chunks(256).zip(output.chunks_mut(256)) {
            chain.process(i, o);
        }
        let peak = output[24000..].iter().fold(0f32, |m, v| m.max(v.abs()));
        let expect = 10f64.powf((12.0 - 3.0) / 20.0) as f32;
        assert!((peak - expect).abs() / expect < 0.01, "peak {peak}, expected {expect}");
    }

    fn run(p: &mut Processor, input: &[f32]) -> Vec<f32> {
        let meter = Meter::new();
        let mut out = vec![0.0; input.len()];
        for (i, o) in input.chunks(256).zip(out.chunks_mut(256)) {
            p.process(&[i], &mut [o], &meter);
        }
        out
    }

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / 48000.0).sin()).collect()
    }

    #[test]
    fn limiter_never_lets_a_sample_over_the_ceiling() {
        let ceiling = db_to_lin(-6.0);
        let mut p = Processor::new(Params::Levels { gate: None, limit: Some(ceiling) });
        p.set(Params::Levels { gate: None, limit: Some(ceiling) }, 48000.0);
        let out = run(&mut p, &sine(440.0, 1.0, 48000));
        let peak = out.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak <= ceiling + 1e-6, "peak {peak} over {ceiling}");
        assert!(peak > ceiling * 0.95, "limited too much: {peak}");
        // Quiet signals pass untouched.
        let quiet = sine(440.0, 0.1, 48000);
        let out = run(&mut p, &quiet);
        assert!((out[47000] - quiet[47000]).abs() < 1e-3);
    }

    #[test]
    fn gate_mutes_quiet_signal_and_passes_loud_signal() {
        let params = Params::Levels { gate: Some(db_to_lin(-40.0)), limit: None };
        let mut p = Processor::new(params);
        p.set(params, 48000.0);
        let hiss = run(&mut p, &sine(3000.0, db_to_lin(-55.0), 48000));
        assert!(hiss[40000..].iter().all(|v| v.abs() < 1e-4), "hiss got through");
        let voice = run(&mut p, &sine(300.0, db_to_lin(-20.0), 48000));
        let peak = voice[24000..].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((peak - db_to_lin(-20.0)).abs() < 1e-3, "voice changed: {peak}");
    }

    #[test]
    fn volume_multiplies() {
        let params = Params::Volume { gain: 0.5 };
        let mut p = Processor::new(params);
        p.set(params, 48000.0);
        let out = run(&mut p, &vec![0.8; 4800]);
        assert!((out[4799] - 0.4).abs() < 1e-4);
    }

    #[test]
    fn disabled_bands_do_nothing() {
        let mut b = bell(500.0, 10.0, 1.0);
        b.enabled = false;
        assert!(response_db(0.0, &[b], 500.0).abs() < 1e-9);
    }
}
