//! The FL27 per-player statistics block in game memory.
//!
//! Layout (FL_2027.exe 1.00):
//! - Two sides (home, away), `SIDE_STRIDE` bytes apart, 40 slots each.
//! - Each slot is a `RECORD`-byte record: u32 head (65533 home / 65534 away), u32 player id,
//!   then for every stat kind `k` nine u32 values at `+8 + k*36`, one per match period
//!   (0-15, 15-30, 30-45, 45-60, 60-75, 75-90, stoppage, extra time 1, extra time 2).
//! - Slots 0..10 are the starting eleven, 11.. the bench. Slots never move on a substitution.

use crate::process::Memory;

pub const RECORD: usize = 0x2134;
pub const SIDE_STRIDE: usize = 0x59650;
pub const SLOTS: usize = 40;
pub const KINDS: usize = 0xB0;
pub const PERIODS: usize = 9;
pub const STARTERS: usize = 11;
/// Record heads, low 16 bits only: the upper bytes vary by match (seen 00 00 and 07 07).
pub const HEAD: [u32; 2] = [65533, 65534];

/// A record head of `side`: low 16 bits fd ff / fe ff, upper 16 bits anything but ff ff
/// (that would just be -3 / -2).
fn is_head(v: u32, side: usize) -> bool {
    v & 0xffff == HEAD[side] && v >> 16 != 0xffff
}

fn is_player_id(v: u32) -> bool {
    (1..1_000_000).contains(&v)
}

/// Stat kinds confirmed in real matches.
pub mod kind {
    pub const GOAL: usize = 0;
    pub const OWN_GOAL: usize = 1;
    pub const FREE_KICK_GOAL: usize = 2;
    pub const PENALTY_GOAL: usize = 3;
    pub const ASSIST: usize = 4;
    pub const SHOT: usize = 6;
    pub const SHOT_ON_TARGET: usize = 17;
    pub const PENALTY_SHOT: usize = 21;
    pub const PASS: usize = 22;
    pub const PASS_OK: usize = 23;
    pub const CROSS: usize = 30;
    pub const CROSS_OK: usize = 31;
    pub const DUEL_WON: usize = 50;
    pub const FOUL: usize = 55;
    pub const OFFSIDE: usize = 56;
    pub const YELLOW: usize = 57;
    pub const RED: usize = 58;
    pub const FREE_KICK: usize = 61;
    pub const CORNER: usize = 63;
    pub const TOUCH: usize = 67;
    pub const INTERCEPTION: usize = 78;
    pub const SAVE: usize = 133;
    /// Grows only while the player is on the pitch. Not minutes.
    pub const RUN: usize = 173;
}

/// One filled slot of the block.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub id: u32,
    /// `KINDS * PERIODS` values, kind-major.
    pub vals: Vec<u32>,
}

impl Slot {
    pub fn get(&self, kind: usize, period: usize) -> u32 {
        self.vals[kind * PERIODS + period]
    }
    pub fn total(&self, kind: usize) -> u32 {
        (0..PERIODS).map(|p| self.get(kind, p)).sum()
    }
    /// True once the player has any recorded activity (used to detect subs coming on).
    pub fn active(&self) -> bool {
        self.total(kind::RUN) > 0 || self.total(kind::TOUCH) > 0 || self.total(kind::PASS) > 0
    }
}

/// One reading of the whole block.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// `[home, away]`, each `SLOTS` long.
    pub sides: [Vec<Option<Slot>>; 2],
}

impl Snapshot {
    pub fn parse(home: &[u8], away: &[u8]) -> Option<Self> {
        let side = |buf: &[u8], s: usize| -> Vec<Option<Slot>> {
            (0..SLOTS)
                .map(|i| {
                    let r = buf.get(i * RECORD..(i + 1) * RECORD)?;
                    let u = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
                    if !is_head(u(0), s) || !is_player_id(u(4)) {
                        return None;
                    }
                    let vals = (0..KINDS * PERIODS).map(|j| u(8 + j * 4)).collect();
                    Some(Slot { id: u(4), vals })
                })
                .collect()
        };
        let s = Snapshot {
            sides: [side(home, 0), side(away, 1)],
        };
        (s.sides[0][..STARTERS].iter().any(Option::is_some)
            && s.sides[1][..STARTERS].iter().any(Option::is_some))
        .then_some(s)
    }

    pub fn read(mem: &dyn Memory, base: u64) -> Option<Self> {
        let home = mem.read(base, SLOTS * RECORD)?;
        let away = mem.read(base + SIDE_STRIDE as u64, SLOTS * RECORD)?;
        Self::parse(&home, &away)
    }

