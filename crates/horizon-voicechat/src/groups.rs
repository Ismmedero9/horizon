//! Groups: the plugin messages that make, join and leave them, and the state the server keeps sending.
//! `voicechat:create_group`, `set_group` (join), `leave_group` go to the server; `add_group`, `remove_group`,
//! `joined_group` and `state` come back.
use crate::wire::{Reader, WireError, Writer};
use std::collections::HashMap;

pub const CHANNEL_CREATE: &str = "voicechat:create_group";
pub const CHANNEL_JOIN: &str = "voicechat:set_group";
pub const CHANNEL_LEAVE: &str = "voicechat:leave_group";
pub const CHANNEL_ADD: &str = "voicechat:add_group";
pub const CHANNEL_REMOVE: &str = "voicechat:remove_group";
pub const CHANNEL_JOINED: &str = "voicechat:joined_group";
pub const CHANNEL_STATE: &str = "voicechat:state";

/// The channels this client listens on. A proxy only passes on a client's plugin messages for channels the
/// client has announced in `minecraft:register`, as Fabric does for the Java mod.
pub const ANNOUNCED: [&str; 8] = [
    "voicechat:secret", "voicechat:state", "voicechat:states", "voicechat:add_group", "voicechat:remove_group",
    "voicechat:joined_group", "voicechat:remove_state", "voicechat:update_state",
];

/// The `minecraft:register` body: the names, each ended by a zero byte.
pub fn register_body() -> Vec<u8> { ANNOUNCED.iter().flat_map(|c| c.bytes().chain(std::iter::once(0))).collect() }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind { Normal, Open, Isolated }

impl Kind {
    pub fn from_short(v: i16) -> Self { match v { 1 => Kind::Open, 2 => Kind::Isolated, _ => Kind::Normal } }
    pub fn short(self) -> i16 { match self { Kind::Normal => 0, Kind::Open => 1, Kind::Isolated => 2 } }
    pub fn name(self) -> &'static str { match self { Kind::Normal => "normal", Kind::Open => "open", Kind::Isolated => "isolated" } }
    pub fn parse(s: &str) -> Self { match s { "open" => Kind::Open, "isolated" => Kind::Isolated, _ => Kind::Normal } }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Group { pub id: u128, pub name: String, pub has_password: bool, pub persistent: bool, pub hidden: bool, pub kind: Kind }

fn short(r: &mut Reader) -> Result<i16, WireError> { Ok(i16::from_be_bytes([r.u8()?, r.u8()?])) }

pub fn parse_add(bytes: &[u8]) -> Result<Group, WireError> {
    let mut r = Reader::new(bytes);
    Ok(Group { id: r.uuid()?, name: r.string(512)?, has_password: r.bool()?, persistent: r.bool()?, hidden: r.bool()?, kind: Kind::from_short(short(&mut r)?) })
}

pub fn parse_remove(bytes: &[u8]) -> Result<u128, WireError> { Reader::new(bytes).uuid() }

/// `joined_group`: the group joined (none: left, or refused) and whether the password was wrong.
pub fn parse_joined(bytes: &[u8]) -> Result<(Option<u128>, bool), WireError> {
    let mut r = Reader::new(bytes);
    let group = if r.bool()? { Some(r.uuid()?) } else { None };
    Ok((group, r.bool()?))
}

/// One player's changed state.
#[derive(Clone, Debug, PartialEq)]
pub struct State { pub uuid: u128, pub name: String, pub disabled: bool, pub disconnected: bool, pub group: Option<u128> }

pub fn parse_state(bytes: &[u8]) -> Result<State, WireError> {
    let mut r = Reader::new(bytes);
    let (disabled, disconnected) = (r.bool()?, r.bool()?);
    let uuid = r.uuid()?;
    let name = r.string(256)?;
    let group = if r.bool()? { Some(r.uuid()?) } else { None };
    Ok(State { uuid, name, disabled, disconnected, group })
}

fn password(w: &mut Writer, p: &str) { if p.is_empty() { w.bool(false); } else { w.bool(true).string(p); } }

pub fn create(name: &str, pass: &str, kind: Kind) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(name);
    password(&mut w, pass);
    w.u8((kind.short() >> 8) as u8).u8(kind.short() as u8);
    w.finish()
}

