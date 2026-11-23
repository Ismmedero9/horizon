//! The voice chat client as the game uses it: hand it the plugin messages the server sends and the player
//! positions, tell it which keys are down, and take the plugin messages it wants sent. It owns the UDP
//! connection, the microphone, the speakers and the mixing, on threads of its own.
use crate::config::Config;
use crate::connection::{Connection, Event};
use crate::mixer::{Mixer, World};
use crate::net::Packet;
use crate::opus::{Application, Encoder, Opus, FRAME};
use crate::secret::{self, Codec, SecretPacket};
use std::collections::{HashMap, VecDeque};
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What the game can show about voice chat.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// Opus loaded and the engine is on.
    pub available: bool,
    pub connected: bool,
    /// The microphone is open and being sent.
    pub talking: bool,
    pub muted: bool,
    pub deafened: bool,
    /// Who is being heard right now.
    pub speakers: Vec<u128>,
    pub error: Option<String>,
}

pub(crate) struct Shared {
    pub config: Mutex<Config>,
    pub world: Mutex<World>,
    pub mixer: Mutex<Option<Mixer>>,
    pub status: Mutex<Status>,
    pub payloads: Mutex<VecDeque<(String, Vec<u8>)>>,
    /// The microphone, mono 48 kHz.
    pub mic: Mutex<VecDeque<i16>>,
    pub names: Mutex<HashMap<u128, String>>,
    pub groups: Mutex<crate::groups::Book>,
    /// When the secret was asked for, until the server answers.
    pub asked: Mutex<Option<Instant>>,
    /// The last microphone frame's level in dB, as f32 bits.
    pub level: std::sync::atomic::AtomicU32,
    pub mic_test: AtomicBool,
    pub ptt: AtomicBool,
    pub whisper: AtomicBool,
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
    pub stop: AtomicBool,
}

pub struct Engine {
    shared: Arc<Shared>,
    opus: Option<Arc<Opus>>,
    use_audio: bool,
    worker: Mutex<Option<(Arc<AtomicBool>, JoinHandle<()>)>>,
    #[cfg(feature = "audio")]
    audio: Mutex<Option<crate::audio::AudioHandle>>,
}

impl Engine {
    /// `use_audio` false leaves the microphone and speakers alone (tests feed and read them directly).
    pub fn new(config: Config, use_audio: bool) -> Self {
        let (opus, error) = match Opus::load() { Ok(o) => (Some(o), None), Err(e) => (None, Some(e)) };
        let mixer = opus.clone().map(Mixer::new);
        let shared = Arc::new(Shared {
            config: Mutex::new(config),
            world: Mutex::new(World::default()),
            mixer: Mutex::new(mixer),
            status: Mutex::new(Status { available: opus.is_some(), error, ..Status::default() }),
            payloads: Mutex::new(VecDeque::new()),
            mic: Mutex::new(VecDeque::new()),
            names: Mutex::new(HashMap::new()),
            groups: Mutex::new(crate::groups::Book::default()),
            asked: Mutex::new(None),
            level: std::sync::atomic::AtomicU32::new((-100.0f32).to_bits()),
            mic_test: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            whisper: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        Self { shared, opus, use_audio, worker: Mutex::new(None), #[cfg(feature = "audio")] audio: Mutex::new(None) }
    }

    pub fn available(&self) -> bool { self.opus.is_some() }

    pub fn config(&self) -> Config { self.shared.config.lock().unwrap().clone() }

    /// Queues the plugin message that asks the server for a secret.
    pub fn queue_join(&self) {
        *self.shared.asked.lock().unwrap() = Some(Instant::now());
        self.shared.payloads.lock().unwrap().push_back(Self::join_payload());
    }

    /// The plugin message to send when the player joins a server.
    pub fn join_payload() -> (String, Vec<u8>) { (secret::CHANNEL_REQUEST_SECRET.to_owned(), secret::request_secret()) }

    /// A plugin message from the server (`voicechat:secret` and `voicechat:states`). `server_host` is the
    /// address the game connected to: where the voice server is unless the secret names another.
    pub fn on_payload(&self, channel: &str, bytes: &[u8], server_host: &str) {
        match channel {
            secret::CHANNEL_SECRET => match { *self.shared.asked.lock().unwrap() = None; SecretPacket::parse(bytes) } {
                Ok(s) => self.connect(s, server_host),
                Err(e) => self.fail(format!("a secret that could not be read: {e:?}")),
            },
            secret::CHANNEL_STATES => {
                if let Ok(states) = secret::parse_states(bytes) {
                    let mut names = self.shared.names.lock().unwrap();
                    let mut book = self.shared.groups.lock().unwrap();
                    for s in states {
                        if s.disconnected { names.remove(&s.uuid); book.members.remove(&s.uuid); }
                        else { book.members.insert(s.uuid, (s.group, s.disabled)); names.insert(s.uuid, s.name); }
                    }
                }
            }
            c if c.starts_with("voicechat:") => {
                if c == crate::groups::CHANNEL_STATE {
                    if let Ok(s) = crate::groups::parse_state(bytes) {
                        let mut names = self.shared.names.lock().unwrap();
                        if s.disconnected { names.remove(&s.uuid); } else { names.insert(s.uuid, s.name); }
                    }
                }
                self.shared.groups.lock().unwrap().on_payload(c, bytes);
            }
            _ => {}
        }
    }

    fn fail(&self, message: String) { self.shared.status.lock().unwrap().error = Some(message); }

    fn connect(&self, secret: SecretPacket, server_host: &str) {
        if self.opus.is_none() || !self.shared.config.lock().unwrap().enabled { return; }
        self.stop_worker();
        let (host, port) = secret::voice_address(&secret.voice_host, server_host, secret.server_port);
        let addr = match (host.as_str(), port).to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(a) => a,
            None => { self.fail(format!("cannot find the voice server {host}:{port}")); return; }
        };
        let socket = match UdpSocket::bind(if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }).and_then(|s| { s.connect(addr)?; Ok(s) }) {
            Ok(s) => s,
            Err(e) => { self.fail(format!("cannot open the voice socket: {e}")); return; }
        };
        let _ = socket.set_read_timeout(Some(Duration::from_millis(5)));
        {
            let mut st = self.shared.status.lock().unwrap();
            st.error = None;
            st.connected = false;
        }
        // The server wants the client's own state once it has sent the secret.
        self.push_state();
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, opus, flag) = (self.shared.clone(), self.opus.clone().unwrap(), stop.clone());
        let handle = std::thread::Builder::new().name("voicechat-net".into())
            .spawn(move || run_network(shared, opus, secret, socket, flag)).expect("voice thread");
        *self.worker.lock().unwrap() = Some((stop, handle));
        self.ensure_audio();
    }

