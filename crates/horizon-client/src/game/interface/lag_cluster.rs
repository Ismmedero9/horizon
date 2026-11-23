//! The lag reducer's clustering: which of a set of points another, nearby one already stands in for.
//! Greedy, over a grid of cells one radius wide, so it is linear in the number of points.
//! Ported from CashpireSMP's soulmod `LagReducer.cluster`; no game types, so it is tested alone.

use std::collections::HashMap;

/// A point the lag reducer may merge: an id the game knows it by, where it is, and the key it
/// must share with the one that stands in for it (an item's type; `()` for TNT).
pub struct Point<K> {
    pub id: u32,
    pub at: [f64; 3],
    pub key: K,
}

/// Each kept point with the points within `radius` of it that it stands in for, in the order
/// the points were given. `skip(i)` points are left alone (an item you can pick up).
pub fn cluster<K: PartialEq>(
    points: &[Point<K>],
    radius: f64,
    same_key: bool,
    skip: impl Fn(usize) -> bool,
) -> Vec<(usize, Vec<usize>)> {
    let r2 = radius * radius;
    let cell = |v: f64| (v / radius).floor() as i64;
    let mut grid: HashMap<(i64, i64, i64), Vec<usize>> = HashMap::new();
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut group_of: HashMap<usize, usize> = HashMap::new();
    for (i, p) in points.iter().enumerate() {
        if skip(i) {
            continue;
        }
        let (cx, cy, cz) = (cell(p.at[0]), cell(p.at[1]), cell(p.at[2]));
        let mut owner = None;
        'search: for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let Some(kept) = grid.get(&(cx + dx, cy + dy, cz + dz)) else { continue };
                    for &k in kept {
                        let q = &points[k];
                        let d2: f64 = (0..3).map(|a| (q.at[a] - p.at[a]).powi(2)).sum();
                        if d2 <= r2 && (!same_key || q.key == p.key) {
                            owner = Some(k);
                            break 'search;
                        }
                    }
                }
            }
        }
        match owner {
            Some(k) => groups[group_of[&k]].1.push(i),
            None => {
                grid.entry((cx, cy, cz)).or_default().push(i);
                group_of.insert(i, groups.len());
                groups.push((i, Vec::new()));
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: u32, x: f64, key: &'static str) -> Point<&'static str> {
        Point { id, at: [x, 0.0, 0.0], key }
    }

    #[test]
    fn nearby_identical_points_merge_into_the_first() {
        let pts = [p(1, 0.0, "a"), p(2, 1.0, "a"), p(3, 1.9, "a"), p(4, 10.0, "a")];
        let groups = cluster(&pts, 2.0, true, |_| false);
        assert_eq!(groups, vec![(0, vec![1, 2]), (3, vec![])]);
    }

    #[test]
    fn different_keys_do_not_merge_when_the_key_matters() {
        let pts = [p(1, 0.0, "a"), p(2, 0.5, "b")];
        assert_eq!(cluster(&pts, 2.0, true, |_| false), vec![(0, vec![]), (1, vec![])]);
        assert_eq!(cluster(&pts, 2.0, false, |_| false), vec![(0, vec![1])]);
    }

    #[test]
    fn skipped_points_are_left_alone() {
        let pts = [p(1, 0.0, "a"), p(2, 0.5, "a")];
        assert_eq!(cluster(&pts, 2.0, true, |i| i == 1), vec![(0, vec![])]);
    }

    #[test]
    fn points_just_over_a_cell_boundary_still_find_each_other() {
        let pts = [p(1, 1.99, "a"), p(2, 2.01, "a")];
        assert_eq!(cluster(&pts, 2.0, true, |_| false), vec![(0, vec![1])]);
    }

    #[test]
    fn negative_coordinates_work() {
        let pts = [p(1, -0.2, "a"), p(2, 0.2, "a"), p(3, -5.0, "a")];
        assert_eq!(cluster(&pts, 2.0, true, |_| false), vec![(0, vec![1]), (2, vec![])]);
    }
}