    /// Ids of the starting eleven of both sides; identifies a match.
    pub fn lineup_key(&self) -> Vec<u32> {
        self.sides
            .iter()
            .flat_map(|s| s[..STARTERS].iter().map(|x| x.as_ref().map_or(0, |p| p.id)))
            .collect()
    }

    /// Rough "how far is this match" measure; drops when a new match reuses the block.
    pub fn volume(&self) -> u64 {
        self.players()
            .map(|(_, _, p)| (p.total(kind::PASS) + p.total(kind::TOUCH)) as u64)
            .sum()
    }

    pub fn players(&self) -> impl Iterator<Item = (usize, usize, &Slot)> {
        self.sides.iter().enumerate().flat_map(|(s, v)| {
            v.iter()
                .enumerate()
                .filter_map(move |(i, p)| p.as_ref().map(|p| (s, i, p)))
        })
    }

    pub fn side_total(&self, side: usize, kind: usize) -> u32 {
        self.sides[side]
            .iter()
            .flatten()
            .map(|p| p.total(kind))
            .sum()
    }

    /// Goals for `[home, away]`: own goals count for the other side.
    pub fn score(&self) -> [u32; 2] {
        [
            self.side_total(0, kind::GOAL) + self.side_total(1, kind::OWN_GOAL),
            self.side_total(1, kind::GOAL) + self.side_total(0, kind::OWN_GOAL),
        ]
    }

    /// Latest period with any activity.
    pub fn period(&self) -> usize {
        (0..PERIODS)
            .rev()
            .find(|&per| {
                self.players().any(|(_, _, p)| {
                    p.get(kind::TOUCH, per) + p.get(kind::PASS, per) + p.get(kind::RUN, per) > 0
                })
            })
            .unwrap_or(0)
    }
}

/// Reads `len` bytes in 1 MiB pieces; unreadable pieces become zeros.
/// None if nothing at all could be read.
pub fn read_in_pieces(mem: &dyn Memory, addr: u64, len: usize) -> Option<Vec<u8>> {
    const PIECE: usize = 1 << 20;
    let mut out = vec![0u8; len];
    let mut any = false;
    let mut o = 0;
    while o < len {
        let n = PIECE.min(len - o);
        if let Some(b) = mem.read(addr + o as u64, n) {
            out[o..o + b.len().min(n)].copy_from_slice(&b[..b.len().min(n)]);
            any = true;
        }
        o += n;
    }
    any.then_some(out)
}

/// Scans all writable regions for statistics blocks and returns the address of home slot 0
/// for each block found. Takes a few seconds on a running game.
pub fn find_blocks(mem: &dyn Memory) -> Vec<u64> {
    const CHUNK: usize = 64 << 20;
    let need = SIDE_STRIDE + RECORD;
    let mut found = vec![];
    for r in mem.regions() {
        if (r.len as usize) < need {
            continue;
        }
        let mut off = 0usize;
        while off + need <= r.len as usize {
            let len = (CHUNK + need).min(r.len as usize - off);
            // One unreadable page must not hide the whole chunk: fall back to 1 MiB pieces.
            let Some(buf) = mem
                .read(r.start + off as u64, len)
                .or_else(|| read_in_pieces(mem, r.start + off as u64, len))
            else {
                off += CHUNK;
                continue;
            };
            let u = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
            let mut i = 0;
            while i + SIDE_STRIDE + 4 <= buf.len() && i < CHUNK {
                // A real block: the same head on three slots in a row, the away side
                // SIDE_STRIDE further, and a plausible player id.
                if is_head(u(i), 0)
                    && u(i + RECORD) == u(i)
                    && u(i + 2 * RECORD) == u(i)
                    && is_head(u(i + SIDE_STRIDE), 1)
                    && is_player_id(u(i + 4))
                {
                    let addr = r.start + (off + i) as u64;
                    found.push(addr);
                }
                i += 4;
            }
            off += CHUNK;
        }
    }
    // Walk back to slot 0 and drop duplicates (every slot of a block matches).
    let mut starts: Vec<u64> = found
        .iter()
        .map(|&a| {
            let mut s = a;
            while found
                .binary_search(&(s.wrapping_sub(RECORD as u64)))
                .is_ok()
            {
                s -= RECORD as u64;
            }
            s
        })
        .collect();
    starts.sort_unstable();
    starts.dedup();
    starts
}

#[cfg(test)]
pub mod fake {
    //! Builds fake game memory for tests and the demo.
    use super::*;
    use crate::process::{Memory, Region};

