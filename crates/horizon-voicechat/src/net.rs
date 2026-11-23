//! The UDP side of Simple Voice Chat: how a datagram is framed and encrypted, and the packets in it.
//! From the mod's 1.21.11 branch (`voice/common/NetworkMessage.java`, `Secret.java`, `*Packet.java`).
//!
//! Client to server: `0xFF`, the connection id (16 bytes), a VarInt length, then the encrypted payload.
//! Server to client: `0xFF`, a VarInt length, then the encrypted payload (no connection id).
//! The payload is a 12-byte IV, then AES-128-GCM ciphertext and tag, of a type byte and the packet body.
use crate::wire::{Reader, WireError, Writer};
use aes_gcm::{aead::{Aead, KeyInit}, Aes128Gcm, Nonce};

pub const MAGIC: u8 = 0xFF;
pub const MAX_PAYLOAD: usize = 2048;
/// The largest Opus frame the mod sends.
pub const MAX_OPUS: usize = 1275;
const IV_LEN: usize = 12;

#[derive(Debug, PartialEq)]
pub enum NetError {
    Wire(WireError),
    NotVoice,
    Decrypt,
    UnknownType(u8),
}

impl From<WireError> for NetError {
    fn from(e: WireError) -> Self { NetError::Wire(e) }
}

pub struct Crypto(Aes128Gcm);

impl Crypto {
    pub fn new(secret: &[u8; 16]) -> Self { Self(Aes128Gcm::new(secret.into())) }

    /// IV, then ciphertext and tag.
    pub fn encrypt(&self, plain: &[u8]) -> Vec<u8> {
        let mut iv = [0u8; IV_LEN];
        getrandom::fill(&mut iv).expect("random bytes");
        self.encrypt_with_iv(&iv, plain)
    }

    pub fn encrypt_with_iv(&self, iv: &[u8; IV_LEN], plain: &[u8]) -> Vec<u8> {
        let ct = self.0.encrypt(Nonce::from_slice(iv), plain).expect("encrypting in memory");
        let mut out = Vec::with_capacity(IV_LEN + ct.len());
        out.extend_from_slice(iv);
        out.extend_from_slice(&ct);
        out
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, NetError> {
        if data.len() < IV_LEN + 16 { return Err(NetError::Decrypt); }
        let (iv, ct) = data.split_at(IV_LEN);
        self.0.decrypt(Nonce::from_slice(iv), ct).map_err(|_| NetError::Decrypt)
    }
}

/// The sound a speaker's packet carries, after the type byte.
#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    /// The channel the audio belongs to: the sender's own for proximity, the group's for a group.
    pub channel: u128,
    pub sender: u128,
    pub opus: Vec<u8>,
    pub sequence: i64,
    /// Proximity and location sounds: how far it carries, in blocks.
    pub distance: f32,
    pub whispering: bool,
    pub category: Option<String>,
    /// Location sounds only.
    pub location: Option<[f64; 3]>,
    pub group: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Packet {
    Mic { opus: Vec<u8>, sequence: i64, whispering: bool },
    Sound(Sound),
    /// The player and the secret, so the server can tell which connection this is (`AuthenticatePacket`).
    Authenticate { player: u128, secret: [u8; 16] },
    AuthenticateAck,
    Ping { id: u128, timestamp: i64 },
    KeepAlive,
    ConnectionCheck,
    ConnectionCheckAck,
}

impl Packet {
    pub fn type_byte(&self) -> u8 {
        match self {
            Packet::Mic { .. } => 0x01,
            Packet::Sound(s) if s.group => 0x03,
            Packet::Sound(s) if s.location.is_some() => 0x04,
            Packet::Sound(_) => 0x02,
            Packet::Authenticate { .. } => 0x05,
            Packet::AuthenticateAck => 0x06,
            Packet::Ping { .. } => 0x07,
            Packet::KeepAlive => 0x08,
            Packet::ConnectionCheck => 0x09,
            Packet::ConnectionCheckAck => 0x0A,
        }
    }

    /// The type byte and the body: what gets encrypted.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(self.type_byte());
        match self {
            Packet::Mic { opus, sequence, whispering } => { w.bytes(opus).i64(*sequence).bool(*whispering); }
            Packet::Sound(s) => {
                w.uuid(s.channel).uuid(s.sender);
                if let Some(l) = s.location { w.f64(l[0]).f64(l[1]).f64(l[2]); }
                w.bytes(&s.opus).i64(s.sequence);
                if !s.group { w.f32(s.distance); }
                let flags = (s.whispering as u8) | ((s.category.is_some() as u8) << 1);
                w.u8(flags);
                if let Some(c) = &s.category { w.string(c); }
            }
            Packet::Authenticate { player, secret } => { w.uuid(*player).raw(secret); }
            Packet::AuthenticateAck | Packet::KeepAlive | Packet::ConnectionCheck | Packet::ConnectionCheckAck => {}
            Packet::Ping { id, timestamp } => { w.uuid(*id).i64(*timestamp); }
        }
        w.finish()
    }

