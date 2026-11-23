//! The plugin messages Simple Voice Chat sends over Minecraft's own connection: the request the client makes,
//! the secret the server answers with (everything needed to open the UDP side), and the client's state.
//! Layouts and the compatibility version are from the mod's 1.21.11 branch (`net/*Packet.java`).
use crate::wire::{Reader, WireError, Writer};

/// `BuildConstants.COMPATIBILITY_VERSION` of 2.6.21 (1.21.11), which logs "Compatibility version 20". A server that expects another version
/// answers with a chat message instead of a secret.
pub const COMPATIBILITY_VERSION: i32 = 20;

pub const CHANNEL_REQUEST_SECRET: &str = "voicechat:request_secret";
pub const CHANNEL_SECRET: &str = "voicechat:secret";
pub const CHANNEL_CLIENT_STATE: &str = "voicechat:client_state";
pub const CHANNEL_STATES: &str = "voicechat:states";

/// Which Opus mode the server asks for (`ServerConfig.Codec` ordinal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Voip,
    Audio,
    RestrictedLowDelay,
}

impl Codec {
    fn from_byte(b: u8) -> Self {
        match b {
            1 => Self::Audio,
            2 => Self::RestrictedLowDelay,
            _ => Self::Voip,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SecretPacket {
    /// The AES-128 key of every datagram.
    pub secret: [u8; 16],
    /// Names this connection in the datagrams the client sends.
    pub connection_id: u128,
    pub server_port: i32,
    pub player_uuid: u128,
    pub codec: Codec,
    pub mtu_size: i32,
    pub voice_chat_distance: f64,
    /// Milliseconds between the server's keep-alives.
    pub keep_alive_ms: i32,
    pub groups_enabled: bool,
    pub voice_host: String,
    pub allow_recording: bool,
}

impl SecretPacket {
    pub fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let secret = r.array16()?;
        let server_port = r.i32()?;
        let player_uuid = r.uuid()?;
        Ok(Self {
            secret,
            // Datagrams name the sender by the player's uuid (`NetworkMessage.writeClient`).
            connection_id: player_uuid,
            server_port,
            player_uuid,
            codec: Codec::from_byte(r.u8()?),
            // The client clamps it to what a datagram can carry.
            mtu_size: r.i32()?.clamp(512, 2048),
            voice_chat_distance: r.f64()?,
            keep_alive_ms: r.i32()?,
            groups_enabled: r.bool()?,
            voice_host: r.string(32767)?,
            allow_recording: r.bool()?,
        })
    }

    #[cfg(test)]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.raw(&self.secret).i32(self.server_port).uuid(self.player_uuid);
        w.u8(match self.codec { Codec::Voip => 0, Codec::Audio => 1, Codec::RestrictedLowDelay => 2 });
        w.i32(self.mtu_size).f64(self.voice_chat_distance).i32(self.keep_alive_ms).bool(self.groups_enabled);
        w.string(&self.voice_host).bool(self.allow_recording);
        w.finish()
    }
}

/// What the client sends when it joins: its compatibility version.
pub fn request_secret() -> Vec<u8> {
    let mut w = Writer::new();
    w.i32(COMPATIBILITY_VERSION);
    w.finish()
}

/// The client's own state, sent after the secret arrives and whenever it changes.
pub fn client_state(muted: bool, disabled: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.bool(muted).bool(disabled);
    w.finish()
}

/// One player in the server's state list.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerState {
    pub uuid: u128,
    pub name: String,
    pub muted: bool,
    pub disabled: bool,
    pub disconnected: bool,
    pub group: Option<u128>,
}

/// `PlayerStatesPacket`: a count, then each player's flags, uuid, name and group.
pub fn parse_states(bytes: &[u8]) -> Result<Vec<PlayerState>, WireError> {
    let mut r = Reader::new(bytes);
    let n = r.i32()?.clamp(0, 4096);
    let mut out = Vec::new();
    for _ in 0..n {
        let flags = r.u8()?;
        let uuid = r.uuid()?;
        let name = r.string(256)?;
        let group = if flags & 0x20 != 0 { Some(r.uuid()?) } else { None };
        out.push(PlayerState { uuid, name, muted: flags & 1 != 0, disabled: flags & 4 != 0, disconnected: flags & 0x10 != 0, group });
    }
    Ok(out)
}