    /// Opens the microphone and speakers, once.
    fn ensure_audio(&self) {
        #[cfg(feature = "audio")]
        if self.use_audio {
            let mut a = self.audio.lock().unwrap();
            if a.is_none() {
                let c = self.shared.config.lock().unwrap().clone();
                *a = Some(crate::audio::start(self.shared.clone(), &c.input_device, &c.output_device));
            }
        }
    }

    /// Closes the microphone and speakers.
    fn release_audio(&self) {
        #[cfg(feature = "audio")]
        if let Some(a) = self.audio.lock().unwrap().take() { a.stop(); }
        self.shared.mic.lock().unwrap().clear();
    }

    fn stop_worker(&self) {
        if let Some((stop, handle)) = self.worker.lock().unwrap().take() {
            stop.store(true, Relaxed);
            let _ = handle.join();
        }
    }

    fn push_state(&self) {
        let (m, d) = (self.shared.muted.load(Relaxed), self.shared.deafened.load(Relaxed));
        self.shared.payloads.lock().unwrap().push_back((secret::CHANNEL_CLIENT_STATE.to_owned(), secret::client_state(m, d)));
    }

    /// The plugin messages waiting to go to the server.
    pub fn take_payloads(&self) -> Vec<(String, Vec<u8>)> { self.shared.payloads.lock().unwrap().drain(..).collect() }

    /// The ears and the players in sight, once a tick.
    pub fn set_world(&self, listener: [f64; 3], yaw: f32, players: HashMap<u128, [f64; 3]>) {
        *self.shared.world.lock().unwrap() = World { listener, yaw, players };
    }

    pub fn set_ptt(&self, down: bool) { self.shared.ptt.store(down, Relaxed); }
    pub fn set_whisper(&self, on: bool) { self.shared.whisper.store(on, Relaxed); }

    pub fn toggle_mute(&self) { self.shared.muted.fetch_xor(true, Relaxed); self.push_state(); }

    /// Deafened also mutes, as in the mod.
    pub fn toggle_deafen(&self) {
        let now = !self.shared.deafened.load(Relaxed);
        self.shared.deafened.store(now, Relaxed);
        if now { self.shared.muted.store(true, Relaxed); }
        if let Some(m) = self.shared.mixer.lock().unwrap().as_mut() { m.deafened = now; }
        self.push_state();
    }

    /// Mute or unmute the microphone; the server is told when it changes.
    pub fn set_muted(&self, muted: bool) {
        if self.shared.muted.swap(muted, Relaxed) != muted { self.push_state(); }
    }

