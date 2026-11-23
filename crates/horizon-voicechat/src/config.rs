//! The voice chat settings, `config.txt` in the mod's data folder (`key:value`, one per line), written by the
//! mod's menu. The keys that talk are the mod's, not stored here.

/// The Java mod's "3D Audio".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioType {
    /// Voices pan left and right and fade with distance.
    Normal,
    /// Less panning.
    Reduced,
    /// Fade with distance only, no panning.
    Off,
}

impl AudioType {
    /// How far a voice off to the side is pulled to that side (0 is not at all).
    pub fn pan_strength(self) -> f32 {
        match self { AudioType::Normal => 0.85, AudioType::Reduced => 0.4, AudioType::Off => 0.0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub enabled: bool,
    /// Talk by holding the push-to-talk key (the default) rather than by voice activation.
    pub push_to_talk: bool,
    /// Voice chat volume, 0 to 3, 1 as the voices come.
    pub volume: f32,
    /// Microphone gain in dB, -40 to 24.
    pub mic_gain_db: f32,
    /// Voice activation opens the microphone above this level, in dB (0 is full scale); -127 to 0.
    pub activation_db: f32,
    pub audio_type: AudioType,
    /// Audio device names; empty is the system's default.
    pub input_device: String,
    pub output_device: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: true, push_to_talk: true, volume: 1.0, mic_gain_db: 0.0, activation_db: -50.0, audio_type: AudioType::Normal,
               input_device: String::new(), output_device: String::new() }
    }
}

impl Config {
    pub fn parse(text: &str) -> Self {
        let mut c = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let v = v.trim();
            match k.trim() {
                "enabled" => c.enabled = v.eq_ignore_ascii_case("true"),
                "activation" => c.push_to_talk = !v.eq_ignore_ascii_case("voice"),
                "volume" => c.volume = v.parse().map_or(c.volume, |x: f32| x.clamp(0.0, 3.0)),
                "gainDb" => c.mic_gain_db = v.parse().map_or(c.mic_gain_db, |x: f32| x.clamp(-40.0, 24.0)),
                "activationDb" => c.activation_db = v.parse().map_or(c.activation_db, |x: f32| x.clamp(-127.0, 0.0)),
                "audioType" => c.audio_type = match v { "reduced" => AudioType::Reduced, "off" => AudioType::Off, _ => AudioType::Normal },
                "inputDevice" => c.input_device = v.to_owned(),
                "outputDevice" => c.output_device = v.to_owned(),
                _ => {}
            }
        }
        c
    }

    pub fn render(&self) -> String {
        format!(
            "enabled:{}\nactivation:{}\nvolume:{:.2}\ngainDb:{:.1}\nactivationDb:{:.0}\naudioType:{}\ninputDevice:{}\noutputDevice:{}\n",
            self.enabled, if self.push_to_talk { "ptt" } else { "voice" }, self.volume, self.mic_gain_db, self.activation_db,
            match self.audio_type { AudioType::Normal => "normal", AudioType::Reduced => "reduced", AudioType::Off => "off" },
            self.input_device, self.output_device,
        )
    }

    /// The microphone's gain as a factor.
    pub fn mic_gain(&self) -> f32 { 10f32.powf(self.mic_gain_db / 20.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_round_trips() {
        let mut c = Config::default();
        c.push_to_talk = false;
        c.activation_db = -42.0;
        c.volume = 1.5;
        c.mic_gain_db = 6.0;
        c.audio_type = AudioType::Reduced;
        c.input_device = "USB Mic".into();
        assert_eq!(Config::parse(&c.render()), c);
    }

    #[test]
    fn junk_and_out_of_range_values_are_tamed() {
        let c = Config::parse("activationDb:-500\nvolume:9999\ngainDb:abc\nbogus\nenabled:false\naudioType:weird\n");
        assert_eq!(c.activation_db, -127.0);
        assert_eq!(c.volume, 3.0);
        assert_eq!(c.mic_gain_db, 0.0);
        assert_eq!(c.audio_type, AudioType::Normal);
        assert!(!c.enabled);
    }

    #[test]
    fn gain_in_db_is_a_factor() {
        let mut c = Config::default();
        assert_eq!(c.mic_gain(), 1.0);
        c.mic_gain_db = 20.0;
        assert!((c.mic_gain() - 10.0).abs() < 1e-4);
        c.mic_gain_db = -20.0;
        assert!((c.mic_gain() - 0.1).abs() < 1e-4);
    }
}
