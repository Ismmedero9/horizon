//! The microphone and the speakers, through cpal (as the game's own audio does). Each stream lives on a thread
//! of its own, which holds it until told to stop; a failure to open a device is recorded as the status's
//! error, not a crash.
use crate::engine::Shared;
use crate::resample::{FromVoice, ToVoice};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl AudioHandle {
    pub fn stop(self) {
        self.stop.store(true, Relaxed);
        for t in self.threads { let _ = t.join(); }
    }
}

/// The names of the input and output devices, for the menu.
pub(crate) fn devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let names = |it: Option<Box<dyn Iterator<Item = cpal::Device>>>| it.map(|d| d.filter_map(|d| d.name().ok()).collect()).unwrap_or_default();
    let inputs = host.input_devices().ok().map(|d| Box::new(d) as Box<dyn Iterator<Item = cpal::Device>>);
    let outputs = host.output_devices().ok().map(|d| Box::new(d) as Box<dyn Iterator<Item = cpal::Device>>);
    (names(inputs), names(outputs))
}

pub(crate) fn start(shared: Arc<Shared>, input: &str, output: &str) -> AudioHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for (name, is_input) in [(input.to_owned(), true), (output.to_owned(), false)] {
        let (shared, stop) = (shared.clone(), stop.clone());
        let t = std::thread::Builder::new().name(if is_input { "voicechat-mic" } else { "voicechat-speakers" }.into()).spawn(move || {
            let result = if is_input { run_input(&shared, &name, &stop) } else { run_output(&shared, &name, &stop) };
            if let Err(e) = result {
                shared.status.lock().unwrap().error = Some(format!("{}: {e}", if is_input { "microphone" } else { "speakers" }));
            }
        }).expect("audio thread");
        threads.push(t);
    }
    AudioHandle { stop, threads }
}

fn find(host: &cpal::Host, name: &str, input: bool) -> Option<cpal::Device> {
    if name.is_empty() {
        return if input { host.default_input_device() } else { host.default_output_device() };
    }
    let mut devices = if input { host.input_devices().ok()? } else { host.output_devices().ok()? };
    devices.find(|d| d.name().ok().as_deref() == Some(name))
        .or_else(|| if input { host.default_input_device() } else { host.default_output_device() })
}

fn run_input(shared: &Arc<Shared>, name: &str, stop: &AtomicBool) -> Result<(), String> {
    let host = cpal::default_host();
    let device = find(&host, name, true).ok_or("no microphone found")?;
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let (channels, rate) = (supported.channels() as usize, supported.sample_rate().0);
    let config = supported.config();
    let err = |e| eprintln!("voice chat microphone: {e}");
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&device, &config, shared.clone(), channels, rate, err),
        SampleFormat::I16 => build_input::<i16>(&device, &config, shared.clone(), channels, rate, err),
        SampleFormat::U16 => build_input::<u16>(&device, &config, shared.clone(), channels, rate, err),
        other => return Err(format!("unsupported sample format {other:?}")),
    }.map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    while !stop.load(Relaxed) { std::thread::sleep(Duration::from_millis(50)); }
    drop(stream);
    Ok(())
}

fn build_input<T>(device: &cpal::Device, config: &cpal::StreamConfig, shared: Arc<Shared>, channels: usize, rate: u32,
                  err: impl FnMut(cpal::StreamError) + Send + 'static) -> Result<cpal::Stream, cpal::BuildStreamError>
where T: SizedSample, f32: FromSample<T> {
    let mut resampler = ToVoice::new(rate);
    let mut scratch: Vec<f32> = Vec::new();
    let mut mono: Vec<f32> = Vec::new();
    device.build_input_stream(config, move |data: &[T], _| {
        mono.clear();
        for frame in data.chunks(channels) {
            let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
            mono.push(sum / channels as f32);
        }
        scratch.clear();
        resampler.process(&mono, &mut scratch);
        let gain = shared.config.lock().unwrap().mic_gain();
        let mut q = shared.mic.lock().unwrap();
        q.extend(scratch.iter().map(|s| ((s * gain).clamp(-1.0, 1.0) * 32767.0) as i16));
        while q.len() > 48_000 * 2 { q.pop_front(); }
    }, err, None)
}

fn run_output(shared: &Arc<Shared>, name: &str, stop: &AtomicBool) -> Result<(), String> {
    let host = cpal::default_host();
    let device = find(&host, name, false).ok_or("no speakers found")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let (channels, rate) = (supported.channels() as usize, supported.sample_rate().0);
    let config = supported.config();
    let err = |e| eprintln!("voice chat speakers: {e}");
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&device, &config, shared.clone(), channels, rate, err),
        SampleFormat::I16 => build_output::<i16>(&device, &config, shared.clone(), channels, rate, err),
        SampleFormat::U16 => build_output::<u16>(&device, &config, shared.clone(), channels, rate, err),
        other => return Err(format!("unsupported sample format {other:?}")),
    }.map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    while !stop.load(Relaxed) { std::thread::sleep(Duration::from_millis(50)); }
    drop(stream);
    Ok(())
}

fn build_output<T>(device: &cpal::Device, config: &cpal::StreamConfig, shared: Arc<Shared>, channels: usize, rate: u32,
                   err: impl FnMut(cpal::StreamError) + Send + 'static) -> Result<cpal::Stream, cpal::BuildStreamError>
where T: SizedSample + FromSample<f32> {
    let mut resampler = FromVoice::new(rate);
    let mut frames: Vec<[f32; 2]> = Vec::new();
    device.build_output_stream(config, move |data: &mut [T], _| {
        let n = data.len() / channels;
        frames.clear();
        frames.resize(n, [0.0; 2]);
        let world = shared.world.lock().unwrap().clone();
        resampler.fill(&mut frames, |need| {
            let mut buf = vec![0.0f32; need * 2];
            if let Some(m) = shared.mixer.lock().unwrap().as_mut() { m.mix(&mut buf, &world); }
            buf.chunks(2).map(|c| [c[0], c[1]]).collect()
        });
        for (out, f) in data.chunks_mut(channels).zip(&frames) {
            for (c, o) in out.iter_mut().enumerate() {
                *o = T::from_sample(if c < 2 { f[c] } else { 0.0 });
            }
            if channels == 1 { out[0] = T::from_sample((f[0] + f[1]) * 0.5); }
        }
    }, err, None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn listing_devices_does_not_panic_even_with_none() {
        let (inputs, outputs) = super::devices();
        eprintln!("{} inputs, {} outputs", inputs.len(), outputs.len());
    }
}