    pub fn decode(plain: &[u8]) -> Result<Self, NetError> {
        let mut r = Reader::new(plain);
        let ty = r.u8()?;
        Ok(match ty {
            0x01 => Packet::Mic { opus: r.bytes(MAX_OPUS)?.to_vec(), sequence: r.i64()?, whispering: r.bool()? },
            0x02 | 0x03 | 0x04 => {
                let channel = r.uuid()?;
                let sender = r.uuid()?;
                let location = if ty == 0x04 { Some([r.f64()?, r.f64()?, r.f64()?]) } else { None };
                let opus = r.bytes(MAX_OPUS)?.to_vec();
                let sequence = r.i64()?;
                let distance = if ty == 0x03 { 0.0 } else { r.f32()? };
                let flags = r.u8()?;
                let category = if flags & 2 != 0 { Some(r.string(16)?) } else { None };
                Packet::Sound(Sound { channel, sender, opus, sequence, distance, whispering: flags & 1 != 0, category, location, group: ty == 0x03 })
            }
            0x05 => Packet::Authenticate { player: r.uuid()?, secret: r.array16()? },
            0x06 => Packet::AuthenticateAck,
            0x07 => Packet::Ping { id: r.uuid()?, timestamp: r.i64()? },
            0x08 => Packet::KeepAlive,
            0x09 => Packet::ConnectionCheck,
            0x0A => Packet::ConnectionCheckAck,
            other => return Err(NetError::UnknownType(other)),
        })
    }
}

/// A datagram the client sends.
pub fn client_datagram(crypto: &Crypto, connection_id: u128, packet: &Packet) -> Vec<u8> {
    let enc = crypto.encrypt(&packet.encode());
    let mut w = Writer::new();
    w.u8(MAGIC).uuid(connection_id).bytes(&enc);
    w.finish()
}

/// A datagram the server sends (what a test server writes).
pub fn server_datagram(crypto: &Crypto, packet: &Packet) -> Vec<u8> {
    let enc = crypto.encrypt(&packet.encode());
    let mut w = Writer::new();
    w.u8(MAGIC).bytes(&enc);
    w.finish()
}

/// Reads a datagram from the server.
pub fn parse_server_datagram(crypto: &Crypto, data: &[u8]) -> Result<Packet, NetError> {
    let mut r = Reader::new(data);
    if r.u8()? != MAGIC { return Err(NetError::NotVoice); }
    let enc = r.bytes(MAX_PAYLOAD)?;
    Packet::decode(&crypto.decrypt(enc)?)
}

