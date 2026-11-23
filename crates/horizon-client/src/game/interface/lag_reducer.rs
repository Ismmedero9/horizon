//! Lag reducer, ported from CashpireSMP's soulmod into the client. It only changes what this
//! window draws: identical dropped items lying close together are drawn as one, primed TNT is
//! thinned out (nearest first), and the TNT thinning switches itself on while the frame rate is low.
//! The server's entities are untouched.
//!
//! Off unless `horizon-mod-data/soulmod/lag-reducer.txt` (written by soulmod's menu) or
//! `horizon-lag-reducer.txt` in the game folder says `enabled:true`, so the game keeps vanilla's
//! picture by default. The file is read again every couple of seconds. Other keys, all optional:
//! `mergeItems`, `itemRadius`, `optimizeTnt`, `tntRadius`, `tntMax`, `autoTnt`, `autoTntFps`, `minStacks`.
use super::{
    Game,
    lag_cluster::{Point, cluster},
};
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq)]
struct Config {
    enabled: bool,
    merge_items: bool,
    /// Identical dropped items closer than this (blocks) are drawn as one.
    item_radius: f64,
    optimize_tnt: bool,
    tnt_radius: f64,
    /// At most this many primed TNT are drawn (nearest first).
    tnt_max: usize,
    /// Switch the TNT thinning on by itself while the frame rate is below `auto_tnt_fps`.
    auto_tnt: bool,
    auto_tnt_fps: f32,
    /// Identical drops are only merged once a cluster has at least this many stacks.
    min_stacks: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            merge_items: true,
            item_radius: 2.0,
            optimize_tnt: true,
            tnt_radius: 2.0,
            tnt_max: 64,
            auto_tnt: true,
            auto_tnt_fps: 30.0,
            min_stacks: 2,
        }
    }
}

impl Config {
    fn parse(text: &str) -> Self {
        let mut c = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            let value = value.trim();
            let flag = || value.eq_ignore_ascii_case("true");
            match key.trim() {
                "enabled" => c.enabled = flag(),
                "mergeItems" => c.merge_items = flag(),
                "itemRadius" => c.item_radius = value.parse().map_or(c.item_radius, |v: f64| v.max(0.5)),
                "optimizeTnt" => c.optimize_tnt = flag(),
                "tntRadius" => c.tnt_radius = value.parse().map_or(c.tnt_radius, |v: f64| v.max(0.5)),
                "tntMax" => c.tnt_max = value.parse().unwrap_or(c.tnt_max),
                "autoTnt" => c.auto_tnt = flag(),
                "autoTntFps" => c.auto_tnt_fps = value.parse().unwrap_or(c.auto_tnt_fps),
                "minStacks" => c.min_stacks = value.parse().unwrap_or(c.min_stacks),
                _ => {}
            }
        }
        c
    }
}

/// What the renderer asks: is this entity left out of the picture.
#[derive(Default)]
pub(in crate::game) struct LagReducer {
    config: Option<Config>,
    hidden: HashSet<u32>,
    auto_tnt_on: bool,
    ticks: u32,
}

impl LagReducer {
    pub(in crate::game) fn hides(&self, id: u32) -> bool {
        !self.hidden.is_empty() && self.hidden.contains(&id)
    }
}

impl Game<'_> {
    /// Once a tick: decide which dropped items and primed TNT are not drawn this tick.
    pub(in crate::game) fn tick_lag_reducer(&mut self) {
        self.lag_reducer.ticks = self.lag_reducer.ticks.wrapping_add(1);
        // Every 40 ticks (two seconds): the menu's file first, then the plain one in the game folder.
        if self.lag_reducer.config.is_none() || self.lag_reducer.ticks % 40 == 0 {
            let dir = horizon_client::logging::player_dir();
            let text = std::fs::read_to_string(dir.join("horizon-mod-data").join("soulmod").join("lag-reducer.txt"))
                .or_else(|_| std::fs::read_to_string(dir.join("horizon-lag-reducer.txt")));
            self.lag_reducer.config = Some(text.map(|t| Config::parse(&t)).unwrap_or_default());
        }
        let config = self.lag_reducer.config.clone().unwrap_or_default();
        self.lag_reducer.hidden.clear();
        if !config.enabled {
            self.lag_reducer.auto_tnt_on = false;
            return;
        }
        let Some(eye) = self.eye_position() else { return };

        // Below the threshold it turns on; it turns off again only well above it, so it does not flap.
        let fps = self.fps;
        if !config.auto_tnt {
            self.lag_reducer.auto_tnt_on = false;
        } else if fps > 0.0 && fps < config.auto_tnt_fps {
            self.lag_reducer.auto_tnt_on = true;
        } else if fps >= config.auto_tnt_fps + 15.0 {
            self.lag_reducer.auto_tnt_on = false;
        }
        let tnt_on = config.optimize_tnt || self.lag_reducer.auto_tnt_on;
        let dist2 = |at: [f64; 3]| (0..3).map(|a| (at[a] - eye[a]).powi(2)).sum::<f64>();
        let mut hidden = Vec::new();

        if config.merge_items {
            let items: Vec<Point<std::sync::Arc<str>>> = self
                .dropped
                .iter()
                .map(|i| Point { id: i.id, at: [i.position.x, i.position.y, i.position.z], key: i.stack.item.clone() })
                .collect();
            // Keep what you can pick up.
            for (_, dups) in cluster(&items, config.item_radius, true, |i| dist2(items[i].at) < 2.25) {
                if dups.is_empty() || dups.len() + 1 < config.min_stacks {
                    continue;
                }
                hidden.extend(dups.into_iter().map(|d| items[d].id));
            }
        }

        if tnt_on {
            let tnt: Vec<Point<()>> = self
                .tnts
                .iter()
                .map(|t| Point { id: t.id, at: [t.position.x, t.position.y, t.position.z], key: () })
                .collect();
            let mut kept = Vec::new();
            for (owner, dups) in cluster(&tnt, config.tnt_radius, false, |_| false) {
                hidden.extend(dups.into_iter().map(|d| tnt[d].id));
                kept.push(owner);
            }
            if kept.len() > config.tnt_max {
                kept.sort_by(|&a, &b| dist2(tnt[a].at).total_cmp(&dist2(tnt[b].at)));
                hidden.extend(kept[config.tnt_max..].iter().map(|&k| tnt[k].id));
            }
        }
        self.lag_reducer.hidden.extend(hidden);
    }
}
