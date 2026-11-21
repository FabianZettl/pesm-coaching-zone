//! The live line-up and the substitution queue of both teams (FL_2027.exe 1.00).
//!
//! Layout, relative to the home team's list `L` (found by pattern, heap address changes per match):
//! - `L + 0x000`: u8[40] position -> squad slot. Positions 0..10 are the eleven, 11.. the bench.
//! - `L + 0x028`: u8[6] slots (set-piece takers), then `ff ff ff` at `+0x02e`.
//! - `L + 0x038`: u32[11] squad slots actually on the pitch.
//! - The away team has the same structure at `L + 0x248`.
//! - Substitution queues: home at `L + 0x2b4`, away at `L + 0x5f4`, 6 entries of
//!   u32 in-slot, u32 out-slot, u32 status (free = ff/ff/0, requested = 1, done = 0x101).
//!   The 4 bytes before each queue read `?? ff 00 NN`, NN = substitution limit.
//!
//! A substitution is requested exactly like the in-game menu does it: swap the two slots in the
//! u8 list and add a "requested" entry to the queue. The game then makes the change at the next
//! stoppage.

use crate::process::Memory;
use anyhow::{Result, bail};
use serde::Serialize;

pub const AWAY: u64 = 0x248;
pub const XI: u64 = 0x38;
pub const QUEUE: [u64; 2] = [0x2b4, 0x5f4];
pub const QUEUE_LEN: usize = 6;
pub const STATUS_REQUESTED: u32 = 1;
pub const STATUS_DONE: u32 = 0x101;
const TEAM_LEN: usize = 0x38 + 11 * 4;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct QueueEntry {
    pub in_slot: u32,
    pub out_slot: u32,
    pub status: u32,
}

impl QueueEntry {
    pub fn free(&self) -> bool {
        self.status == 0 && self.in_slot == 0xff
    }
}

/// One reading of both teams.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Lineups {
    /// Squad slots on the pitch, `[home, away]`.
    pub xi: [Vec<u32>; 2],
    pub queue: [Vec<QueueEntry>; 2],
    pub limit: [u8; 2],
}

fn team_ok(b: &[u8]) -> bool {
    if b.len() < TEAM_LEN {
        return false;
    }
    let list = &b[..40];
    if list.iter().any(|&x| x >= 40) || b[40..46].iter().any(|&x| x >= 40) || b[46..49] != [0xff; 3]
    {
        return false;
    }
    let mut seen = [false; 40];
    let mut distinct = 0;
    for &x in list {
        if !seen[x as usize] {
            seen[x as usize] = true;
            distinct += 1;
        }
    }
    distinct >= 36
        && (0..11).all(|i| {
            let v = u32::from_le_bytes(b[0x38 + i * 4..0x3c + i * 4].try_into().unwrap());
            v < 40
        })
}

