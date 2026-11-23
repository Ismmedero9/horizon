//! Linear resampling between a device's rate and voice chat's 48 kHz. Voice does not need better, and
//! a fixed step keeps it cheap enough for an audio callback.

/// Mono, any rate to 48 kHz (the microphone).
pub struct ToVoice {
    step: f64,
    pos: f64,
    prev: f32,
    primed: bool,
}

impl ToVoice {
    pub fn new(from_rate: u32) -> Self { Self { step: from_rate as f64 / 48_000.0, pos: 0.0, prev: 0.0, primed: false } }

    /// Appends the input's samples at 48 kHz to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        for &x in input {
            if !self.primed { self.prev = x; self.primed = true; self.pos = 0.0; continue; }
            // `pos` is where the next output sits between `prev` (0) and `x` (1).
            while self.pos < 1.0 {
                out.push(self.prev + (x - self.prev) * self.pos as f32);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = x;
        }
    }
}

/// Stereo, 48 kHz to any rate (the speakers). `provide(n)` returns the next `n` frames at 48 kHz.
pub struct FromVoice {
    step: f64,
    pos: f64,
    cur: [f32; 2],
    next: [f32; 2],
    primed: bool,
}

impl FromVoice {
    pub fn new(to_rate: u32) -> Self { Self { step: 48_000.0 / to_rate as f64, pos: 0.0, cur: [0.0; 2], next: [0.0; 2], primed: false } }

    pub fn fill(&mut self, out: &mut [[f32; 2]], mut provide: impl FnMut(usize) -> Vec<[f32; 2]>) {
        if (self.step - 1.0).abs() < 1e-9 {
            let src = provide(out.len());
            for (o, s) in out.iter_mut().zip(src) { *o = s; }
            return;
        }
        // The source frames this call consumes, with one more to interpolate toward.
        let need = (((out.len() as f64) * self.step + self.pos).ceil() as usize + 1).max(2);
        let mut src = provide(need).into_iter();
        if !self.primed {
            self.cur = src.next().unwrap_or([0.0; 2]);
            self.next = src.next().unwrap_or([0.0; 2]);
            self.primed = true;
        }
        for o in out.iter_mut() {
            while self.pos >= 1.0 {
                self.cur = self.next;
                self.next = src.next().unwrap_or(self.next);
                self.pos -= 1.0;
            }
            let t = self.pos as f32;
            *o = [self.cur[0] + (self.next[0] - self.cur[0]) * t, self.cur[1] + (self.next[1] - self.cur[1]) * t];
            self.pos += self.step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_microphone_comes_out_at_the_right_length_and_pitch() {
        for rate in [44_100u32, 16_000, 96_000, 48_000] {
            let mut r = ToVoice::new(rate);
            let mut out = Vec::new();
            let freq = 440.0;
            let input: Vec<f32> = (0..rate as usize).map(|i| (i as f32 / rate as f32 * freq * std::f32::consts::TAU).sin()).collect();
            // In chunks, as a callback delivers it.
            for chunk in input.chunks(441) { r.process(chunk, &mut out); }
            assert!((out.len() as i64 - 48_000).abs() <= 6, "{rate}: {} samples for one second", out.len());
            // Counting zero crossings gives the frequency: about 2 per cycle.
            let crossings = out.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
            assert!((crossings as i32 - 440).abs() <= 2, "{rate}: {crossings} cycles");
        }
    }

    #[test]
    fn the_speakers_get_the_rate_they_asked_for_without_gaps() {
        for rate in [44_100u32, 96_000, 48_000] {
            let mut r = FromVoice::new(rate);
            let mut produced = 0usize;
            let mut phase = 0usize;
            let mut all = Vec::new();
            // A 300 Hz tone at 48 kHz, in as many calls of 512 device frames as it takes for a second.
            while produced < rate as usize {
                let mut out = vec![[0.0f32; 2]; 512];
                r.fill(&mut out, |n| {
                    (0..n).map(|_| { let v = (phase as f32 / 48_000.0 * 300.0 * std::f32::consts::TAU).sin(); phase += 1; [v, v] }).collect()
                });
                all.extend(out.iter().map(|f| f[0]));
                produced += 512;
            }
            let crossings = all.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
            let seconds = all.len() as f32 / rate as f32;
            assert!(((crossings as f32 / seconds) - 300.0).abs() < 4.0, "{rate}: {} Hz", crossings as f32 / seconds);
            // No sample jumps: neighbouring samples of a 300 Hz tone differ by little.
            let jump = all.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            assert!(jump < 0.2, "{rate}: a jump of {jump}");
        }
    }
}