pub fn join(id: u128, pass: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.uuid(id);
    password(&mut w, pass);
    w.finish()
}

/// What the client knows of the groups.
#[derive(Default, Clone, Debug)]
pub struct Book {
    pub groups: Vec<Group>,
    /// Each player's group and whether their voice chat is disabled.
    pub members: HashMap<u128, (Option<u128>, bool)>,
    pub mine: Option<u128>,
    pub wrong_password: bool,
}

impl Book {
    pub fn on_payload(&mut self, channel: &str, bytes: &[u8]) {
        match channel {
            CHANNEL_ADD => if let Ok(g) = parse_add(bytes) { self.groups.retain(|x| x.id != g.id); self.groups.push(g); },
            CHANNEL_REMOVE => if let Ok(id) = parse_remove(bytes) {
                self.groups.retain(|g| g.id != id);
                for m in self.members.values_mut() { if m.0 == Some(id) { m.0 = None; } }
                if self.mine == Some(id) { self.mine = None; }
            },
            CHANNEL_JOINED => if let Ok((g, wrong)) = parse_joined(bytes) { self.wrong_password = wrong; if !wrong { self.mine = g; } },
            CHANNEL_STATE => if let Ok(s) = parse_state(bytes) {
                if s.disconnected { self.members.remove(&s.uuid); } else { self.members.insert(s.uuid, (s.group, s.disabled)); }
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_register_body_is_zero_ended_names() {
        let b = register_body();
        assert!(b.starts_with(b"voicechat:secret\0voicechat:state\0") && b.ends_with(b"update_state\0"));
    }

    #[test]
    fn a_group_comes_in_and_goes() {
        let mut w = Writer::new();
        w.uuid(7).string("Base").bool(true).bool(false).bool(false).u8(0).u8(1);
        let mut b = Book::default();
        b.on_payload(CHANNEL_ADD, &w.finish());
        assert_eq!(b.groups.len(), 1);
        assert_eq!((b.groups[0].name.as_str(), b.groups[0].has_password, b.groups[0].kind), ("Base", true, Kind::Open));
        let mut r = Writer::new();
        r.uuid(7);
        b.on_payload(CHANNEL_REMOVE, &r.finish());
        assert!(b.groups.is_empty());
    }

    #[test]
    fn joining_and_a_wrong_password() {
        let mut b = Book::default();
        let mut w = Writer::new();
        w.bool(true).uuid(9).bool(false);
        b.on_payload(CHANNEL_JOINED, &w.finish());
        assert_eq!(b.mine, Some(9));
        let mut w = Writer::new();
        w.bool(false).bool(true);
        b.on_payload(CHANNEL_JOINED, &w.finish());
        assert!(b.wrong_password);
        assert_eq!(b.mine, Some(9), "a refusal leaves the group as it was");
        let mut w = Writer::new();
        w.bool(false).bool(false);
        b.on_payload(CHANNEL_JOINED, &w.finish());
        assert_eq!(b.mine, None);
    }

    #[test]
    fn a_player_state_names_the_group() {
        let mut w = Writer::new();
        w.bool(true).bool(false).uuid(5).string("Steve").bool(true).uuid(9);
        let s = parse_state(&w.finish()).unwrap();
        assert_eq!((s.uuid, s.name.as_str(), s.disabled, s.group), (5, "Steve", true, Some(9)));
    }

    #[test]
    fn what_we_send_reads_back() {
        let c = create("Team", "pw", Kind::Isolated);
        let mut r = Reader::new(&c);
        assert_eq!(r.string(512).unwrap(), "Team");
        assert!(r.bool().unwrap());
        assert_eq!(r.string(512).unwrap(), "pw");
        assert_eq!((r.u8().unwrap(), r.u8().unwrap()), (0, 2));
        assert_eq!(join(3, "").len(), 17);
    }
}