fn head_ok(b: &[u8]) -> bool {
    b.len() >= 4 && b[1] == 0xff && b[2] == 0 && (1..=12).contains(&b[3])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn parse_queue(b: &[u8]) -> Vec<QueueEntry> {
    (0..QUEUE_LEN)
        .map(|i| QueueEntry {
            in_slot: u32_at(b, i * 12),
            out_slot: u32_at(b, i * 12 + 4),
            status: u32_at(b, i * 12 + 8),
        })
        .collect()
}

fn queue_ok(q: &[QueueEntry]) -> bool {
    q.iter().all(|e| {
        e.free()
            || (e.in_slot < 40
                && e.out_slot < 40
                && (e.status == STATUS_REQUESTED || e.status == STATUS_DONE))
    })
}

/// Checks a candidate home list address.
pub fn verify(mem: &dyn Memory, home: u64) -> bool {
    let Some(b) = mem.read(home, QUEUE[1] as usize + QUEUE_LEN * 12) else {
        return false;
    };
    let away = AWAY as usize;
    team_ok(&b[..TEAM_LEN])
        && team_ok(&b[away..away + TEAM_LEN])
        && head_ok(&b[QUEUE[0] as usize - 4..])
        && head_ok(&b[QUEUE[1] as usize - 4..])
        && queue_ok(&parse_queue(&b[QUEUE[0] as usize..]))
        && queue_ok(&parse_queue(&b[QUEUE[1] as usize..]))
}

/// Scans the game's memory for the home list. Takes a few seconds.
pub fn find(mem: &dyn Memory) -> Vec<u64> {
    const CHUNK: usize = 64 << 20;
    let span = QUEUE[1] as usize + QUEUE_LEN * 12;
    let mut out = vec![];
    for r in mem.regions() {
        let len = r.len as usize;
        if len < span {
            continue;
        }
        let mut off = 0usize;
        while off + span <= len {
            let n = (CHUNK + span).min(len - off);
            if let Some(b) = mem.read(r.start + off as u64, n) {
                // Cheap pre-filter: the home queue head "?? ff 00 NN" at +0x2b0 and "ff ff ff" at +0x2e.
                let h = QUEUE[0] as usize - 4;
                let mut i = 0;
                while i + span <= b.len() && i < CHUNK {
                    if b[i + 46] == 0xff
                        && b[i + 47] == 0xff
                        && b[i + 48] == 0xff
                        && head_ok(&b[i + h..])
                        && team_ok(&b[i..i + TEAM_LEN])
                    {
                        let addr = r.start + (off + i) as u64;
                        if verify(mem, addr) {
                            out.push(addr);
                        }
                    }
                    i += 1;
                }
            }
            off += CHUNK;
        }
    }
    out
}

pub fn read(mem: &dyn Memory, home: u64) -> Option<Lineups> {
    let b = mem.read(home, QUEUE[1] as usize + QUEUE_LEN * 12)?;
    let away = AWAY as usize;
    if !team_ok(&b[..TEAM_LEN]) || !team_ok(&b[away..away + TEAM_LEN]) {
        return None;
    }
    let xi = |base: usize| {
        (0..11)
            .map(|i| u32_at(&b, base + XI as usize + i * 4))
            .collect::<Vec<_>>()
    };
    let q = |side: usize| parse_queue(&b[QUEUE[side] as usize..]);
    let lim = |side: usize| b[QUEUE[side] as usize - 1];
    let l = Lineups {
        xi: [xi(0), xi(away)],
        queue: [q(0), q(1)],
        limit: [lim(0), lim(1)],
    };
    (queue_ok(&l.queue[0]) && queue_ok(&l.queue[1])).then_some(l)
}

/// Requests a substitution the way the in-game menu does. The game carries it out at the next stoppage.
pub fn request_sub(
    mem: &dyn Memory,
    home: u64,
    side: usize,
    out_slot: u32,
    in_slot: u32,
    _minute: u32,
) -> Result<()> {
    if side > 1 {
        bail!("unknown team");
    }
    let base = home + if side == 1 { AWAY } else { 0 };
    let Some(xb) = mem.read(base + XI, 11 * 4) else {
        bail!("could not read the line-up")
    };
    let Some(qb) = mem.read(home + QUEUE[side], QUEUE_LEN * 12) else {
        bail!("could not read the substitution queue")
    };
    let Some(head) = mem.read(home + QUEUE[side] - 4, 4) else {
        bail!("could not read the substitution queue")
    };
    let queue = parse_queue(&qb);
    if !queue_ok(&queue) || !head_ok(&head) {
        bail!("the line-up in memory looks different than expected; is this FL_2027.exe 1.00?");
    }
    if out_slot >= 40 || in_slot >= 40 {
        bail!("player not found in the line-up");
    }
    let Some(list) = mem.read(base, 40) else {
        bail!("could not read the line-up")
    };
    let xi: Vec<u32> = (0..11).map(|i| u32_at(&xb, i * 4)).collect();
    // FL27 checks its own line-up list when it makes the change and drops the sub
    // unless the player going off is in the eleven there and the one coming on is on the bench.
    let (eleven, bench) = list.split_at(11);
    if !eleven.contains(&(out_slot as u8))
        || !bench.contains(&(in_slot as u8))
        || eleven.contains(&(in_slot as u8))
    {
        bail!(
            "FL27's own line-up doesn't match this change. Please use the pause menu for this one."
        );
    }
    if !xi.contains(&out_slot) {
        bail!("the player going off is not on the pitch");
    }
    if xi.contains(&in_slot) {
        bail!("the player coming on is already on the pitch");
    }
    if queue
        .iter()
        .any(|e| e.status == STATUS_REQUESTED && (e.in_slot == in_slot || e.out_slot == out_slot))
    {
        bail!("one of these players is already part of a substitution");
    }
    if queue
        .iter()
        .any(|e| e.status == STATUS_DONE && e.out_slot == in_slot && !xi.contains(&in_slot))
    {
        bail!("that player was already substituted off");
    }
    let used = queue.iter().filter(|e| !e.free()).count();
    if used >= head[3] as usize {
        bail!("no substitutions left");
    }
    let Some(free) = queue.iter().position(QueueEntry::free) else {
        bail!("no substitutions left")
    };

    // Only the queue entry, exactly like the pause menu. The game swaps its own
    // line-up lists when it makes the change; touching them first makes it drop the sub.
    let e = home + QUEUE[side] + free as u64 * 12;
    let mut slots = in_slot.to_le_bytes().to_vec();
    slots.extend(out_slot.to_le_bytes());
    if !mem.write(e, &slots) || !mem.write(e + 8, &STATUS_REQUESTED.to_le_bytes()) {
        bail!("writing to the game was not allowed");
    }
    // The first byte of the queue head is 0xff for a team a person controls. Then the
    // engine leaves the queue alone (only the pause menu works through it) and drops our
    // entry at the next stoppage. Computer teams have a slot number there, and the engine
    // makes their subs itself. Setting it makes the engine take our sub too.
    // 0x0a worked in two matches; other values are left as they are.
    if head[0] == 0xff && !mem.write(home + QUEUE[side] - 4, &[0x0a]) {
        bail!("writing to the game was not allowed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Region;
    use std::cell::RefCell;

    struct Fake {
        base: u64,
        bytes: RefCell<Vec<u8>>,
    }
    impl Memory for Fake {
        fn read(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
            let o = addr.checked_sub(self.base)? as usize;
            self.bytes.borrow().get(o..o + len).map(<[u8]>::to_vec)
        }
        fn regions(&self) -> Vec<Region> {
            vec![Region {
                start: self.base,
                len: self.bytes.borrow().len() as u64,
            }]
        }
        fn write(&self, addr: u64, b: &[u8]) -> bool {
            let o = (addr - self.base) as usize;
            self.bytes.borrow_mut()[o..o + b.len()].copy_from_slice(b);
            true
        }
    }

    fn team(b: &mut [u8], at: usize) {
        for i in 0..40 {
            b[at + i] = i as u8;
        }
        b[at + 40..at + 46].copy_from_slice(&[5, 5, 6, 5, 5, 5]);
        b[at + 46..at + 49].copy_from_slice(&[0xff; 3]);
        for i in 0..11 {
            b[at + 0x38 + i * 4..at + 0x3c + i * 4].copy_from_slice(&(i as u32).to_le_bytes());
        }
    }
    fn queue(b: &mut [u8], at: usize, head0: u8) {
        b[at - 4..at].copy_from_slice(&[head0, 0xff, 0, 5]);
        for i in 0..QUEUE_LEN {
            let o = at + i * 12;
            b[o..o + 4].copy_from_slice(&0xffu32.to_le_bytes());
            b[o + 4..o + 8].copy_from_slice(&0xffu32.to_le_bytes());
            b[o + 8..o + 12].copy_from_slice(&0u32.to_le_bytes());
        }
    }
    fn fake(at: usize) -> Fake {
        let mut b = vec![0x11u8; at + 0x1000];
        team(&mut b, at);
        queue(&mut b, at + QUEUE[0] as usize, 0x0a);
        // The away team overlaps the home queue region the same way the game lays it out.
        team(&mut b, at + AWAY as usize);
        queue(&mut b, at + QUEUE[1] as usize, 0);
        Fake {
            base: 0x4000_0000,
            bytes: RefCell::new(b),
        }
    }

    #[test]
    fn finds_and_reads() {
        let m = fake(0x333);
        let found = find(&m);
        assert_eq!(found, vec![0x4000_0333]);
        let l = read(&m, found[0]).unwrap();
        assert_eq!(l.xi[0], (0..11).collect::<Vec<u32>>());
        assert_eq!(l.limit, [5, 5]);
        assert!(l.queue[1].iter().all(QueueEntry::free));
    }

    #[test]
    fn request_sub_like_the_menu() {
        let m = fake(0x300);
        let home = 0x4000_0300;
        m.write(home + QUEUE[0] - 4, &[0xff]);
        request_sub(&m, home, 0, 10, 28, 25).unwrap();
        assert_eq!(m.read(home + QUEUE[0] - 4, 1).unwrap(), vec![10]);
        // The menu leaves the line-up lists alone; so do we.
        let list = m.read(home, 40).unwrap();
        assert_eq!(list[10], 10);
        assert_eq!(list[28], 28);
        let l = read(&m, home).unwrap();
        assert_eq!(
            l.queue[0][0],
            QueueEntry {
                in_slot: 28,
                out_slot: 10,
                status: STATUS_REQUESTED
            }
        );
        // Same players again, or a player already on the pitch, is refused.
        assert!(request_sub(&m, home, 0, 10, 29, 25).is_err());
        assert!(request_sub(&m, home, 0, 3, 4, 25).is_err());
        // Away side writes its own queue.
        request_sub(&m, home, 1, 9, 22, 25).unwrap();
        assert_eq!(read(&m, home).unwrap().queue[1][0].in_slot, 22);
    }
}