/// Reads a datagram from a client (what a test server does): the connection id and the packet.
pub fn parse_client_datagram(crypto_of: impl Fn(u128) -> Option<Crypto>, data: &[u8]) -> Result<(u128, Packet), NetError> {
    let mut r = Reader::new(data);
    if r.u8()? != MAGIC { return Err(NetError::NotVoice); }
    let id = r.uuid()?;
    let enc = r.bytes(MAX_PAYLOAD)?;
    let crypto = crypto_of(id).ok_or(NetError::Decrypt)?;
    Ok((id, Packet::decode(&crypto.decrypt(enc)?)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 16] = *b"super-secret-key";

    #[test]
    fn encryption_round_trips_and_authenticates() {
        let c = Crypto::new(&KEY);
        let enc = c.encrypt(b"hello voice");
        assert_eq!(enc.len(), 12 + 11 + 16); // IV, ciphertext, tag
        assert_eq!(c.decrypt(&enc).unwrap(), b"hello voice");
        // A flipped bit anywhere is refused, as is another key.
        for i in [0, 12, enc.len() - 1] {
            let mut bad = enc.clone();
            bad[i] ^= 1;
            assert_eq!(c.decrypt(&bad), Err(NetError::Decrypt));
        }
        assert_eq!(Crypto::new(b"another-key-1234").decrypt(&enc), Err(NetError::Decrypt));
        assert_eq!(c.decrypt(&enc[..20]), Err(NetError::Decrypt));
    }

    #[test]
    fn aes_gcm_matches_the_published_test_vector() {
        // NIST GCM test case 2 (AES-128, zero key, zero IV, 16 zero bytes): the well-known ciphertext and tag.
        let c = Crypto::new(&[0u8; 16]);
        let out = c.encrypt_with_iv(&[0u8; 12], &[0u8; 16]);
        let expect_ct = [0x03, 0x88, 0xda, 0xce, 0x60, 0xb6, 0xa3, 0x92, 0xf3, 0x28, 0xc2, 0xb9, 0x71, 0xb2, 0xfe, 0x78];
        let expect_tag = [0xab, 0x6e, 0x47, 0xd4, 0x2c, 0xec, 0x13, 0xbd, 0xf5, 0x3a, 0x67, 0xb2, 0x12, 0x57, 0xbd, 0xdf];
        assert_eq!(&out[12..28], &expect_ct);
        assert_eq!(&out[28..], &expect_tag);
    }

    fn sound(group: bool, location: Option<[f64; 3]>, category: Option<&str>) -> Sound {
        Sound { channel: 1, sender: 2, opus: vec![9, 8, 7], sequence: -5, distance: if group { 0.0 } else { 48.0 }, whispering: !group, category: category.map(str::to_owned), location, group }
    }

    #[test]
    fn every_packet_round_trips() {
        let packets = vec![
            Packet::Mic { opus: vec![1, 2, 3], sequence: 7, whispering: true },
            Packet::Mic { opus: vec![], sequence: 8, whispering: false },
            Packet::Sound(sound(false, None, None)),
            Packet::Sound(sound(false, None, Some("radio"))),
            Packet::Sound(sound(true, None, Some("g"))),
            Packet::Sound(sound(false, Some([1.5, -2.0, 3.25]), None)),
            Packet::Authenticate { player: 9, secret: [4; 16] },
            Packet::AuthenticateAck,
            Packet::Ping { id: 77, timestamp: 123456789 },
            Packet::KeepAlive,
            Packet::ConnectionCheck,
            Packet::ConnectionCheckAck,
        ];
        for p in packets {
            assert_eq!(Packet::decode(&p.encode()).unwrap(), p, "{p:?}");
        }
        assert_eq!(Packet::decode(&[0x7F]), Err(NetError::UnknownType(0x7F)));
    }

    #[test]
    fn type_bytes_are_the_mods_ids() {
        assert_eq!(Packet::Mic { opus: vec![], sequence: 0, whispering: false }.type_byte(), 0x01);
        assert_eq!(Packet::Sound(sound(false, None, None)).type_byte(), 0x02);
        assert_eq!(Packet::Sound(sound(true, None, None)).type_byte(), 0x03);
        assert_eq!(Packet::Sound(sound(false, Some([0.0; 3]), None)).type_byte(), 0x04);
        assert_eq!(Packet::Authenticate { player: 0, secret: [0; 16] }.type_byte(), 0x05);
        assert_eq!(Packet::KeepAlive.type_byte(), 0x08);
        assert_eq!(Packet::ConnectionCheckAck.type_byte(), 0x0A);
    }

    #[test]
    fn the_mic_packet_bytes_are_the_documented_layout() {
        // type 1, VarInt length 3, the data, an i64 sequence, the whisper bool.
        let p = Packet::Mic { opus: vec![0xAA, 0xBB, 0xCC], sequence: 2, whispering: true };
        assert_eq!(p.encode(), vec![0x01, 3, 0xAA, 0xBB, 0xCC, 0, 0, 0, 0, 0, 0, 0, 2, 1]);
    }

    #[test]
    fn datagrams_have_the_client_and_server_framing() {
        let c = Crypto::new(&KEY);
        let d = client_datagram(&c, 0xABCD, &Packet::Authenticate { player: 1, secret: [2; 16] });
        assert_eq!(d[0], MAGIC);
        assert_eq!(u128::from_be_bytes(d[1..17].try_into().unwrap()), 0xABCD);
        let (id, p) = parse_client_datagram(|_| Some(Crypto::new(&KEY)), &d).unwrap();
        assert_eq!((id, p), (0xABCD, Packet::Authenticate { player: 1, secret: [2; 16] }));

        let s = server_datagram(&c, &Packet::KeepAlive);
        assert_eq!(s[0], MAGIC);
        assert_eq!(parse_server_datagram(&c, &s).unwrap(), Packet::KeepAlive);
        // No magic byte: not a voice datagram.
        assert_eq!(parse_server_datagram(&c, &s[1..]), Err(NetError::NotVoice));
    }
}
