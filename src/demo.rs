//! Demo mode: a made-up match so the phone page can be tried without the game.

use pesm_coaching_zone::lineup::{Lineups, QueueEntry, STATUS_DONE};
use pesm_coaching_zone::pesdb::{self, Database, PlayerInfo};
use pesm_coaching_zone::stats::{KINDS, PERIODS, SLOTS, STARTERS, Slot, Snapshot, kind};
use rand::RngExt;

/// Substitutions per side in the demo.
const SUB_LIMIT: u8 = 5;

const HOME: [(&str, &str); 18] = [
    ("Hale", "GK"),
    ("Okafor", "RB"),
    ("Brandt", "CB"),
    ("Lindqvist", "CB"),
    ("Moreau", "LB"),
    ("Kessler", "DMF"),
    ("Ndiaye", "CMF"),
    ("Ferreira", "RWF"),
    ("Aydin", "AMF"),
    ("Walsh", "LWF"),
    ("Novak", "CF"),
    ("Price", "GK"),
    ("Silva", "CB"),
    ("Duarte", "CMF"),
    ("Mbeki", "CMF"),
    ("Larsen", "RWF"),
    ("Costa", "CF"),
    ("Ibe", "LB"),
];
const AWAY: [(&str, &str); 18] = [
    ("Ward", "GK"),
    ("Reyes", "RB"),
    ("Kovac", "CB"),
    ("Fischer", "CB"),
    ("Doyle", "LB"),
    ("Haddad", "DMF"),
    ("Petit", "CMF"),
    ("Sato", "AMF"),
    ("Mendes", "RWF"),
    ("Oyelaran", "LWF"),
    ("Barros", "CF"),
    ("Lowe", "GK"),
    ("Varga", "CB"),
    ("Nolan", "CMF"),
    ("Quinn", "AMF"),
    ("Ito", "LWF"),
    ("Rossi", "CF"),
    ("Akande", "RB"),
];

pub fn database() -> Database {
    let mut db = Database::default();
    for (side, list) in [HOME, AWAY].iter().enumerate() {
        let team = 9000 + side as u32;
        db.teams.insert(
            team,
            if side == 0 {
                "Sunderland".into()
            } else {
                "Leeds".into()
            },
        );
        for (i, (n, pos)) in list.iter().enumerate() {
            let id = id(side, i);
            let position = pesdb::POSITIONS.iter().find(|p| *p == pos).copied();
            db.players.insert(
                id,
                PlayerInfo {
                    name: n.to_string(),
                    shirt_name: n.to_uppercase(),
                    position,
                },
            );
            db.assignments
                .insert(id, vec![(team, if i == 0 { 1 } else { i as u32 + 1 })]);
        }
    }
    // Sunderland red and white, Leeds white (so the blue second colour is used).
    db.kits.insert(
        9000,
        vec![pesdb::Kit {
            kind: 0,
            color1: [0xd4, 0x15, 0x2f],
            color2: [0xff, 0xff, 0xff],
        }],
    );
    db.kits.insert(
        9001,
        vec![pesdb::Kit {
            kind: 0,
            color1: [0xf4, 0xf4, 0xf4],
            color2: [0x1d, 0x42, 0x8a],
        }],
    );
    db
}

fn id(side: usize, i: usize) -> u32 {
    700_000 + side as u32 * 100 + i as u32
}

/// A match that advances one step per call.
pub struct DemoMatch {
    snap: Snapshot,
    step: u32,
    on: [Vec<usize>; 2],
    bench: [Vec<usize>; 2],
    queue: [Vec<QueueEntry>; 2],
}

/// Steps per period (one step per 2 s tick, so one period is ~40 s).
pub const STEPS_PER_PERIOD: u32 = 20;

impl Default for DemoMatch {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoMatch {
    pub fn new() -> Self {
        let side = |s: usize| {
            (0..SLOTS)
                .map(|i| {
                    (i < 18).then(|| Slot {
                        id: id(s, i),
                        vals: vec![0; KINDS * PERIODS],
                    })
                })
                .collect::<Vec<_>>()
        };
        Self {
            snap: Snapshot {
                sides: [side(0), side(1)],
            },
            step: 0,
            on: [(0..STARTERS).collect(), (0..STARTERS).collect()],
            bench: [(STARTERS..18).collect(), (STARTERS..18).collect()],
            queue: [vec![], vec![]],
        }
    }

    /// The demo's line-ups, shaped like the ones read from FL27's memory.
    pub fn lineups(&self) -> Lineups {
        Lineups {
            xi: [0, 1].map(|s| self.on[s].iter().map(|x| *x as u32).collect()),
            queue: self.queue.clone(),
            limit: [SUB_LIMIT; 2],
            roles: [vec![], vec![]],
        }
    }

