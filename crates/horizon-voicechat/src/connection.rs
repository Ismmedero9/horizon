//! The client's side of the UDP handshake and keep-alive, as a state machine with no sockets and no clock
//! of its own (the caller gives the time), so it is tested against a fake server.
//!
//! The order, from the mod: send `Authenticate` (player and secret) every second until the server's
//! `AuthenticateAck`; send `ConnectionCheck` every second until `ConnectionCheckAck`; then the
//! connection is up. The server sends `KeepAlive` and the client answers each. A `Ping` is echoed.
use crate::net::{client_datagram, parse_server_datagram, Crypto, NetError, Packet, Sound};
use crate::secret::SecretPacket;
use std::time::{Duration, Instant};

const RESEND: Duration = Duration::from_millis(1000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Authenticating,
    Checking,
    Connected,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Connected,
    Sound(Sound),
    /// The server stopped answering for ten keep-alive periods.
    TimedOut,
}

pub struct Connection {
    crypto: Crypto,
    connection_id: u128,
    keep_alive: Duration,
    phase: Phase,
    secret: [u8; 16],
    player: u128,
    last_send: Option<Instant>,
    last_keep_alive: Instant,
    sequence: i64,
    timed_out: bool,
}

impl Connection {
    pub fn new(secret: &SecretPacket, now: Instant) -> Self {
        Self {
            crypto: Crypto::new(&secret.secret),
            connection_id: secret.connection_id,
            keep_alive: Duration::from_millis(secret.keep_alive_ms.max(1000) as u64),
            phase: Phase::Authenticating,
            secret: secret.secret,
            player: secret.player_uuid,
            last_send: None,
            last_keep_alive: now,
            sequence: 0,
            timed_out: false,
        }
    }

    pub fn phase(&self) -> Phase { self.phase }
    pub fn connected(&self) -> bool { self.phase == Phase::Connected && !self.timed_out }

    pub fn datagram(&self, packet: &Packet) -> Vec<u8> { client_datagram(&self.crypto, self.connection_id, packet) }

    /// What to send now: the handshake step again once a second, until it is answered.
    pub fn poll(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let due = self.last_send.map_or(true, |t| now.duration_since(t) >= RESEND);
        let packet = match self.phase {
            Phase::Authenticating => Packet::Authenticate { player: self.player, secret: self.secret },
            Phase::Checking => Packet::ConnectionCheck,
            _ => return Vec::new(),
        };
        if !due { return Vec::new(); }
        self.last_send = Some(now);
        vec![self.datagram(&packet)]
    }

    /// Takes a datagram from the server: the datagrams to send back and what happened.
    pub fn on_datagram(&mut self, data: &[u8], now: Instant) -> Result<(Vec<Vec<u8>>, Vec<Event>), NetError> {
        let packet = parse_server_datagram(&self.crypto, data)?;
        let (mut replies, mut events) = (Vec::new(), Vec::new());
        match packet {
            Packet::AuthenticateAck if self.phase == Phase::Authenticating => {
                self.phase = Phase::Checking;
                self.last_send = None; // send the check at once
            }
            Packet::ConnectionCheckAck if self.phase == Phase::Checking => {
                self.phase = Phase::Connected;
                self.last_keep_alive = now;
                events.push(Event::Connected);
            }
            Packet::KeepAlive => {
                self.last_keep_alive = now;
                replies.push(self.datagram(&Packet::KeepAlive));
            }
            Packet::Ping { id, timestamp } => replies.push(self.datagram(&Packet::Ping { id, timestamp })),
            Packet::Sound(s) if self.phase == Phase::Connected => events.push(Event::Sound(s)),
            _ => {}
        }
        Ok((replies, events))
    }

    /// Ten keep-alive periods of silence: the server has dropped us (and will send a new secret).
    pub fn check_timeout(&mut self, now: Instant) -> Option<Event> {
        if self.phase == Phase::Connected && !self.timed_out && now.duration_since(self.last_keep_alive) > self.keep_alive * 10 {
            self.timed_out = true;
            return Some(Event::TimedOut);
        }
        None
    }

    /// A microphone packet, once the connection is up. Numbers its packets from zero.
    pub fn mic_datagram(&mut self, opus: Vec<u8>, whispering: bool) -> Option<Vec<u8>> {
        if !self.connected() { return None; }
        let sequence = self.sequence;
        self.sequence += 1;
        Some(self.datagram(&Packet::Mic { opus, sequence, whispering }))
    }