    pub struct FakeMem {
        pub base: u64,
        pub bytes: Vec<u8>,
    }

    impl Memory for FakeMem {
        fn read(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
            let o = addr.checked_sub(self.base)? as usize;
            self.bytes.get(o..o + len).map(<[u8]>::to_vec)
        }
        fn regions(&self) -> Vec<Region> {
            vec![Region {
                start: self.base,
                len: self.bytes.len() as u64,
            }]
        }
    }

    pub fn put(bytes: &mut [u8], block: usize, side: usize, slot: usize, id: u32) {
        let r = block + side * SIDE_STRIDE + slot * RECORD;
        bytes[r..r + 4].copy_from_slice(&HEAD[side].to_le_bytes());
        bytes[r + 4..r + 8].copy_from_slice(&id.to_le_bytes());
    }

    pub fn set(
        bytes: &mut [u8],
        block: usize,
        side: usize,
        slot: usize,
        k: usize,
        per: usize,
        v: u32,
    ) {
        let o = block + side * SIDE_STRIDE + slot * RECORD + 8 + (k * PERIODS + per) * 4;
        bytes[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    fn mem_with_block(at: usize) -> FakeMem {
        let mut bytes = vec![0u8; at + 2 * SIDE_STRIDE + 0x1000];
        for s in 0..2 {
            for i in 0..18 {
                put(&mut bytes, at, s, i, 1000 * (s as u32 + 1) + i as u32);
            }
        }
        set(&mut bytes, at, 0, 9, kind::GOAL, 1, 1);
        set(&mut bytes, at, 1, 4, kind::OWN_GOAL, 3, 1);
        set(&mut bytes, at, 1, 10, kind::GOAL, 3, 1);
        set(&mut bytes, at, 0, 5, kind::TOUCH, 3, 12);
        FakeMem {
            base: 0x1000_0000,
            bytes,
        }
    }

    #[test]
    fn finds_block_and_reads_score() {
        let m = mem_with_block(0x2340);
        let blocks = find_blocks(&m);
        assert_eq!(blocks, vec![0x1000_0000 + 0x2340]);
        let s = Snapshot::read(&m, blocks[0]).unwrap();
        assert_eq!(s.sides[0][0].as_ref().unwrap().id, 1000);
        assert_eq!(s.sides[1][17].as_ref().unwrap().id, 2017);
        assert!(s.sides[0][18].is_none());
        assert_eq!(s.score(), [2, 1]);
        assert_eq!(s.period(), 3);
    }

    #[test]
    fn finds_block_with_other_head_bytes() {
        // Some matches have heads like fd ff 07 07 instead of fd ff 00 00.
        let at = 0x2340;
        let mut m = mem_with_block(at);
        for side in 0..2 {
            for slot in 0..18 {
                let r = at + side * SIDE_STRIDE + slot * RECORD;
                m.bytes[r + 2..r + 4].copy_from_slice(&[7, 7]);
            }
        }
        let blocks = find_blocks(&m);
        assert_eq!(blocks, vec![0x1000_0000 + at as u64]);
        let s = Snapshot::read(&m, blocks[0]).unwrap();
        assert_eq!(s.sides[1][17].as_ref().unwrap().id, 2017);
        assert_eq!(s.score(), [2, 1]);
    }

    #[test]
    fn ignores_stray_heads() {
        let mut bytes = vec![0u8; 2 * SIDE_STRIDE + 0x4000];
        // fd ff here and fe ff one side further, but no run of slots: noise.
        bytes[0x100..0x104].copy_from_slice(&[0xfd, 0xff, 0x12, 0x34]);
        bytes[0x104..0x108].copy_from_slice(&5u32.to_le_bytes());
        let o = 0x100 + SIDE_STRIDE;
        bytes[o..o + 4].copy_from_slice(&[0xfe, 0xff, 0x12, 0x34]);
        // -3 repeated is not a head either.
        for k in 0..3 {
            let r = 0x200 + k * RECORD;
            bytes[r..r + 4].copy_from_slice(&(-3i32).to_le_bytes());
            bytes[r + 4..r + 8].copy_from_slice(&5u32.to_le_bytes());
        }
        let m = FakeMem { base: 0, bytes };
        assert!(find_blocks(&m).is_empty());
    }

    #[test]
    fn no_block_in_empty_memory() {
        let m = FakeMem {
            base: 0,
            bytes: vec![0; 2 * SIDE_STRIDE],
        };
        assert!(find_blocks(&m).is_empty());
    }
}