    /// Deafen (no voices are played) or not; the server is told when it changes.
    pub fn set_deafened(&self, deafened: bool) {
        if self.shared.deafened.swap(deafened, Relaxed) != deafened {
            if let Some(m) = self.shared.mixer.lock().unwrap().as_mut() { m.deafened = deafened; }
            self.push_state();
        }
    }

    /// The microphone level in dB (0 is full scale) of the last frame heard.
    pub fn level_db(&self) -> f32 { f32::from_bits(self.shared.level.load(Relaxed)) }

    /// Hear your own microphone (the settings' test), without needing a connection.
    pub fn set_mic_test(&self, on: bool) {
        if self.shared.mic_test.swap(on, Relaxed) == on { return; }
        if on { self.ensure_audio(); }
        else if !self.shared.status.lock().unwrap().connected { self.release_audio(); }
    }

    /// Called often by whoever runs the engine: while the microphone test is on and there is no
    /// connection to send it down, plays the microphone back through the mixer.
    pub fn pump(&self) {
        if !self.shared.mic_test.load(Relaxed) { return; }
        let connected = self.shared.status.lock().unwrap().connected;
        loop {
            let frame: Vec<i16> = {
                let mut q = self.shared.mic.lock().unwrap();
                if q.len() < FRAME { break; }
                q.drain(..FRAME).collect()
            };
            self.shared.level.store(level_db(&frame).to_bits(), Relaxed);
            if !connected { if let Some(m) = self.shared.mixer.lock().unwrap().as_mut() { m.push_local(frame); } }
        }
    }

    /// Opens the microphone and speakers again, after a device was chosen.
    pub fn restart_audio(&self) {
        let wanted = self.shared.status.lock().unwrap().connected || self.shared.mic_test.load(Relaxed);
        self.release_audio();
        if wanted { self.ensure_audio(); }
    }

    /// Per-player volume factors by uuid (the menu's "Adjust Volumes"), 1.0 as they come.
    pub fn set_player_volumes(&self, volumes: HashMap<u128, f32>) {
        if let Some(m) = self.shared.mixer.lock().unwrap().as_mut() { m.player_volumes = volumes; }
    }

    pub fn set_config(&self, config: Config) {
        if let Some(m) = self.shared.mixer.lock().unwrap().as_mut() { m.volume = config.volume; m.pan_strength = config.audio_type.pan_strength(); }
        *self.shared.config.lock().unwrap() = config;
    }

    pub fn status(&self) -> Status {
        let mut s = self.shared.status.lock().unwrap().clone();
        s.muted = self.shared.muted.load(Relaxed);
        s.deafened = self.shared.deafened.load(Relaxed);
        if s.error.is_none() && self.shared.asked.lock().unwrap().is_some_and(|t| t.elapsed() > Duration::from_secs(8)) {
            s.error = Some("the server did not answer: it has no Simple Voice Chat plugin, or a different version (this client speaks compatibility 20)".to_owned());
        }
        if let Some(m) = self.shared.mixer.lock().unwrap().as_ref() { s.speakers = m.talking(Instant::now(), Duration::from_millis(400)); }
        s
    }

    /// The groups, who is in which, and the one this player is in.
    pub fn groups(&self) -> crate::groups::Book { self.shared.groups.lock().unwrap().clone() }

    pub fn create_group(&self, name: &str, password: &str, kind: crate::groups::Kind) {
        self.shared.groups.lock().unwrap().wrong_password = false;
        self.shared.payloads.lock().unwrap().push_back((crate::groups::CHANNEL_CREATE.to_owned(), crate::groups::create(name, password, kind)));
    }

    pub fn join_group(&self, id: u128, password: &str) {
        self.shared.groups.lock().unwrap().wrong_password = false;
        self.shared.payloads.lock().unwrap().push_back((crate::groups::CHANNEL_JOIN.to_owned(), crate::groups::join(id, password)));
    }

    pub fn leave_group(&self) {
        self.shared.payloads.lock().unwrap().push_back((crate::groups::CHANNEL_LEAVE.to_owned(), Vec::new()));
    }

    pub fn name_of(&self, uuid: u128) -> Option<String> { self.shared.names.lock().unwrap().get(&uuid).cloned() }

    /// Closes the connection, the microphone and the speakers.
    pub fn shutdown(&self) {
        self.stop_worker();
        self.release_audio();
        let mut st = self.shared.status.lock().unwrap();
        st.connected = false;
        st.talking = false;
        self.shared.names.lock().unwrap().clear();
        *self.shared.groups.lock().unwrap() = crate::groups::Book::default();
        *self.shared.asked.lock().unwrap() = None;
    }

