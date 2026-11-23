//! Other players' voices: each speaker's Opus stream is decoded (concealing lost packets, recovering one
//! from the next packet's error correction), held in a small jitter buffer, and mixed to stereo with the
//! distance falloff the mod uses (linear, silent past the packet's distance) and a left-right pan.
use crate::net::Sound;
use crate::opus::{Decoder, Opus, FRAME};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Samples held before a speaker starts playing, and the most it will hold (50 frames' worth is a second).
const PRIME: usize = 3 * FRAME;
const MAX_QUEUE: usize = 15 * FRAME;
/// A gap bigger than this many frames resets the decoder instead of concealing.
const MAX_CONCEAL: i64 = 8;

#[derive(Default, Clone)]
pub struct World {
    /// The listener's ears: the camera position and its yaw in degrees (Minecraft's: 0 faces +z).
    pub listener: [f64; 3],
    pub yaw: f32,
    /// Where the players the client can see are, by uuid.
    pub players: HashMap<u128, [f64; 3]>,
}

struct Speaker {
    decoder: Decoder,
    last_sequence: Option<i64>,
    queue: VecDeque<f32>,
    primed: bool,
    distance: f32,
    group: bool,
    location: Option<[f64; 3]>,
    last_heard: Instant,
}

pub struct Mixer {
    lib: Arc<Opus>,
    speakers: HashMap<(u128, bool), Speaker>,
    /// The output volume, 1.0 as it comes.
    pub volume: f32,
    /// Deafened: nothing is played.
    pub deafened: bool,
    /// How far a voice off to the side is panned (`AudioType::pan_strength`).
    pub pan_strength: f32,
    /// Each player's own volume factor, by uuid.
    pub player_volumes: HashMap<u128, f32>,
}

/// Left and right gain for a source: linear falloff to nothing at `distance`, none at all past it, and an
/// equal-power pan by where the source is across the listener's view. `None` is silence.
pub fn spatial(listener: [f64; 3], yaw_deg: f32, source: [f64; 3], distance: f32) -> Option<(f32, f32)> {
    spatial_with(listener, yaw_deg, source, distance, 0.85)
}

/// [`spatial`] with the pan strength a 3D audio setting gives (0 is no pan, the voice in both ears alike).
pub fn spatial_with(listener: [f64; 3], yaw_deg: f32, source: [f64; 3], distance: f32, pan: f32) -> Option<(f32, f32)> {
    let (dx, dy, dz) = (source[0] - listener[0], source[1] - listener[1], source[2] - listener[2]);
    let d = (dx * dx + dy * dy + dz * dz).sqrt() as f32;
    if distance <= 0.0 || d > distance + 1.0 { return None; }
    let volume = 1.0 - d.min(distance) / distance;
    // Minecraft: yaw 0 faces +z, and the right-hand side is (-cos yaw, -sin yaw).
    let y = (yaw_deg as f64).to_radians();
    let right = [-y.cos(), -y.sin()];
    let flat = (dx * dx + dz * dz).sqrt();
    let lateral = if flat < 0.5 { 0.0 } else { ((dx * right[0] + dz * right[1]) / flat) as f32 };
    let angle = (lateral * pan + 1.0) * std::f32::consts::FRAC_PI_4;
    let k = std::f32::consts::SQRT_2 * volume;
    Some((angle.cos() * k, angle.sin() * k))
}

impl Mixer {
    pub fn new(lib: Arc<Opus>) -> Self { Self { lib, speakers: HashMap::new(), volume: 1.0, deafened: false, pan_strength: 0.85, player_volumes: HashMap::new() } }

    fn enqueue(speaker: &mut Speaker, pcm: Vec<i16>) {
        speaker.queue.extend(pcm.into_iter().map(|s| s as f32 / 32768.0));
        while speaker.queue.len() > MAX_QUEUE { speaker.queue.pop_front(); }
    }

    /// A speaker's sound packet: decoded and queued. An empty frame means they stopped talking.
    pub fn push(&mut self, sound: &Sound, now: Instant) -> Result<(), String> {
        let key = (sound.sender, sound.group);
        if !self.speakers.contains_key(&key) {
            let decoder = Decoder::new(&self.lib)?;
            self.speakers.insert(key, Speaker { decoder, last_sequence: None, queue: VecDeque::new(), primed: false,
                distance: sound.distance, group: sound.group, location: sound.location, last_heard: now });
        }
        let lib = self.lib.clone();
        let sp = self.speakers.get_mut(&key).unwrap();
        sp.distance = sound.distance;
        sp.location = sound.location;
        sp.last_heard = now;
        if sound.opus.is_empty() {
            sp.last_sequence = None;
            sp.decoder = Decoder::new(&lib)?;
            return Ok(());
        }
        if let Some(last) = sp.last_sequence {
            if sound.sequence <= last { return Ok(()); } // late or repeated
            let gap = sound.sequence - last - 1;
            if gap > MAX_CONCEAL {
                sp.decoder = Decoder::new(&lib)?;
                sp.queue.clear();
            } else if gap > 0 {
                for _ in 0..gap - 1 {
                    if let Ok(pcm) = sp.decoder.conceal() { Self::enqueue(sp, pcm); }
                }
                if let Ok(pcm) = sp.decoder.recover(&sound.opus) { Self::enqueue(sp, pcm); }
            }
        }
        let pcm = sp.decoder.decode(&sound.opus).map_err(|e| format!("opus decode: {e}"))?;
        Self::enqueue(sp, pcm);
        sp.last_sequence = Some(sound.sequence);
        Ok(())
    }

    /// Your own microphone, played back: a speaker that is not placed anywhere.
    pub fn push_local(&mut self, pcm: Vec<i16>) {
        let now = Instant::now();
        let key = (u128::MAX, true);
        if !self.speakers.contains_key(&key) {
            let Ok(decoder) = Decoder::new(&self.lib) else { return };
            self.speakers.insert(key, Speaker { decoder, last_sequence: None, queue: VecDeque::new(), primed: false, distance: 0.0, group: true, location: None, last_heard: now });
        }
        let sp = self.speakers.get_mut(&key).unwrap();
        sp.last_heard = now;
        Self::enqueue(sp, pcm);
    }

    /// Fills `out` (interleaved stereo at 48 kHz) with every voice mixed. Overwrites it.
    pub fn mix(&mut self, out: &mut [f32], world: &World) {
        out.fill(0.0);
        let frames = out.len() / 2;
        for ((sender, _), sp) in self.speakers.iter_mut() {
            if !sp.primed && sp.queue.len() >= PRIME { sp.primed = true; }
            if !sp.primed { continue; }
            let take = frames.min(sp.queue.len());
            let samples: Vec<f32> = sp.queue.drain(..take).collect();
            if sp.queue.is_empty() { sp.primed = false; } // underrun: refill before playing on
            if self.deafened { continue; }
            let gains = if sp.group {
                Some((1.0, 1.0))
            } else {
                let at = sp.location.or_else(|| world.players.get(sender).copied());
                at.and_then(|p| spatial_with(world.listener, world.yaw, p, sp.distance, self.pan_strength))
            };
            let Some((l, r)) = gains else { continue };
            let own = self.player_volumes.get(sender).copied().unwrap_or(1.0);
            let (l, r) = (l * own, r * own);
            for (i, s) in samples.iter().enumerate() {
                out[2 * i] += s * l;
                out[2 * i + 1] += s * r;
            }
        }
        for v in out.iter_mut() { *v = (*v * self.volume).clamp(-1.0, 1.0); }
        self.speakers.retain(|_, sp| !sp.queue.is_empty() || sp.last_heard.elapsed() < Duration::from_secs(30));
    }

    /// The senders heard within `window`, for a "talking" display.
    pub fn talking(&self, now: Instant, window: Duration) -> Vec<u128> {
        self.speakers.iter()
            .filter(|(_, sp)| now.duration_since(sp.last_heard) < window && (sp.primed || !sp.queue.is_empty()))
            .map(|((sender, _), _)| *sender)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opus::{Application, Encoder};

    #[test]
    fn volume_falls_off_linearly_and_stops_past_the_distance() {
        let at = |x: f64| spatial([0.0; 3], 0.0, [x, 0.0, 0.0], 48.0);
        // Straight ahead is +z, so sources along x are to the side; the pan is checked below. Compare loudness.
        let loud = |g: Option<(f32, f32)>| g.map(|(l, r)| (l * l + r * r).sqrt());
        let near = loud(spatial([0.0; 3], 0.0, [0.0, 0.0, 0.1], 48.0)).unwrap();
        let mid = loud(spatial([0.0; 3], 0.0, [0.0, 0.0, 24.0], 48.0)).unwrap();
        assert!(near > mid && (mid / near - 0.5).abs() < 0.02, "half the distance, half the volume: {mid} vs {near}");
        assert!(at(48.5).is_some() && at(48.9).is_some(), "the mod keeps a block of grace");
        assert!(at(49.5).is_none(), "past the distance it is silent");
        assert!(spatial([0.0; 3], 0.0, [0.0, 0.0, 1.0], 0.0).is_none());
    }

    #[test]
    fn a_voice_on_the_right_is_louder_in_the_right_ear() {
        // Facing +z (yaw 0), the right-hand side is -x.
        let (l, r) = spatial([0.0; 3], 0.0, [-10.0, 0.0, 0.0], 48.0).unwrap();
        assert!(r > l * 2.0, "right {r} left {l}");
        let (l, r) = spatial([0.0; 3], 0.0, [10.0, 0.0, 0.0], 48.0).unwrap();
        assert!(l > r * 2.0, "left {l} right {r}");
        // Straight ahead or behind is centred, and turning the head moves the voice across.
        let (l, r) = spatial([0.0; 3], 0.0, [0.0, 0.0, 10.0], 48.0).unwrap();
        assert!((l - r).abs() < 1e-5);
        let (l, r) = spatial([0.0; 3], 90.0, [-10.0, 0.0, 0.0], 48.0).unwrap(); // facing -x: it is now ahead
        assert!((l - r).abs() < 1e-5);
        // On top of the listener there is no pan at all.
        let (l, r) = spatial([0.0; 3], 33.0, [0.1, 0.0, 0.1], 48.0).unwrap();
        assert!((l - r).abs() < 1e-5);
    }

    fn sound(sender: u128, sequence: i64, opus: Vec<u8>) -> Sound {
        Sound { channel: sender, sender, opus, sequence, distance: 48.0, whispering: false, category: None, location: None, group: false }
    }

    fn tone_packets(n: usize) -> Option<(Arc<Opus>, Vec<Vec<u8>>)> {
        let lib = Opus::load().ok()?;
        let mut enc = Encoder::new(&lib, Application::Voip).unwrap();
        let packets = (0..n).map(|f| {
            let pcm: Vec<i16> = (0..FRAME).map(|i| ((((f * FRAME + i) as f32) / 48000.0 * 440.0 * std::f32::consts::TAU).sin() * 12000.0) as i16).collect();
            enc.encode(&pcm).unwrap()
        }).collect();
        Some((lib, packets))
    }

    fn world_with(sender: u128, at: [f64; 3]) -> World {
        let mut w = World::default();
        w.players.insert(sender, at);
        w
    }

    #[test]
    fn a_speaker_plays_after_a_few_frames_are_buffered_and_stops_when_it_runs_dry() {
        let Some((lib, packets)) = tone_packets(6) else { return };
        let mut m = Mixer::new(lib);
        let (now, world) = (Instant::now(), world_with(7, [0.0, 0.0, 5.0]));
        let mut out = vec![0.0f32; 2 * FRAME];
        m.push(&sound(7, 0, packets[0].clone()), now).unwrap();
        m.push(&sound(7, 1, packets[1].clone()), now).unwrap();
        m.mix(&mut out, &world);
        assert!(out.iter().all(|&v| v == 0.0), "not primed yet");
        m.push(&sound(7, 2, packets[2].clone()), now).unwrap();
        m.push(&sound(7, 3, packets[3].clone()), now).unwrap();
        m.mix(&mut out, &world);
        assert!(out.iter().any(|&v| v.abs() > 0.05), "playing");
        assert!(m.talking(now, Duration::from_millis(400)).contains(&7));
        // Drain the rest: after the queue empties it goes quiet instead of looping.
        for _ in 0..4 { m.mix(&mut out, &world); }
        m.mix(&mut out, &world);
        assert!(out.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn late_and_repeated_packets_are_dropped_and_gaps_are_filled() {
        let Some((lib, packets)) = tone_packets(8) else { return };
        let mut m = Mixer::new(lib);
        let now = Instant::now();
        let q = |m: &Mixer| m.speakers[&(7, false)].queue.len();
        m.push(&sound(7, 0, packets[0].clone()), now).unwrap();
        assert_eq!(q(&m), FRAME);
        m.push(&sound(7, 0, packets[0].clone()), now).unwrap(); // a repeat
        m.push(&sound(7, 1, packets[1].clone()), now).unwrap();
        assert_eq!(q(&m), 2 * FRAME);
        // 2 and 3 are lost; 4 arrives: two frames are made up, then its own.
        m.push(&sound(7, 4, packets[4].clone()), now).unwrap();
        assert_eq!(q(&m), 5 * FRAME);
        m.push(&sound(7, 3, packets[3].clone()), now).unwrap(); // arrives late: ignored
        assert_eq!(q(&m), 5 * FRAME);
    }

    #[test]
    fn a_big_gap_starts_over_and_the_empty_frame_ends_the_stream() {
        let Some((lib, packets)) = tone_packets(3) else { return };
        let mut m = Mixer::new(lib);
        let now = Instant::now();
        m.push(&sound(7, 0, packets[0].clone()), now).unwrap();
        m.push(&sound(7, 100, packets[1].clone()), now).unwrap();
        assert_eq!(m.speakers[&(7, false)].queue.len(), FRAME, "the old audio is dropped, not concealed for 99 frames");
        m.push(&sound(7, 101, Vec::new()), now).unwrap();
        assert_eq!(m.speakers[&(7, false)].last_sequence, None);
        // The next stream may start from sequence 0 again.
        m.push(&sound(7, 0, packets[2].clone()), now).unwrap();
        assert_eq!(m.speakers[&(7, false)].queue.len(), 2 * FRAME);
    }

    #[test]
    fn a_player_out_of_range_or_unseen_is_silent_and_a_group_is_not_positional() {
        let Some((lib, packets)) = tone_packets(4) else { return };
        let mut m = Mixer::new(lib);
        let now = Instant::now();
        for i in 0..4 { m.push(&sound(7, i, packets[i as usize].clone()), now).unwrap(); }
        let mut out = vec![0.0f32; 2 * FRAME];
        m.mix(&mut out, &World::default()); // 7 is not in the world: not heard
        assert!(out.iter().all(|&v| v == 0.0));
        let mut g = sound(8, 0, packets[0].clone());
        g.group = true;
        let mut m = Mixer::new(m.lib.clone());
        for i in 0..4 { g.sequence = i; g.opus = packets[i as usize].clone(); m.push(&g, now).unwrap(); }
        m.mix(&mut out, &World::default());
        assert!(out.chunks(2).all(|f| (f[0] - f[1]).abs() < 1e-6) && out.iter().any(|&v| v.abs() > 0.05));
        m.deafened = true;
        for i in 4..8 { g.sequence = i; g.opus = packets[(i % 4) as usize].clone(); m.push(&g, now).unwrap(); }
        m.mix(&mut out, &World::default());
        assert!(out.iter().all(|&v| v == 0.0), "deafened");
    }
}
