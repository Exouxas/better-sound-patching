//! Equalizer math: biquad filters (RBJ audio EQ cookbook), their frequency response for the
//! curve editor, and a per-channel processing chain. No PipeWire here, so it's unit-testable.

use crate::model::{Band, BandKind, MAX_BANDS};
use std::f64::consts::PI;

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

    #[test]
    fn disabled_bands_do_nothing() {
        let mut b = bell(500.0, 10.0, 1.0);
        b.enabled = false;
        assert!(response_db(0.0, &[b], 500.0).abs() < 1e-9);
    }
}