    /// The packet that tells the others you stopped talking: an empty frame.
    pub fn stop_datagram(&mut self) -> Option<Vec<u8>> { self.mic_datagram(Vec::new(), false) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{parse_client_datagram, server_datagram};
    use crate::secret::Codec;

    fn secret() -> SecretPacket {
        SecretPacket { secret: *b"0123456789abcdef", connection_id: 0xC0DE, server_port: 24454, player_uuid: 0xC0DE, codec: Codec::Voip,
            mtu_size: 1275, voice_chat_distance: 48.0, keep_alive_ms: 1000, groups_enabled: true, voice_host: String::new(), allow_recording: true }
    }

    /// The server's half, as the mod's `Server` does it.
    struct FakeServer { crypto: Crypto }
    impl FakeServer {
        fn new() -> Self { Self { crypto: Crypto::new(&secret().secret) } }
        fn read(&self, d: &[u8]) -> (u128, Packet) { parse_client_datagram(|_| Some(Crypto::new(&secret().secret)), d).unwrap() }
        fn send(&self, p: &Packet) -> Vec<u8> { server_datagram(&self.crypto, p) }
    }

    #[test]
    fn the_handshake_runs_authenticate_then_check_then_connected() {
        let (server, t0) = (FakeServer::new(), Instant::now());
        let mut c = Connection::new(&secret(), t0);
        // First poll sends Authenticate, naming the connection.
        let out = c.poll(t0);
        assert_eq!(out.len(), 1);
        assert_eq!(server.read(&out[0]), (0xC0DE, Packet::Authenticate { player: 0xC0DE, secret: *b"0123456789abcdef" }));
        // Nothing again until a second has passed, then it is repeated.
        assert!(c.poll(t0 + Duration::from_millis(500)).is_empty());
        let again = c.poll(t0 + Duration::from_millis(1000));
        assert!(matches!(server.read(&again[0]).1, Packet::Authenticate { .. }));
        // The ack; the check follows at once.
        let (replies, events) = c.on_datagram(&server.send(&Packet::AuthenticateAck), t0).unwrap();
        assert!(replies.is_empty() && events.is_empty());
        assert_eq!(c.phase(), Phase::Checking);
        let check = c.poll(t0 + Duration::from_millis(1001));
        assert_eq!(server.read(&check[0]).1, Packet::ConnectionCheck);
        assert!(!c.connected() && c.mic_datagram(vec![1], false).is_none(), "no audio before the connection is up");
        let (_, events) = c.on_datagram(&server.send(&Packet::ConnectionCheckAck), t0).unwrap();
        assert_eq!(events, vec![Event::Connected]);
        assert!(c.connected() && c.poll(t0 + Duration::from_secs(5)).is_empty());
    }

    fn up(c: &mut Connection, server: &FakeServer, t: Instant) {
        c.poll(t);
        c.on_datagram(&server.send(&Packet::AuthenticateAck), t).unwrap();
        c.on_datagram(&server.send(&Packet::ConnectionCheckAck), t).unwrap();
    }

    #[test]
    fn keep_alives_and_pings_are_answered() {
        let (server, t0) = (FakeServer::new(), Instant::now());
        let mut c = Connection::new(&secret(), t0);
        up(&mut c, &server, t0);
        let (replies, _) = c.on_datagram(&server.send(&Packet::KeepAlive), t0).unwrap();
        assert_eq!(server.read(&replies[0]).1, Packet::KeepAlive);
        let ping = Packet::Ping { id: 5, timestamp: 99 };
        let (replies, _) = c.on_datagram(&server.send(&ping), t0).unwrap();
        assert_eq!(server.read(&replies[0]).1, ping);
    }

    #[test]
    fn mic_packets_are_numbered_and_stopping_sends_an_empty_frame() {
        let (server, t0) = (FakeServer::new(), Instant::now());
        let mut c = Connection::new(&secret(), t0);
        up(&mut c, &server, t0);
        for want in 0..3 {
            let d = c.mic_datagram(vec![7, 7], false).unwrap();
            assert_eq!(server.read(&d).1, Packet::Mic { opus: vec![7, 7], sequence: want, whispering: false });
        }
        let stop = c.stop_datagram().unwrap();
        assert_eq!(server.read(&stop).1, Packet::Mic { opus: vec![], sequence: 3, whispering: false });
    }

    #[test]
    fn sounds_come_out_as_events_only_once_connected() {
        let (server, t0) = (FakeServer::new(), Instant::now());
        let mut c = Connection::new(&secret(), t0);
        let s = Sound { channel: 2, sender: 2, opus: vec![1], sequence: 0, distance: 48.0, whispering: false, category: None, location: None, group: false };
        let (_, before) = c.on_datagram(&server.send(&Packet::Sound(s.clone())), t0).unwrap();
        assert!(before.is_empty());
        up(&mut c, &server, t0);
        let (_, after) = c.on_datagram(&server.send(&Packet::Sound(s.clone())), t0).unwrap();
        assert_eq!(after, vec![Event::Sound(s)]);
    }

    #[test]
    fn silence_from_the_server_for_ten_periods_times_out() {
        let (server, t0) = (FakeServer::new(), Instant::now());
        let mut c = Connection::new(&secret(), t0);
        up(&mut c, &server, t0);
        assert_eq!(c.check_timeout(t0 + Duration::from_secs(9)), None);
        assert_eq!(c.check_timeout(t0 + Duration::from_secs(11)), Some(Event::TimedOut));
        assert!(!c.connected());
        assert_eq!(c.check_timeout(t0 + Duration::from_secs(12)), None, "reported once");
    }

    #[test]
    fn a_datagram_under_another_key_is_refused() {
        let t0 = Instant::now();
        let mut c = Connection::new(&secret(), t0);
        let other = Crypto::new(b"another-key-1234");
        assert!(c.on_datagram(&server_datagram(&other, &Packet::KeepAlive), t0).is_err());
    }
}
