/// Dialogue enhancement with no lookahead or resampling. The wet mix ramps
/// across toggles, so changing the setting does not interrupt the device.
pub(super) struct Voice {
    blend: f32,
    ramp: f32,
    bass_alpha: f32,
    speech_alpha: f32,
    envelope_alpha: f32,
    bass: f32,
    speech: f32,
    envelope: f32,
    center_level: f32,
    mid_level: f32,
}

impl Voice {
    pub fn new(rate: f32) -> Self {
        let pole = |hz: f32| 1.0 - (-std::f32::consts::TAU * hz / rate).exp();
        Self {
            blend: 0.0,
            ramp: 1.0 / (rate * 0.025),
            bass_alpha: pole(170.0),
            speech_alpha: pole(3900.0),
            envelope_alpha: pole(18.0),
            bass: 0.0,
            speech: 0.0,
            envelope: 0.0,
            center_level: 0.0,
            mid_level: 0.0,
        }
    }

    pub fn process(
        &mut self,
        dry: [f32; 2],
        front: [f32; 2],
        center: Option<f32>,
        enabled: bool,
    ) -> [f32; 2] {
        self.blend = if enabled {
            (self.blend + self.ramp).min(1.0)
        } else {
            (self.blend - self.ramp).max(0.0)
        };
        if self.blend == 0.0 {
            return dry;
        }
        let mid = (front[0] + front[1]) * 0.5;
        let side = (front[0] - front[1]) * 0.5;
        self.mid_level += self.envelope_alpha * (mid.abs() - self.mid_level);
        self.center_level +=
            self.envelope_alpha * (center.unwrap_or_default().abs() - self.center_level);
        let center_weight = if center.is_some() {
            self.center_level / (self.center_level + 0.25 * self.mid_level + 0.004)
        } else {
            0.0
        };
        let dialogue = mid + (center.unwrap_or(mid) - mid) * center_weight;
        self.bass += self.bass_alpha * (dialogue - self.bass);
        self.speech += self.speech_alpha * (dialogue - self.bass - self.speech);
        self.envelope += self.envelope_alpha * (self.speech.abs() - self.envelope);
        let presence = self.speech * (0.85 / (1.0 + 2.5 * self.envelope));
        let voice = dialogue * (0.78 + 0.12 * center_weight) + presence;
        let ambient = mid * 0.28 * center_weight;
        let wet = [voice + ambient + side * 0.42, voice + ambient - side * 0.42];
        std::array::from_fn(|i| dry[i] + (wet[i] - dry[i]) * self.blend)
    }

    pub fn limit(&self, amplified: f32, dry: f32) -> f32 {
        let magnitude = amplified.abs();
        if self.blend == 0.0 || magnitude <= 0.80 {
            return dry;
        }
        let compressed = 1.0 - 0.04 / (0.20 + magnitude - 0.80);
        (dry + (compressed.copysign(amplified) - dry) * self.blend).clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boosts_dialogue_and_reduces_stereo_bed_without_clipping() {
        let mut dsp = Voice::new(48_000.0);
        for i in 0..48_000 {
            let phase = std::f32::consts::TAU * i as f32 / 48_000.0;
            let dialogue = (phase * 1000.0).sin() * 0.25;
            let bed = (phase * 90.0).sin() * 0.5;
            let front = [dialogue + bed, dialogue - bed];
            let [l, r] = dsp.process(front, front, None, true);
            if i > 4800 {
                assert!((l - r).abs() <= (front[0] - front[1]).abs() * 0.43 + 0.0001);
            }
            assert!(dsp.limit(l * 2.0, (l * 2.0).clamp(-1.0, 1.0)).is_finite());
        }
        assert!(dsp.limit(1.8, 1.0) < 1.0 && dsp.limit(1.8, 1.0) > 0.95);
    }

    #[test]
    fn uses_centre_when_present_and_returns_to_dry_mix() {
        let mut dsp = Voice::new(48_000.0);
        let mut peak: f32 = 0.0;
        for i in 0..4000 {
            let center = (std::f32::consts::TAU * i as f32 / 48.0).sin() * 0.25;
            let out = dsp.process([0.0; 2], [0.0; 2], Some(center), true);
            peak = peak.max(out[0].abs());
            assert_eq!(out[0], out[1]);
        }
        assert!(peak > 0.25);
        for _ in 0..1500 {
            dsp.process([0.2, -0.2], [0.2, -0.2], None, false);
        }
        assert_eq!(
            dsp.process([0.2, -0.2], [0.2, -0.2], None, false),
            [0.2, -0.2]
        );
    }
}
