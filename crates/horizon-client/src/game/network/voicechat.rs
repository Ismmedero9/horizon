//! Voice chat on a server: the plugin messages of Simple Voice Chat (`voicechat:*`) go to the voice chat service
//! (`horizon-voicechat`), which does the rest, and what it wants to send goes back down the connection. The
//! menu and the keys are a mod's (they reach the service through files, see `horizon_voicechat::service`).
use super::*;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};

/// The player's entity id on the server the service was last told about: a new one is a new server
/// (a proxy switching backends), which has to be asked for its own secret.
static LAST_ENTITY: AtomicI32 = AtomicI32::new(0);

/// Whether the service has been told about the server this window is on.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// `mc.example.com:25565` and `[::1]:25565` to the host alone.
fn host_of(address: &str) -> &str {
    if let Some(rest) = address.strip_prefix('[') { return rest.split(']').next().unwrap_or(rest); }
    match address.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !host.contains(':') => host,
        _ => address,
    }
}

impl Game<'_> {
    /// A `voicechat:` plugin message from the server.
    pub(super) fn voicechat_payload(&self, channel: &str, data: &[u8]) {
        horizon_voicechat::service::on_payload(channel, data);
    }

    /// Once a tick: join the voice chat on a server, keep it supplied with positions, send what it has to send,
    /// and let go of the microphone when the player leaves.
    pub(super) fn voicechat_tick(&mut self) {
        let on_server = self.integrated.is_none() && self.remote.as_ref().is_some_and(|r| r.logged_in);
        if !on_server {
            if ACTIVE.swap(false, Relaxed) { horizon_voicechat::service::left(); }
            return;
        }
        let entity = self.own_entity_id();
        if ACTIVE.load(Relaxed) && LAST_ENTITY.swap(entity, Relaxed) != entity {
            // Another server behind the same connection: let go of the old one's voice chat and start over.
            horizon_voicechat::service::left();
            ACTIVE.store(false, Relaxed);
        }
        LAST_ENTITY.store(entity, Relaxed);
        if !ACTIVE.swap(true, Relaxed) {
            horizon_voicechat::service::start(horizon_client::logging::player_dir().join("horizon-mod-data").join("voicechat"));
            let host = self.server_address().map(host_of).unwrap_or("").to_owned();
            // Announce the channels in the play phase too (a proxy keeps its own list per phase).
            self.send(horizon_protocol::packets::play::Serverbound::CustomPayload(horizon_protocol::packets::configuration::CustomPayload {
                channel: "minecraft:register".into(),
                data: horizon_voicechat::groups::register_body(),
            }));
            horizon_voicechat::service::joined(&host);
        }
        // The ears and the players the client can see.
        if let (Some(eye), Some(remote)) = (self.eye_position(), self.remote.as_ref()) {
            let players = remote
                .level_players
                .iter()
                .filter(|(_, uuid)| **uuid != remote.profile)
                .filter_map(|(id, uuid)| {
                    let m = self.mobs.iter().find(|m| m.id == *id as u32 && !m.removed)?;
                    let p = m.position;
                    Some((*uuid, [p.x, p.y + 1.62, p.z]))
                })
                .collect();
            horizon_voicechat::service::set_world(eye, self.yaw, players);
        }
        for (channel, data) in horizon_voicechat::service::take_payloads() {
            self.send(horizon_protocol::packets::play::Serverbound::CustomPayload(horizon_protocol::packets::configuration::CustomPayload { channel: channel.into(), data }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::host_of;

    #[test]
    fn the_host_is_what_voice_chat_connects_to() {
        assert_eq!(host_of("mc.example.com:25565"), "mc.example.com");
        assert_eq!(host_of("mc.example.com"), "mc.example.com");
        assert_eq!(host_of("[::1]:25565"), "::1");
        assert_eq!(host_of("10.0.0.5:30000"), "10.0.0.5");
        assert_eq!(host_of("fe80::1"), "fe80::1");
    }
}
