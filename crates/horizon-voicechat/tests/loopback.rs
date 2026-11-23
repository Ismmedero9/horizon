//! The whole client against a fake Simple Voice Chat server on a real UDP socket: the handshake, a
//! microphone frame going out and being decoded, a voice coming back and being mixed, and the stop packet.
use horizon_voicechat::config::Config;
use horizon_voicechat::engine::Engine;
use horizon_voicechat::net::{parse_client_datagram, server_datagram, Crypto, Packet, Sound};
use horizon_voicechat::opus::{Application, Decoder, Encoder, Opus, FRAME};
use horizon_voicechat::secret::{self, Codec, SecretPacket};
use std::collections::HashMap;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

const SECRET: [u8; 16] = *b"loopback-secret!";
const CONNECTION: u128 = 0xFEED;

fn wait(mut done: impl FnMut() -> bool, what: &str) {
    let t = Instant::now();
    while !done() {
        assert!(t.elapsed() < Duration::from_secs(8), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn tone(frame: usize, hz: f32) -> Vec<i16> {
    (0..FRAME).map(|i| (((frame * FRAME + i) as f32 / 48_000.0 * hz * std::f32::consts::TAU).sin() * 12_000.0) as i16).collect()
}

#[test]
fn a_full_session_against_a_fake_server() {
    let Ok(lib) = Opus::load() else { eprintln!("no libopus here; skipped"); return };
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let port = server.local_addr().unwrap().port();

    let mut cfg = Config::default();
    cfg.push_to_talk = true;
    let engine = Engine::new(cfg, false);
    assert!(engine.available());
    let secret = SecretPacket { secret: SECRET, connection_id: CONNECTION, server_port: port as i32, player_uuid: CONNECTION, codec: Codec::Voip,
        mtu_size: 1275, voice_chat_distance: 48.0, keep_alive_ms: 1000, groups_enabled: true, voice_host: String::new(), allow_recording: true };
    engine.on_payload(secret::CHANNEL_SECRET, &secret_bytes(&secret), "127.0.0.1");

    // The client's own state goes to the server after the secret.
    let sent = engine.take_payloads();
    assert_eq!(sent, vec![(secret::CHANNEL_CLIENT_STATE.to_owned(), secret::client_state(false, false))]);

    // The handshake, as the mod's server does it.
    let crypto = Crypto::new(&SECRET);
    let mut buf = vec![0u8; 4096];
    let mut client_addr = None;
    let (mut authed, mut checked) = (false, false);
    let t = Instant::now();
    while !checked {
        assert!(t.elapsed() < Duration::from_secs(8), "the handshake did not finish");
        let Ok((n, from)) = server.recv_from(&mut buf) else { continue };
        let (id, packet) = parse_client_datagram(|_| Some(Crypto::new(&SECRET)), &buf[..n]).unwrap();
        assert_eq!(id, CONNECTION, "every datagram names the connection");
        client_addr = Some(from);
        match packet {
            Packet::Authenticate { player, secret } => {
                assert_eq!((player, secret), (CONNECTION, SECRET), "it names the player and carries the secret");
                authed = true;
                server.send_to(&server_datagram(&crypto, &Packet::AuthenticateAck), from).unwrap();
            }
            Packet::ConnectionCheck => {
                assert!(authed, "the check follows the ack");
                server.send_to(&server_datagram(&crypto, &Packet::ConnectionCheckAck), from).unwrap();
                checked = true;
            }
            other => panic!("unexpected {other:?} during the handshake"),
        }
    }
    let client_addr = client_addr.unwrap();
    wait(|| engine.status().connected, "the client to report connected");

    // Holding push to talk sends the microphone, numbered from zero, decodable Opus of what was said.
    engine.set_ptt(true);
    for f in 0..6 { engine.feed_mic(&tone(f, 440.0)); }
    let mut dec = Decoder::new(&lib).unwrap();
    let mut seqs = Vec::new();
    let mut last_pcm = Vec::new();
    let t = Instant::now();
    while seqs.len() < 6 {
        assert!(t.elapsed() < Duration::from_secs(8), "got {} of 6 mic packets", seqs.len());
        let Ok((n, _)) = server.recv_from(&mut buf) else { continue };
        match parse_client_datagram(|_| Some(Crypto::new(&SECRET)), &buf[..n]).unwrap().1 {
            Packet::Mic { opus, sequence, whispering } => {
                assert!(!whispering);
                last_pcm = dec.decode(&opus).unwrap();
                seqs.push(sequence);
            }
            Packet::KeepAlive => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(seqs, vec![0, 1, 2, 3, 4, 5]);
    let energy = last_pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / FRAME as f64;
    assert!(energy > 1.0e6, "the tone arrived: energy {energy}");

    // Letting go sends the stop packet.
    engine.set_ptt(false);
    engine.feed_mic(&tone(6, 440.0));
    let t = Instant::now();
    loop {
        assert!(t.elapsed() < Duration::from_secs(8), "no stop packet");
        let Ok((n, _)) = server.recv_from(&mut buf) else { continue };
        if let Packet::Mic { opus, .. } = parse_client_datagram(|_| Some(Crypto::new(&SECRET)), &buf[..n]).unwrap().1 {
            assert!(opus.is_empty(), "after the key is let go the packet is empty");
            break;
        }
    }

    // Someone else speaks: their voice, from a player the client can see, comes out of the mixer.
    let mut enc = Encoder::new(&lib, Application::Voip).unwrap();
    for f in 0..5 {
        let s = Sound { channel: 42, sender: 42, opus: enc.encode(&tone(f, 300.0)).unwrap(), sequence: f as i64, distance: 48.0,
            whispering: false, category: None, location: None, group: false };
        server.send_to(&server_datagram(&crypto, &Packet::Sound(s)), client_addr).unwrap();
    }
    engine.set_world([0.0; 3], 0.0, HashMap::from([(42u128, [-6.0, 0.0, 0.0])])); // to the right of someone facing +z
    let mut out = vec![0.0f32; 2 * FRAME];
    let t = Instant::now();
    let (mut left, mut right) = (0.0f32, 0.0f32);
    while left + right == 0.0 {
        assert!(t.elapsed() < Duration::from_secs(8), "the voice never reached the speakers");
        engine.render(&mut out);
        left = out.chunks(2).map(|f| f[0].abs()).sum();
        right = out.chunks(2).map(|f| f[1].abs()).sum();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(right > left * 1.5, "a voice on the right is louder on the right: {left} vs {right}");
    assert!(engine.status().speakers.contains(&42));
    engine.shutdown();
    assert!(!engine.status().connected);
}

fn secret_bytes(s: &SecretPacket) -> Vec<u8> {
    // The mod's layout, written out here independently of the crate's encoder.
    let mut b = Vec::new();
    b.extend_from_slice(&s.secret);
    b.extend_from_slice(&s.server_port.to_be_bytes());
    b.extend_from_slice(&s.player_uuid.to_be_bytes());
    b.push(0);
    b.extend_from_slice(&s.mtu_size.to_be_bytes());
    b.extend_from_slice(&s.voice_chat_distance.to_be_bytes());
    b.extend_from_slice(&s.keep_alive_ms.to_be_bytes());
    b.push(s.groups_enabled as u8);
    b.push(0); // an empty voice host
    b.push(s.allow_recording as u8);
    b
}