    /// Feeds microphone samples (mono, 48 kHz): what the capture does, and what a test does.
    pub fn feed_mic(&self, samples: &[i16]) {
        let mut q = self.shared.mic.lock().unwrap();
        q.extend(samples.iter().copied());
        while q.len() > 48_000 * 2 { q.pop_front(); }
    }

    /// Mixes `out` (interleaved stereo, 48 kHz): what the playback does, and what a test does.
    pub fn render(&self, out: &mut [f32]) {
        let world = self.shared.world.lock().unwrap().clone();
        match self.shared.mixer.lock().unwrap().as_mut() { Some(m) => m.mix(out, &world), None => out.fill(0.0) }
    }
}

impl Drop for Engine {
    fn drop(&mut self) { self.shutdown(); }
}

fn level_db(frame: &[i16]) -> f32 {
    let mean = frame.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / frame.len().max(1) as f64;
    (10.0 * (mean / (32768.0 * 32768.0)).max(1e-12).log10()) as f32
}

/// How long voice activation stays open after the voice drops under the threshold.
const HANGOVER: Duration = Duration::from_millis(500);

fn run_network(shared: Arc<Shared>, opus: Arc<Opus>, secret: SecretPacket, socket: UdpSocket, stop: Arc<AtomicBool>) {
    let app = match secret.codec { Codec::Voip => Application::Voip, Codec::Audio => Application::Audio, Codec::RestrictedLowDelay => Application::LowDelay };
    let mut encoder = match Encoder::new(&opus, app) {
        Ok(e) => e,
        Err(e) => { shared.status.lock().unwrap().error = Some(e); return; }
    };
    let mut conn = Connection::new(&secret, Instant::now());
    let started = Instant::now();
    let peer = socket.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut buf = vec![0u8; 4096];
    let (mut was_talking, mut open_until) = (false, Instant::now());
    while !stop.load(Relaxed) && !shared.stop.load(Relaxed) {
        let now = Instant::now();
        for d in conn.poll(now) { let _ = socket.send(&d); }
        if let Ok(n) = socket.recv(&mut buf) {
            if let Ok((replies, events)) = conn.on_datagram(&buf[..n], now) {
                for r in replies { let _ = socket.send(&r); }
                for e in events {
                    match e {
                        Event::Connected => { let mut st = shared.status.lock().unwrap(); st.connected = true; st.error = None; }
                        Event::Sound(s) => { if let Some(m) = shared.mixer.lock().unwrap().as_mut() { let _ = m.push(&s, now); } }
                        Event::TimedOut => {}
                    }
                }
            }
        }
        if !conn.connected() && started.elapsed() > Duration::from_secs(10) {
            let mut st = shared.status.lock().unwrap();
            if st.error.is_none() { st.error = Some(format!("the voice server at {peer} (UDP) is not answering: its voice port may be blocked or not open")); }
        }
        if let Some(Event::TimedOut) = conn.check_timeout(now) {
            let mut st = shared.status.lock().unwrap();
            st.connected = false;
            st.error = Some("the voice server stopped answering".into());
            drop(st);
            // The server issues a fresh secret to a client it dropped; ask for it.
            shared.payloads.lock().unwrap().push_back(crate::engine::Engine::join_payload());
        }
        // The microphone, a frame at a time.
        loop {
            let frame: Vec<i16> = {
                let mut q = shared.mic.lock().unwrap();
                if q.len() < FRAME { break; }
                q.drain(..FRAME).collect()
            };
            shared.level.store(level_db(&frame).to_bits(), Relaxed);
            if !conn.connected() { continue; }
            let cfg = shared.config.lock().unwrap().clone();
            let blocked = shared.muted.load(Relaxed) || shared.deafened.load(Relaxed);
            let open = if cfg.push_to_talk {
                shared.ptt.load(Relaxed)
            } else {
                if level_db(&frame) > cfg.activation_db { open_until = now + HANGOVER; }
                now < open_until
            };
            let talking = open && !blocked;
            if talking {
                if let Ok(opus) = encoder.encode(&frame) {
                    if let Some(d) = conn.mic_datagram(opus, shared.whisper.load(Relaxed)) { let _ = socket.send(&d); }
                }
            } else if was_talking {
                if let Some(d) = conn.stop_datagram() { let _ = socket.send(&d); }
            }
            was_talking = talking;
            shared.status.lock().unwrap().talking = talking;
        }
    }
    if was_talking { if let Some(d) = conn.stop_datagram() { let _ = socket.send(&d); } }
    // A final goodbye is not part of the protocol; the server drops us after the keep-alive timeout.
    let _ = Packet::KeepAlive;
}