/// Where the UDP side lives: the server's own address unless `voice_host` says otherwise. It is empty
/// (use the server's address and `server_port`), a bare port, or `host`, `host:port`.
pub fn voice_address(voice_host: &str, server_host: &str, server_port: i32) -> (String, u16) {
    let port = server_port.clamp(1, 65535) as u16;
    let h = voice_host.trim();
    if h.is_empty() { return (server_host.to_owned(), port); }
    if let Ok(p) = h.parse::<i32>() {
        return if (1..=65535).contains(&p) { (server_host.to_owned(), p as u16) } else { (server_host.to_owned(), port) };
    }
    // `voicechat://host:port`, the way the mod reads it.
    let h = h.strip_prefix("voicechat://").unwrap_or(h);
    match h.rsplit_once(':') {
        Some((host, p)) if !host.contains(']') || host.ends_with(']') => match p.parse::<u16>() {
            Ok(p) if p > 0 => (host.trim_matches(['[', ']']).to_owned(), p),
            _ => (host.trim_matches(['[', ']']).to_owned(), port),
        },
        _ => (h.to_owned(), port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SecretPacket {
        SecretPacket {
            secret: *b"0123456789abcdef",
            connection_id: 0x1,
            server_port: 24454,
            player_uuid: 0x1,
            codec: Codec::Audio,
            mtu_size: 1275,
            voice_chat_distance: 48.0,
            keep_alive_ms: 1000,
            groups_enabled: true,
            voice_host: String::new(),
            allow_recording: true,
        }
    }

    #[test]
    fn the_secret_with_no_voice_host_is_56_bytes_as_the_mod_writes_it() {
        let bytes = sample().encode();
        assert_eq!(bytes.len(), 56);
        assert_eq!(&bytes[..16], b"0123456789abcdef");
        assert_eq!(SecretPacket::parse(&bytes).unwrap(), sample());
    }

    #[test]
    fn a_voice_host_and_the_mtu_clamp() {
        let mut s = sample();
        s.voice_host = "voice.example.com:30000".into();
        s.mtu_size = 99999;
        let back = SecretPacket::parse(&s.encode()).unwrap();
        assert_eq!(back.voice_host, "voice.example.com:30000");
        assert_eq!(back.mtu_size, 2048);
        let mut s = sample();
        s.mtu_size = 1;
        assert_eq!(SecretPacket::parse(&s.encode()).unwrap().mtu_size, 512);
    }

    #[test]
    fn a_short_secret_is_refused() {
        assert!(SecretPacket::parse(&sample().encode()[..40]).is_err());
    }

    #[test]
    fn the_requests_have_the_layouts_the_server_reads() {
        assert_eq!(request_secret(), vec![0, 0, 0, 20]);
        assert_eq!(client_state(true, false), vec![1, 0]);
    }

    #[test]
    fn player_states_parse_flags_names_and_groups() {
        let mut w = Writer::new();
        w.i32(2);
        w.u8(0b0000_0001).uuid(7).string("Steve");
        w.u8(0b0010_0100).uuid(8).string("Alex").uuid(99);
        let states = parse_states(&w.0).unwrap();
        assert_eq!(states[0], PlayerState { uuid: 7, name: "Steve".into(), muted: true, disabled: false, disconnected: false, group: None });
        assert_eq!(states[1].group, Some(99));
        assert!(states[1].disabled && !states[1].muted);
    }

    #[test]
    fn the_voice_address_follows_the_mods_rules() {
        assert_eq!(voice_address("", "mc.example.com", 24454), ("mc.example.com".into(), 24454));
        assert_eq!(voice_address("25000", "mc.example.com", 24454), ("mc.example.com".into(), 25000));
        assert_eq!(voice_address("99999", "mc.example.com", 24454), ("mc.example.com".into(), 24454));
        assert_eq!(voice_address("voice.example.com:30000", "mc.example.com", 24454), ("voice.example.com".into(), 30000));
        assert_eq!(voice_address("voice.example.com", "mc.example.com", 24454), ("voice.example.com".into(), 24454));
    }
}
