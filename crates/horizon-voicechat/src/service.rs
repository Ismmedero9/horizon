//! Voice chat as a service in the game's process, one for the whole game. The game feeds it the plugin messages
//! and the players' positions; a mod (the menu and the keys) talks to it through small files in one folder:
//!
//! * `control.txt`, written by the mod every tick: `ptt`, `whisper`, `muted`, `deafened`, `micTest` as 0 or 1.
//!   Older than a second, it is treated as gone and push-to-talk lets go.
//! * `config.txt`, the settings (`crate::config`), read when it changes.
//! * `volumes.txt`, each player's own volume (`<uuid>:<percent>`), read when it changes.
//! * `devices.txt`, the audio devices, written at the start and whenever `refreshDevices:1` is in the control file.
//! * `group.txt`, a command from the mod: `seq:N` (new each time), `op:create|join|leave`, `name`, `password`, `type`, `id`.
//! * `status.txt`, written here several times a second: what the mod shows.
use crate::config::Config;
use crate::engine::Engine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

struct Service {
    engine: Engine,
    dir: PathBuf,
    server_host: Mutex<String>,
}

static HIDE_ICONS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static SERVICE: OnceLock<Service> = OnceLock::new();

/// Starts the service once, with the folder the mod shares. Does nothing the second time.
pub fn start(dir: PathBuf) {
    if SERVICE.get().is_some() { return; }
    let _ = std::fs::create_dir_all(&dir);
    let config = std::fs::read_to_string(dir.join("config.txt")).map(|t| Config::parse(&t)).unwrap_or_default();
    let svc = Service { engine: Engine::new(config, true), dir, server_host: Mutex::new(String::new()) };
    if SERVICE.set(svc).is_err() { return; }
    std::thread::Builder::new().name("voicechat-service".into()).spawn(bridge).expect("voice chat service");
}

/// Who has an icon over their name: talking now, voice chat disabled, in a group.
#[derive(Default)]
pub struct Icons { pub talking: std::collections::HashSet<u128>, pub disabled: std::collections::HashSet<u128>, pub grouped: std::collections::HashSet<u128> }

/// The name tag icons, none while the mod says to hide them or voice chat is off.
pub fn icons() -> Option<Icons> {
    let s = svc()?;
    if HIDE_ICONS.load(std::sync::atomic::Ordering::Relaxed) { return None; }
    let st = s.engine.status();
    if !st.connected { return None; }
    let book = s.engine.groups();
    let mut i = Icons { talking: st.speakers.iter().copied().collect(), ..Icons::default() };
    for (u, (g, d)) in &book.members { if *d { i.disabled.insert(*u); } if g.is_some() { i.grouped.insert(*u); } }
    Some(i)
}

fn svc() -> Option<&'static Service> { SERVICE.get() }

/// A plugin message of the server's whose channel starts `voicechat:`.
pub fn on_payload(channel: &str, bytes: &[u8]) {
    if let Some(s) = svc() { let host = s.server_host.lock().unwrap().clone(); s.engine.on_payload(channel, bytes, &host); }
}

/// The player joined a server at `host`: ask it for the secret.
pub fn joined(host: &str) {
    if let Some(s) = svc() {
        *s.server_host.lock().unwrap() = host.to_owned();
        if s.engine.available() && s.engine_enabled() { s.engine.queue_join(); }
    }
}

/// The player left the server.
pub fn left() { if let Some(s) = svc() { s.engine.shutdown(); } }

/// The plugin messages waiting to go to the server.
pub fn take_payloads() -> Vec<(String, Vec<u8>)> { svc().map(|s| s.engine.take_payloads()).unwrap_or_default() }

/// The ears and the players in sight.
pub fn set_world(listener: [f64; 3], yaw: f32, players: HashMap<u128, [f64; 3]>) {
    if let Some(s) = svc() { s.engine.set_world(listener, yaw, players); }
}

impl Service {
    fn engine_enabled(&self) -> bool { true }
}

fn modified(path: &Path) -> Option<SystemTime> { std::fs::metadata(path).and_then(|m| m.modified()).ok() }

fn flag(map: &HashMap<String, String>, key: &str) -> bool { map.get(key).is_some_and(|v| v == "1" || v == "true") }

fn parse_pairs(text: &str) -> HashMap<String, String> {
    text.lines().filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect()
}

/// The audio devices, for the menu's lists.
fn write_devices(path: &Path) {
    #[cfg(feature = "audio")]
    {
        let (ins, outs) = crate::audio::devices();
        let mut text = String::new();
        for n in ins { text.push_str(&format!("in:{n}\n")); }
        for n in outs { text.push_str(&format!("out:{n}\n")); }
        let _ = std::fs::write(path, text);
    }
    #[cfg(not(feature = "audio"))]
    let _ = path;
}

