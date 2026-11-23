//! libopus, loaded when voice chat starts (`libopus.so.0`, or the platform's name), so a machine without
//! it still starts the game and only loses voice chat. The calls and constants are libopus's public API.
use libloading::Library;
use std::ffi::c_void;
use std::sync::Arc;

pub const SAMPLE_RATE: i32 = 48_000;
/// 20 ms of mono audio, what the mod sends and expects.
pub const FRAME: usize = 960;

const OPUS_APPLICATION_VOIP: i32 = 2048;
const OPUS_APPLICATION_AUDIO: i32 = 2049;
const OPUS_APPLICATION_RESTRICTED_LOWDELAY: i32 = 2051;
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: i32 = 4014;
const OPUS_SET_INBAND_FEC_REQUEST: i32 = 4012;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Application {
    Voip,
    Audio,
    LowDelay,
}

impl Application {
    fn raw(self) -> i32 {
        match self {
            Application::Voip => OPUS_APPLICATION_VOIP,
            Application::Audio => OPUS_APPLICATION_AUDIO,
            Application::LowDelay => OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        }
    }
}

type EncCreate = unsafe extern "C" fn(i32, i32, i32, *mut i32) -> *mut c_void;
type EncEncode = unsafe extern "C" fn(*mut c_void, *const i16, i32, *mut u8, i32) -> i32;
type EncCtl = unsafe extern "C" fn(*mut c_void, i32, ...) -> i32;
type Destroy = unsafe extern "C" fn(*mut c_void);
type DecCreate = unsafe extern "C" fn(i32, i32, *mut i32) -> *mut c_void;
type DecDecode = unsafe extern "C" fn(*mut c_void, *const u8, i32, *mut i16, i32, i32) -> i32;

/// The loaded library and its entry points. Shared by every encoder and decoder.
pub struct Opus {
    enc_create: EncCreate,
    enc_encode: EncEncode,
    enc_ctl: EncCtl,
    enc_destroy: Destroy,
    dec_create: DecCreate,
    dec_decode: DecDecode,
    dec_destroy: Destroy,
    _lib: Library,
}

impl Opus {
    pub fn load() -> Result<Arc<Self>, String> {
        let names = ["libopus.so.0", "libopus.so", "libopus.0.dylib", "libopus.dylib", "opus.dll"];
        let lib = names
            .iter()
            // SAFETY: loading libopus runs its (trivial) initialisers.
            .find_map(|n| unsafe { Library::new(n) }.ok())
            .ok_or_else(|| "libopus was not found (install libopus0)".to_string())?;
        // SAFETY: the symbols are libopus's, with the signatures its header declares.
        unsafe {
            macro_rules! sym {
                ($name:literal, $ty:ty) => {
                    *lib.get::<$ty>(concat!($name, "\0").as_bytes()).map_err(|e| format!("{}: {e}", $name))?
                };
            }
            Ok(Arc::new(Self {
                enc_create: sym!("opus_encoder_create", EncCreate),
                enc_encode: sym!("opus_encode", EncEncode),
                enc_ctl: sym!("opus_encoder_ctl", EncCtl),
                enc_destroy: sym!("opus_encoder_destroy", Destroy),
                dec_create: sym!("opus_decoder_create", DecCreate),
                dec_decode: sym!("opus_decode", DecDecode),
                dec_destroy: sym!("opus_decoder_destroy", Destroy),
                _lib: lib,
            }))
        }
    }
}

pub struct Encoder {
    lib: Arc<Opus>,
    ptr: *mut c_void,
}

// SAFETY: an encoder is used from one thread at a time; libopus state has no thread affinity.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(lib: &Arc<Opus>, app: Application) -> Result<Self, String> {
        let mut err = 0;
        // SAFETY: valid arguments; `err` is written by the call.
        let ptr = unsafe { (lib.enc_create)(SAMPLE_RATE, 1, app.raw(), &mut err) };
        if ptr.is_null() || err != 0 { return Err(format!("opus_encoder_create: {err}")); }
        // As the mod does: in-band FEC on, expecting 5 % loss.
        // SAFETY: a live encoder and int arguments, as the variadic call takes.
        unsafe {
            (lib.enc_ctl)(ptr, OPUS_SET_INBAND_FEC_REQUEST, 1i32);
            (lib.enc_ctl)(ptr, OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5i32);
        }
        Ok(Self { lib: lib.clone(), ptr })
    }

    /// One 20 ms frame of mono samples to an Opus packet.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, i32> {
        assert_eq!(pcm.len(), FRAME, "an Opus frame is 960 samples");
        let mut out = vec![0u8; 1275];
        // SAFETY: the buffers are as long as the lengths passed.
        let n = unsafe { (self.lib.enc_encode)(self.ptr, pcm.as_ptr(), FRAME as i32, out.as_mut_ptr(), out.len() as i32) };
        if n < 0 { return Err(n); }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the encoder was created by this library and is dropped once.
        unsafe { (self.lib.enc_destroy)(self.ptr) }
    }
}