    /// Makes a substitution at once.
    fn substitute(&mut self, side: usize, out_slot: u32, in_slot: u32) -> Result<(), String> {
        let (out, inn) = (out_slot as usize, in_slot as usize);
        if side > 1 {
            return Err("Unknown team.".into());
        }
        if self.queue[side].len() >= SUB_LIMIT as usize {
            return Err("No substitutions left.".into());
        }
        let Some(oi) = self.on[side].iter().position(|s| *s == out) else {
            return Err("That player is not on the pitch.".into());
        };
        let Some(bi) = self.bench[side].iter().position(|s| *s == inn) else {
            return Err("That player is not on the bench.".into());
        };
        self.bench[side].remove(bi);
        self.on[side][oi] = inn;
        self.add(side, inn, kind::RUN, 0, 0);
        self.queue[side].push(QueueEntry {
            in_slot,
            out_slot,
            status: STATUS_DONE,
        });
        Ok(())
    }

    fn add(&mut self, side: usize, slot: usize, k: usize, per: usize, v: u32) {
        if let Some(p) = self.snap.sides[side][slot].as_mut() {
            p.vals[k * PERIODS + per] += v;
        }
    }

    pub fn next(&mut self) -> Snapshot {
        let total = STEPS_PER_PERIOD * 6;
        if self.step >= total {
            // Full time: hold, then restart with a fresh match.
            if self.step > total + 30 {
                *self = Self::new();
            }
            self.step += 1;
            return self.snap.clone();
        }
        let per = (self.step / STEPS_PER_PERIOD) as usize;
        let mut rng = rand::rng();
        for side in 0..2 {
            let on = self.on[side].clone();
            for &slot in &on {
                let outfield = slot != 0 && slot != STARTERS;
                self.add(side, slot, kind::RUN, per, rng.random_range(8..14));
                if rng.random_bool(0.6) {
                    let pass = rng.random_bool(0.85);
                    self.add(side, slot, kind::TOUCH, per, 1);
                    self.add(side, slot, kind::PASS, per, 1);
                    if pass {
                        self.add(side, slot, kind::PASS_OK, per, 1);
                    }
                }
                if outfield && rng.random_bool(0.04) {
                    self.add(side, slot, kind::DUEL_WON, per, 1);
                }
                if outfield && rng.random_bool(0.02) {
                    self.add(side, slot, kind::FOUL, per, 1);
                    if rng.random_bool(0.05)
                        && self.snap.sides[side][slot]
                            .as_ref()
                            .unwrap()
                            .total(kind::YELLOW)
                            == 0
                    {
                        self.add(side, slot, kind::YELLOW, per, 1);
                    }
                }
            }
            // Attack: a shot now and then, sometimes a goal.
            if rng.random_bool(0.1) {
                let att: Vec<usize> = on
                    .iter()
                    .copied()
                    .filter(|s| *s >= 6 || (*s >= STARTERS))
                    .collect();
                if let Some(&shooter) = att.get(
                    rng.random_range(0..att.len().max(1))
                        .min(att.len().saturating_sub(1)),
                ) {
                    self.add(side, shooter, kind::SHOT, per, 1);
                    if rng.random_bool(0.45) {
                        self.add(side, shooter, kind::SHOT_ON_TARGET, per, 1);
                        if rng.random_bool(0.35) {
                            self.add(side, shooter, kind::GOAL, per, 1);
                            let helpers: Vec<usize> = on
                                .iter()
                                .copied()
                                .filter(|s| *s != shooter && *s != 0)
                                .collect();
                            if rng.random_bool(0.7) && !helpers.is_empty() {
                                let a = helpers[rng.random_range(0..helpers.len())];
                                self.add(side, a, kind::ASSIST, per, 1);
                            }
                        } else {
                            self.add(1 - side, 0, kind::SAVE, per, 1);
                        }
                    }
                }
            }
            if rng.random_bool(0.03) {
                self.add(
                    side,
                    on[rng.random_range(1..on.len())],
                    kind::CORNER,
                    per,
                    1,
                );
            }
            // The away side makes one substitution in the second half.
            if side == 1 && per >= 3 && self.queue[side].is_empty() && rng.random_bool(0.03) {
                let out = self.on[side][rng.random_range(5..STARTERS)];
                if let Some(&inn) = self.bench[side].iter().find(|s| **s != 11) {
                    let _ = self.substitute(side, out as u32, inn as u32);
                }
            }
        }
        self.step += 1;
        self.snap.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_sides_shoot() {
        let mut m = DemoMatch::new();
        let mut s = m.next();
        for _ in 0..100 {
            s = m.next();
        }
        let shots = [s.side_total(0, kind::SHOT), s.side_total(1, kind::SHOT)];
        assert!(shots[0] > 0 && shots[1] > 0, "{shots:?}");
    }
}