fn bridge() {
    let Some(s) = svc() else { return };
    let (control, config, status) = (s.dir.join("control.txt"), s.dir.join("config.txt"), s.dir.join("status.txt"));
    let mut config_seen: Option<SystemTime> = None;
    let (volumes, devices) = (s.dir.join("volumes.txt"), s.dir.join("devices.txt"));
    let mut volumes_seen: Option<SystemTime> = None;
    let mut devices_asked = true;
    let group_cmd = s.dir.join("group.txt");
    let mut group_seq = std::fs::read_to_string(&group_cmd).map(|t| parse_pairs(&t).get("seq").cloned().unwrap_or_default()).unwrap_or_default();
    write_devices(&devices);
    let mut last_status = String::new();
    let mut last_write = Instant::now() - Duration::from_secs(1);
    loop {
        std::thread::sleep(Duration::from_millis(10));
        // The mod's keys; stale means the mod is not running: let go of everything.
        let fresh = modified(&control).is_some_and(|t| t.elapsed().map_or(true, |e| e < Duration::from_millis(1500)));
        let map = if fresh { std::fs::read_to_string(&control).map(|t| parse_pairs(&t)).unwrap_or_default() } else { HashMap::new() };
        s.engine.set_ptt(flag(&map, "ptt"));
        s.engine.set_whisper(flag(&map, "whisper"));
        if fresh {
            s.engine.set_muted(flag(&map, "muted"));
            s.engine.set_deafened(flag(&map, "deafened"));
        }
        s.engine.set_mic_test(flag(&map, "micTest"));
        HIDE_ICONS.store(flag(&map, "hideIcons"), std::sync::atomic::Ordering::Relaxed);
        let want_devices = flag(&map, "refreshDevices");
        if want_devices && !devices_asked { write_devices(&devices); }
        devices_asked = want_devices;
        if let Ok(text) = std::fs::read_to_string(&group_cmd) {
            let m = parse_pairs(&text);
            let seq = m.get("seq").cloned().unwrap_or_default();
            if seq != group_seq {
                {
                    let g = |k: &str| m.get(k).cloned().unwrap_or_default();
                    match g("op").as_str() {
                        "create" => s.engine.create_group(&g("name"), &g("password"), crate::groups::Kind::parse(&g("type"))),
                        "join" => if let Ok(id) = u128::from_str_radix(&g("id"), 16) { s.engine.join_group(id, &g("password")) },
                        "leave" => s.engine.leave_group(),
                        _ => {}
                    }
                }
                group_seq = seq;
            }
        }
        s.engine.pump();
        // The settings, when the file changes.
        let seen = modified(&config);
        if seen != config_seen {
            config_seen = seen;
            if let Ok(text) = std::fs::read_to_string(&config) {
                let new = Config::parse(&text);
                let devices_changed = { let old = s.engine.config(); old.input_device != new.input_device || old.output_device != new.output_device };
                s.engine.set_config(new);
                if devices_changed { s.engine.restart_audio(); }
            }
        }
        let seen = modified(&volumes);
        if seen != volumes_seen {
            volumes_seen = seen;
            if let Ok(text) = std::fs::read_to_string(&volumes) {
                let v = text.lines().filter_map(|l| l.split_once(':')).filter_map(|(k, p)| Some((u128::from_str_radix(k.trim(), 16).ok()?, p.trim().parse::<f32>().ok()? / 100.0))).collect();
                s.engine.set_player_volumes(v);
            }
        }
        // What the mod shows, a few times a second.
        if last_write.elapsed() >= Duration::from_millis(100) {
            last_write = Instant::now();
            let st = s.engine.status();
            let mut text = format!(
                "available:{}\nconnected:{}\ntalking:{}\nmuted:{}\ndeafened:{}\nlevel:{:.1}\nerror:{}\n",
                st.available as u8, st.connected as u8, st.talking as u8, st.muted as u8, st.deafened as u8, s.engine.level_db(), st.error.clone().unwrap_or_default(),
            );
            for u in &st.speakers { text.push_str(&format!("speaker:{:032x}:{}\n", u, s.engine.name_of(*u).unwrap_or_default())); }
            let book = s.engine.groups();
            if let Some(g) = book.mine { text.push_str(&format!("mygroup:{g:032x}\n")); }
            if book.wrong_password { text.push_str("groupError:wrong_password\n"); }
            for g in book.groups.iter().filter(|g| !g.hidden) {
                text.push_str(&format!("group:{:032x}:{}:{}:{}\n", g.id, g.has_password as u8, g.kind.name(), g.name));
            }
            for (u, (grp, dis)) in &book.members {
                text.push_str(&format!("member:{:032x}:{}:{}:{}\n", u, grp.map_or("-".to_owned(), |g| format!("{g:032x}")), *dis as u8, s.engine.name_of(*u).unwrap_or_default()));
            }
            if text != last_status {
                let tmp = status.with_extension("tmp");
                if std::fs::write(&tmp, &text).is_ok() { let _ = std::fs::rename(&tmp, &status); }
                last_status = text;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_file_is_a_list_of_flags() {
        let m = parse_pairs("ptt:1\nwhisper:0\nmuted:true\nbad line\n");
        assert!(flag(&m, "ptt") && !flag(&m, "whisper") && flag(&m, "muted") && !flag(&m, "deafened"));
    }
}