pub struct Decoder {
    lib: Arc<Opus>,
    ptr: *mut c_void,
}

// SAFETY: as `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(lib: &Arc<Opus>) -> Result<Self, String> {
        let mut err = 0;
        // SAFETY: valid arguments; `err` is written by the call.
        let ptr = unsafe { (lib.dec_create)(SAMPLE_RATE, 1, &mut err) };
        if ptr.is_null() || err != 0 { return Err(format!("opus_decoder_create: {err}")); }
        Ok(Self { lib: lib.clone(), ptr })
    }

    fn run(&mut self, data: Option<&[u8]>, fec: bool) -> Result<Vec<i16>, i32> {
        let mut out = vec![0i16; FRAME];
        let (p, len) = match data { Some(d) => (d.as_ptr(), d.len() as i32), None => (std::ptr::null(), 0) };
        // SAFETY: `data` and `out` are as long as the lengths passed; null with length 0 asks for concealment.
        let n = unsafe { (self.lib.dec_decode)(self.ptr, p, len, out.as_mut_ptr(), FRAME as i32, fec as i32) };
        if n < 0 { return Err(n); }
        out.truncate(n as usize);
        out.resize(FRAME, 0);
        Ok(out)
    }

    /// A packet's frame.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<i16>, i32> { self.run(Some(data), false) }

    /// A lost frame, guessed (packet-loss concealment).
    pub fn conceal(&mut self) -> Result<Vec<i16>, i32> { self.run(None, false) }

    /// The frame before `next`, recovered from the forward error correction in `next`.
    pub fn recover(&mut self, next: &[u8]) -> Result<Vec<i16>, i32> { self.run(Some(next), true) }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the decoder was created by this library and is dropped once.
        unsafe { (self.lib.dec_destroy)(self.ptr) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frame: usize, hz: f32) -> Vec<i16> {
        (0..FRAME).map(|i| {
            let t = (frame * FRAME + i) as f32 / SAMPLE_RATE as f32;
            ((t * hz * std::f32::consts::TAU).sin() * 12000.0) as i16
        }).collect()
    }

    fn energy(s: &[i16]) -> f64 { s.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / s.len() as f64 }

    #[test]
    fn a_tone_survives_the_codec() {
        let Ok(lib) = Opus::load() else { eprintln!("libopus is not installed here; skipped"); return };
        for app in [Application::Voip, Application::Audio, Application::LowDelay] {
            let mut enc = Encoder::new(&lib, app).unwrap();
            let mut dec = Decoder::new(&lib).unwrap();
            let mut last = Vec::new();
            for f in 0..15 {
                let packet = enc.encode(&sine(f, 440.0)).unwrap();
                assert!(!packet.is_empty() && packet.len() <= 1275);
                last = dec.decode(&packet).unwrap();
                assert_eq!(last.len(), FRAME);
            }
            // After a few frames the decoder is settled: about the energy of the input tone.
            let ratio = energy(&last) / energy(&sine(14, 440.0));
            assert!((0.5..1.6).contains(&ratio), "{app:?}: energy ratio {ratio}");
        }
    }

    #[test]
    fn a_lost_frame_is_concealed_and_fec_recovers_it() {
        let Ok(lib) = Opus::load() else { return };
        let mut enc = Encoder::new(&lib, Application::Voip).unwrap();
        let mut dec = Decoder::new(&lib).unwrap();
        let mut packets = Vec::new();
        for f in 0..6 {
            packets.push(enc.encode(&sine(f, 300.0)).unwrap());
        }
        for p in &packets[..4] { dec.decode(p).unwrap(); }
        // Frame 4 is lost: guess it, or take it from frame 5's FEC.
        let guessed = dec.conceal().unwrap();
        assert_eq!(guessed.len(), FRAME);
        let mut dec2 = Decoder::new(&lib).unwrap();
        for p in &packets[..4] { dec2.decode(p).unwrap(); }
        let recovered = dec2.recover(&packets[5]).unwrap();
        assert_eq!(recovered.len(), FRAME);
        assert!(energy(&recovered) > 0.0);
    }

    #[test]
    fn silence_encodes_small_and_decodes_quiet() {
        let Ok(lib) = Opus::load() else { return };
        let mut enc = Encoder::new(&lib, Application::Voip).unwrap();
        let mut dec = Decoder::new(&lib).unwrap();
        let mut last = Vec::new();
        for _ in 0..10 {
            let p = enc.encode(&vec![0i16; FRAME]).unwrap();
            assert!(p.len() < 40, "silence packet {} bytes", p.len());
            last = dec.decode(&p).unwrap();
        }
        assert!(energy(&last) < 4.0);
    }
}
