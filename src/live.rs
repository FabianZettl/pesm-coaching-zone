//! Turns successive snapshots of the stats block into a live match view:
//! score, events (goals, cards, substitutions), player and team stats.

use crate::lineup::{Lineups, STATUS_DONE, STATUS_REQUESTED};
use crate::pesdb::Database;
use crate::stats::{PERIODS, SLOTS, STARTERS, Snapshot, kind};
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Goal,
    OwnGoal,
    Yellow,
    SecondYellow,
    Red,
    Sub,
    PenaltyMissed,
    Chance,
    ShotWide,
    Corner,
    FreeKick,
    Offside,
}

impl EventKind {
    /// Goals, cards, subs, penalties and saved chances. The rest is "all events" only.
    pub fn key(&self) -> bool {
        !matches!(
            self,
            EventKind::ShotWide | EventKind::Corner | EventKind::FreeKick | EventKind::Offside
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u32,
    pub minute: u32,
    pub side: usize,
    pub kind: EventKind,
    pub slot: usize,
    /// For goals: the assisting slot. For subs: the slot that went off (if known).
    pub other: Option<usize>,
    /// For goals: 1 = penalty, 2 = free kick. For chances: the keeper's slot on the other side + 100.
    pub detail: u32,
    pub key: bool,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
struct PlayerState {
    last_run: u32,
    last_growth: u64,
    on: bool,
    came_on: bool,
    went_off: bool,
    sent_off: bool,
}

struct PendingSub {
    /// Value of `moving[side]` when the sub came on.
    moving_at: u64,
    side: usize,
    slot: usize,
    tick: u64,
    minute: u32,
    seq: u32,
}

pub struct Tracker {
    key: Vec<u32>,
    prev: Option<Snapshot>,
    tick: u64,
    players: [Vec<PlayerState>; 2],
    pending: Vec<PendingSub>,
    /// Per side: number of ticks in which the team's run counter grew (game not paused).
    moving: [u64; 2],
    /// The real line-up and substitution queue from memory, when found.
    lineups: Option<Lineups>,
    /// Whether substitutions can be requested (line-up found and writing allowed).
    pub can_sub: bool,
    pub events: Vec<Event>,
    seq: u32,
    period_seen: [Option<Instant>; PERIODS],
    period_lens: Vec<Duration>,
    now: Instant,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            key: vec![],
            prev: None,
            tick: 0,
            players: [
                vec![PlayerState::default(); SLOTS],
                vec![PlayerState::default(); SLOTS],
            ],
            pending: vec![],
            moving: [0, 0],
            lineups: None,
            can_sub: false,
            events: vec![],
            seq: 0,
            period_seen: [None; PERIODS],
            period_lens: vec![],
            now: Instant::now(),
        }
    }

    /// True if this snapshot belongs to a different (new) match than the one being tracked.
    pub fn is_new_match(&self, s: &Snapshot) -> bool {
        match &self.prev {
            None => false,
            Some(p) => s.lineup_key() != self.key || s.volume() < p.volume(),
        }
    }

    /// Feeds one reading. Call every ~2 seconds.
    pub fn update(&mut self, s: Snapshot, now: Instant) {
        if self.prev.is_some() && self.is_new_match(&s) {
            *self = Self::new();
        }
        self.now = now;
        self.tick += 1;
        let first = self.prev.is_none();
        if first {
            self.key = s.lineup_key();
        }
        let period = s.period();
        if self.period_seen[period].is_none() {
            self.period_seen[period] = Some(now);
            if period > 0
                && period < 6
                && let Some(prev_start) = self.period_seen[period - 1]
            {
                let d = now - prev_start;
                if d > Duration::from_secs(20) {
                    self.period_lens.push(d);
                }
            }
        }
        let prev = self.prev.take();
        self.diff(prev.as_ref(), &s, first);
        self.resolve_pending(&s);
        self.apply_lineups(&s, first);
        self.prev = Some(s);
    }

    /// Estimated match minute for an event in `period` (now if it is the current one).
    pub fn minute_in(&self, period: usize, current: usize) -> u32 {
        let mut a = 0.5;
        if period == current
            && let Some(start) = self.period_seen[period]
        {
            // Until a full period has been seen, assume the game's default match length (10 min => 100 s per period).
            let len = self.period_len().unwrap_or(DEFAULT_PERIOD);
            a = ((self.now - start).as_secs_f64() / len.as_secs_f64()).clamp(0.0, 0.99);
        }
        minute_from(period, a)
    }

    fn period_len(&self) -> Option<Duration> {
        let mut v = self.period_lens.clone();
        v.sort();
        v.get(v.len() / 2).copied()
    }

    fn push(
        &mut self,
        minute: u32,
        side: usize,
        kind: EventKind,
        slot: usize,
        other: Option<usize>,
    ) -> u32 {
        self.seq += 1;
        let key = kind.key();
        self.events.push(Event {
            seq: self.seq,
            minute,
            side,
            kind,
            slot,
            other,
            detail: 0,
            key,
            text: String::new(),
        });
        self.seq
    }

    fn diff(&mut self, prev: Option<&Snapshot>, s: &Snapshot, first: bool) {
        let cur = s.period();
        if let Some(p) = prev {
            for side in 0..2 {
                if s.side_total(side, kind::RUN) > p.side_total(side, kind::RUN) {
                    self.moving[side] += 1;
                }
            }
        }
        for side in 0..2 {
            for slot in 0..SLOTS {
                let Some(p) = &s.sides[side][slot] else {
                    continue;
                };
                let old = prev.and_then(|x| x.sides[side][slot].as_ref());
                let run = p.total(kind::RUN);
                let st = &mut self.players[side][slot];
                if run > st.last_run || (first && run > 0) {
                    st.last_growth = self.tick;
                }
                st.last_run = run;
                let became_active = p.active() && !st.on && !st.went_off;
                if slot < STARTERS && !st.on && !st.went_off && (p.active() || first) {
                    st.on = true;
                } else if slot >= STARTERS && became_active {
                    st.on = true;
                    st.came_on = true;
                    if !first {
                        let minute = self.minute_in(cur, cur);
                        let seq = self.push(minute, side, EventKind::Sub, slot, None);
                        let moving_at = self.moving[side];
                        self.pending.push(PendingSub {
                            moving_at,
                            side,
                            slot,
                            tick: self.tick,
                            minute,
                            seq,
                        });
                    }
                }
                // Per-period increments: goals, own goals, cards.
                for per in 0..PERIODS {
                    let d = |k: usize| {
                        p.get(k, per)
                            .saturating_sub(old.map_or(0, |o| o.get(k, per)))
                    };
                    let minute = self.minute_in(per, cur);
                    for _ in 0..d(kind::GOAL) {
                        let assist = (0..SLOTS).find(|&j| {
                            j != slot
                                && s.sides[side][j].as_ref().is_some_and(|q| {
                                    q.get(kind::ASSIST, per)
                                        > prev
                                            .and_then(|x| x.sides[side][j].as_ref())
                                            .map_or(0, |o| o.get(kind::ASSIST, per))
                                })
                        });
                        self.push(minute, side, EventKind::Goal, slot, assist);
                    }
                    for _ in 0..d(kind::OWN_GOAL) {
                        self.push(minute, side, EventKind::OwnGoal, slot, None);
                    }
                    let reds = d(kind::RED);
                    let yellows = d(kind::YELLOW);
                    let total_y = p.total(kind::YELLOW);
                    for i in 0..yellows {
                        let second = reds > 0 && total_y >= 2 && i == yellows - 1;
                        self.push(
                            minute,
                            side,
                            if second {
                                EventKind::SecondYellow
                            } else {
                                EventKind::Yellow
                            },
                            slot,
                            None,
                        );
                    }
                    let straight = if yellows > 0 && total_y >= 2 {
                        reds.saturating_sub(1)
                    } else {
                        reds
                    };
                    for _ in 0..straight {
                        self.push(minute, side, EventKind::Red, slot, None);
                    }
                    if reds > 0 {
                        let st = &mut self.players[side][slot];
                        st.sent_off = true;
                        st.on = false;
                    }
                    // Penalty and free-kick goals: mark the goal events just pushed.
                    let (pg, fg) = (d(kind::PENALTY_GOAL), d(kind::FREE_KICK_GOAL));
                    if pg + fg > 0 {
                        let mut left = (pg, fg);
                        for e in self.events.iter_mut().rev().take(d(kind::GOAL) as usize) {
                            if e.kind == EventKind::Goal && e.side == side && e.slot == slot {
                                if left.0 > 0 {
                                    e.detail = 1;
                                    left.0 -= 1;
                                } else if left.1 > 0 {
                                    e.detail = 2;
                                    left.1 -= 1;
                                }
                            }
                        }
                    }
                    if first {
                        continue; // Don't replay the whole match's shots and corners on start-up.
                    }
                    let goals = d(kind::GOAL);
                    let missed_pen = d(kind::PENALTY_SHOT).saturating_sub(d(kind::PENALTY_GOAL));
                    let on = d(kind::SHOT_ON_TARGET).saturating_sub(goals);
                    let wide = d(kind::SHOT).saturating_sub(d(kind::SHOT_ON_TARGET));
                    let (wide, on) = {
                        let from_wide = missed_pen.min(wide);
                        (wide - from_wide, on.saturating_sub(missed_pen - from_wide))
                    };
                    for _ in 0..missed_pen {
                        self.push(minute, side, EventKind::PenaltyMissed, slot, None);
                    }
                    for _ in 0..on {
                        let keeper = (0..SLOTS).find(|&j| {
                            s.sides[1 - side][j].as_ref().is_some_and(|q| {
                                q.get(kind::SAVE, per)
                                    > prev
                                        .and_then(|x| x.sides[1 - side][j].as_ref())
                                        .map_or(0, |o| o.get(kind::SAVE, per))
                            })
                        });
                        self.push(minute, side, EventKind::Chance, slot, keeper);
                    }
                    for _ in 0..wide {
                        self.push(minute, side, EventKind::ShotWide, slot, None);
                    }
                    for _ in 0..d(kind::CORNER) {
                        self.push(minute, side, EventKind::Corner, slot, None);
                    }
                    for _ in 0..d(kind::FREE_KICK) {
                        self.push(minute, side, EventKind::FreeKick, slot, None);
                    }
                    for _ in 0..d(kind::OFFSIDE) {
                        self.push(minute, side, EventKind::Offside, slot, None);
                    }
                }
            }
        }
        self.events.sort_by_key(|e| (e.minute, e.seq));
    }

    /// A sub's "off" player is the one on the pitch whose run counter stopped when the sub came on,
    /// while the rest of the team kept running.
    fn resolve_pending(&mut self, s: &Snapshot) {
        let mut done = vec![];
        for (i, ps) in self.pending.iter().enumerate() {
            // Only count ticks in which the game was running; a paused game tells us nothing.
            let ran = self.moving[ps.side] - ps.moving_at;
            if ran < 3 {
                continue;
            }
            let cands: Vec<usize> = (0..SLOTS)
                .filter(|&j| {
                    let st = &self.players[ps.side][j];
                    j != ps.slot
                        && st.on
                        && !st.went_off
                        && !st.sent_off
                        && s.sides[ps.side][j].is_some()
                        && st.last_growth <= ps.tick
                })
                .collect();
            if cands.len() == 1 {
                done.push((i, Some(cands[0])));
            } else if ran > 30 {
                done.push((i, None));
            }
        }
        for (i, off) in done.iter().rev() {
            let ps = self.pending.remove(*i);
            if let Some(off) = off {
                let st = &mut self.players[ps.side][*off];
                st.on = false;
                st.went_off = true;
            }
            if let Some(e) = self.events.iter_mut().find(|e| e.seq == ps.seq) {
                e.other = *off;
                e.minute = ps.minute;
            }
        }
    }

    /// Sets the line-up read from memory for the next [`update`](Self::update).
    pub fn set_lineups(&mut self, l: Option<Lineups>) {
        self.lineups = l;
    }

    /// The real line-up beats the run-counter guesswork: who is on the pitch, and exact sub pairs.
    fn apply_lineups(&mut self, s: &Snapshot, first: bool) {
        let Some(l) = self.lineups.clone() else {
            return;
        };
        let cur = s.period();
        for side in 0..2 {
            for slot in 0..SLOTS {
                if s.sides[side][slot].is_none() {
                    continue;
                }
                let on = l.xi[side].contains(&(slot as u32));
                let st = &mut self.players[side][slot];
                if st.sent_off {
                    st.on = false;
                    continue;
                }
                if st.on && !on {
                    st.went_off = true;
                }
                if on && slot >= STARTERS {
                    st.came_on = true;
                }
                st.on = on;
            }
            for e in l.queue[side].iter().filter(|e| e.status == STATUS_DONE) {
                let (i, o) = (e.in_slot as usize, e.out_slot as usize);
                if i >= SLOTS || o >= SLOTS {
                    continue;
                }
                self.players[side][o].went_off = true;
                self.players[side][o].on = false;
                self.players[side][i].came_on = true;
                self.pending.retain(|p| !(p.side == side && p.slot == i));
                if let Some(ev) = self
                    .events
                    .iter_mut()
                    .find(|ev| ev.kind == EventKind::Sub && ev.side == side && ev.slot == i)
                {
                    ev.other = Some(o);
                } else {
                    let minute = if first { 0 } else { self.minute_in(cur, cur) };
                    self.push(minute, side, EventKind::Sub, i, Some(o));
                }
            }
        }
        self.events.sort_by_key(|e| (e.minute, e.seq));
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.prev.as_ref()
    }

    pub fn last_seq(&self) -> u32 {
        self.seq
    }

    pub fn view(&self, db: Option<&Database>) -> Option<MatchView> {
        let s = self.prev.as_ref()?;
        let cur = s.period();
        let score = s.score();
        let teams: [Option<u32>; 2] = [0, 1].map(|side| {
            db.and_then(|d| {
                d.team_of(
                    &s.sides[side][..STARTERS]
                        .iter()
                        .flatten()
                        .map(|p| p.id)
                        .collect::<Vec<_>>(),
                )
            })
        });
        let mut players = vec![];
        for (side, slot, p) in s.players() {
            let st = &self.players[side][slot];
            let info = db.and_then(|d| d.players.get(&p.id));
            let number = db.and_then(|d| d.shirt_number(p.id, teams[side]));
            let played = p.active() || st.on;
            players.push(PlayerView {
                side,
                slot,
                id: p.id,
                name: info
                    .map(|i| {
                        if i.shirt_name.is_empty() {
                            i.name.clone()
                        } else {
                            title_case(&i.shirt_name)
                        }
                    })
                    .unwrap_or_else(|| format!("#{}", number.map_or(slot as u32 + 1, |n| n))),
                full_name: info.map(|i| i.name.clone()).unwrap_or_default(),
                number,
                position: info.and_then(|i| i.position),
                starter: slot < STARTERS,
                on_pitch: st.on,
                came_on: st.came_on,
                went_off: st.went_off,
                sent_off: st.sent_off,
                rating: played.then(|| rating(p)),
                goals: p.total(kind::GOAL),
                assists: p.total(kind::ASSIST),
                shots: p.total(kind::SHOT),
                on_target: p.total(kind::SHOT_ON_TARGET),
                passes: p.total(kind::PASS),
                passes_ok: p.total(kind::PASS_OK),
                touches: p.total(kind::TOUCH),
                duels_won: p.total(kind::DUEL_WON),
                interceptions: p.total(kind::INTERCEPTION),
                fouls: p.total(kind::FOUL),
                yellow: p.total(kind::YELLOW),
                red: p.total(kind::RED),
                saves: p.total(kind::SAVE),
            });
        }
        let name_of = |side: usize, slot: usize| {
            players
                .iter()
                .find(|p| p.side == side && p.slot == slot)
                .map_or("?".to_string(), |p| p.name.clone())
        };
        let events = self
            .events
            .iter()
            .map(|e| {
                let mut e = e.clone();
                let n = name_of(e.side, e.slot);
                e.text = match e.kind {
                    EventKind::Goal => {
                        let how = match e.detail {
                            1 => " from the penalty spot",
                            2 => " from a free kick",
                            _ => "",
                        };
                        match e.other {
                            Some(a) if e.detail != 1 => {
                                format!("Goal! {n} scores{how}, assist {}.", name_of(e.side, a))
                            }
                            _ => format!("Goal! {n} scores{how}."),
                        }
                    }
                    EventKind::PenaltyMissed => {
                        format!("Penalty missed! {n} doesn't score from the spot.")
                    }
                    EventKind::Chance => match e.other {
                        Some(k) => format!("Chance! {n} shoots, {} saves.", name_of(1 - e.side, k)),
                        None => format!("Chance! {n} gets a shot on target."),
                    },
                    EventKind::ShotWide => format!("{n} shoots wide."),
                    EventKind::Corner => format!("Corner, taken by {n}."),
                    EventKind::FreeKick => format!("Free kick, taken by {n}."),
                    EventKind::Offside => format!("{n} is caught offside."),
                    EventKind::OwnGoal => format!("Own goal by {n}."),
                    EventKind::Yellow => format!("Yellow card for {n}."),
                    EventKind::SecondYellow => format!("Second yellow, {n} is sent off."),
                    EventKind::Red => format!("Red card! {n} is sent off."),
                    EventKind::Sub => match e.other {
                        Some(o) => format!("Substitution: {n} on, {} off.", name_of(e.side, o)),
                        None => format!("Substitution: {n} comes on."),
                    },
                };
                e
            })
            .collect();
        let row = |label: &str, k: usize| StatRow {
            label: label.into(),
            home: s.side_total(0, k),
            away: s.side_total(1, k),
        };
        let pct = |side: usize| {
            let a = s.side_total(side, kind::PASS);
            if a == 0 {
                0
            } else {
                (s.side_total(side, kind::PASS_OK) * 100 + a / 2) / a
            }
        };
        let flow = (0..=cur.max(5).min(PERIODS - 1))
            .map(|per| {
                let t = |side: usize, k: usize| {
                    s.sides[side]
                        .iter()
                        .flatten()
                        .map(|p| p.get(k, per))
                        .sum::<u32>()
                };
                FlowRow {
                    period: per,
                    label: PERIOD_LABELS[per].into(),
                    shots: [t(0, kind::SHOT), t(1, kind::SHOT)],
                    touches: [t(0, kind::TOUCH), t(1, kind::TOUCH)],
                }
            })
            .filter(|f| f.period <= cur || f.period < 6)
            .collect();
        let touch_now = [0, 1].map(|side| {
            s.sides[side]
                .iter()
                .flatten()
                .map(|p| p.get(kind::TOUCH, cur))
                .sum::<u32>()
        });
        let team_name = |side: usize| {
            teams[side]
                .and_then(|t| db.and_then(|d| d.teams.get(&t).cloned()))
                .unwrap_or_else(|| {
                    if side == 0 {
                        "Home".into()
                    } else {
                        "Away".into()
                    }
                })
        };
        Some(MatchView {
            home: team_name(0),
            away: team_name(1),
            score,
            period: cur,
            minute: self.minute_in(cur, cur),
            period_label: PERIOD_LABELS[cur].into(),
            minute_tracked: self.period_len().is_some(),
            touch_share: if touch_now[0] + touch_now[1] == 0 {
                50
            } else {
                touch_now[0] * 100 / (touch_now[0] + touch_now[1])
            },
            team_stats: vec![
                row("Shots", kind::SHOT),
                row("On target", kind::SHOT_ON_TARGET),
                row("Passes", kind::PASS),
                StatRow {
                    label: "Pass accuracy %".into(),
                    home: pct(0),
                    away: pct(1),
                },
                row("Crosses", kind::CROSS),
                row("Duels won", kind::DUEL_WON),
                row("Interceptions", kind::INTERCEPTION),
                row("Corners", kind::CORNER),
                row("Free kicks", kind::FREE_KICK),
                row("Fouls", kind::FOUL),
                row("Offsides", kind::OFFSIDE),
                row("Yellow cards", kind::YELLOW),
                row("Red cards", kind::RED),
                row("Saves", kind::SAVE),
            ],
            xg: [0, 1].map(|side| xg(s, side)),
            players,
            events,
            flow,
            last_seq: self.seq,
            subs: SubsInfo {
                available: self.can_sub && self.lineups.is_some(),
                limit: self.lineups.as_ref().map_or([5, 5], |l| l.limit),
                from_memory: self.lineups.is_some(),
            },
            subs_pending: self
                .lineups
                .iter()
                .flat_map(|l| {
                    (0..2).flat_map(move |side| {
                        l.queue[side]
                            .iter()
                            .filter(|e| e.status == STATUS_REQUESTED)
                            .map(move |e| PendingView {
                                side,
                                in_slot: e.in_slot,
                                out_slot: e.out_slot,
                            })
                    })
                })
                .collect(),
        })
    }
}

const DEFAULT_PERIOD: Duration = Duration::from_secs(100);

pub const PERIOD_LABELS: [&str; PERIODS] = [
    "0–15", "15–30", "30–45", "45–60", "60–75", "75–90", "90+", "ET 1", "ET 2",
];

/// Minute from period index and share of the period already played (0..1).
pub fn minute_from(period: usize, a: f64) -> u32 {
    match period {
        p if p < 6 => 15 * p as u32 + (15.0 * a) as u32 + 1,
        6 => 90 + 1 + (4.0 * a) as u32,
        p => 90 + 15 * (p as u32 - 7) + (15.0 * a) as u32 + 1,
    }
}

fn xg(s: &Snapshot, side: usize) -> f32 {
    let pens = s.side_total(side, kind::PENALTY_SHOT) as f32;
    let on = s.side_total(side, kind::SHOT_ON_TARGET) as f32 - pens;
    let off = s.side_total(side, kind::SHOT) as f32 - on.max(0.0) - pens;
    let v = 0.76 * pens + 0.12 * on.max(0.0) + 0.04 * off.max(0.0);
    (v * 100.0).round() / 100.0
}

/// Simple match rating (6.0 baseline), Sofascore-style scale 3.0..10.0.
pub fn rating(p: &crate::stats::Slot) -> f32 {
    let t = |k| p.total(k) as f32;
    let r = 6.0
        + 1.0 * t(kind::GOAL)
        + 0.6 * t(kind::ASSIST)
        + 0.12 * t(kind::SHOT_ON_TARGET)
        + 0.004 * t(kind::PASS_OK)
        + 0.06 * t(kind::DUEL_WON)
        + 0.05 * t(kind::INTERCEPTION)
        + 0.3 * t(kind::SAVE)
        - 0.3 * t(kind::YELLOW)
        - 1.5 * t(kind::RED)
        - 0.1 * t(kind::FOUL)
        - 0.8 * t(kind::OWN_GOAL);
    (r.clamp(3.0, 10.0) * 10.0).round() / 10.0
}

/// "B.FERNANDES" -> "B.Fernandes", "DE LIGT" -> "De Ligt", "O'NEIL" -> "O'Neil".
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for ch in s.chars() {
        if start {
            out.extend(ch.to_uppercase());
        } else {
            out.extend(ch.to_lowercase());
        }
        start = matches!(ch, ' ' | '.' | '-' | '\'');
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerView {
    pub side: usize,
    pub slot: usize,
    pub id: u32,
    pub name: String,
    pub full_name: String,
    pub number: Option<u32>,
    pub position: Option<&'static str>,
    pub starter: bool,
    pub on_pitch: bool,
    pub came_on: bool,
    pub went_off: bool,
    pub sent_off: bool,
    pub rating: Option<f32>,
    pub goals: u32,
    pub assists: u32,
    pub shots: u32,
    pub on_target: u32,
    pub passes: u32,
    pub passes_ok: u32,
    pub touches: u32,
    pub duels_won: u32,
    pub interceptions: u32,
    pub fouls: u32,
    pub yellow: u32,
    pub red: u32,
    pub saves: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatRow {
    pub label: String,
    pub home: u32,
    pub away: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowRow {
    pub period: usize,
    pub label: String,
    pub shots: [u32; 2],
    pub touches: [u32; 2],
}

#[derive(Debug, Clone, Serialize)]
pub struct MatchView {
    pub home: String,
    pub away: String,
    pub score: [u32; 2],
    pub period: usize,
    pub minute: u32,
    pub period_label: String,
    pub minute_tracked: bool,
    /// Share of ball touches in the current period (0-100). Not possession.
    pub touch_share: u32,
    pub team_stats: Vec<StatRow>,
    pub xg: [f32; 2],
    pub players: Vec<PlayerView>,
    pub events: Vec<Event>,
    pub flow: Vec<FlowRow>,
    pub last_seq: u32,
    pub subs: SubsInfo,
    /// Substitutions requested but not yet made by the game.
    pub subs_pending: Vec<PendingView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubsInfo {
    pub available: bool,
    pub limit: [u8; 2],
    /// True when the line-up comes from game memory (not guessed).
    pub from_memory: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingView {
    pub side: usize,
    pub in_slot: u32,
    pub out_slot: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{KINDS, Slot};

    fn snap() -> Snapshot {
        let mk = |side: u32, n: usize| {
            (0..SLOTS)
                .map(|i| {
                    (i < n).then(|| Slot {
                        id: side * 1000 + i as u32,
                        vals: vec![0; KINDS * PERIODS],
                    })
                })
                .collect::<Vec<_>>()
        };
        Snapshot {
            sides: [mk(1, 18), mk(2, 18)],
        }
    }
    fn add(s: &mut Snapshot, side: usize, slot: usize, k: usize, per: usize, v: u32) {
        s.sides[side][slot].as_mut().unwrap().vals[k * PERIODS + per] += v;
    }
    fn run_all(s: &mut Snapshot, per: usize, except: &[(usize, usize)]) {
        for side in 0..2 {
            for slot in 0..SLOTS {
                if except.contains(&(side, slot)) || s.sides[side][slot].is_none() {
                    continue;
                }
                let st = s.sides[side][slot].as_ref().unwrap();
                if slot < STARTERS || st.total(kind::RUN) > 0 {
                    add(s, side, slot, kind::RUN, per, 10);
                    add(s, side, slot, kind::TOUCH, per, 1);
                }
            }
        }
    }

    #[test]
    fn goal_with_assist_and_score() {
        let mut t = Tracker::new();
        let mut s = snap();
        let now = Instant::now();
        run_all(&mut s, 0, &[]);
        t.update(s.clone(), now);
        add(&mut s, 0, 10, kind::GOAL, 1, 1);
        add(&mut s, 0, 7, kind::ASSIST, 1, 1);
        run_all(&mut s, 1, &[]);
        t.update(s.clone(), now + Duration::from_secs(2));
        let v = t.view(None).unwrap();
        assert_eq!(v.score, [1, 0]);
        assert_eq!(v.events.len(), 1);
        assert_eq!(v.events[0].kind, EventKind::Goal);
        assert_eq!(v.events[0].other, Some(7));
        assert!((16..=30).contains(&v.events[0].minute));
    }

    #[test]
    fn substitution_finds_player_off() {
        let mut t = Tracker::new();
        let mut s = snap();
        let mut now = Instant::now();
        run_all(&mut s, 3, &[]);
        t.update(s.clone(), now);
        // bench slot 14 comes on, starter slot 6 stops running
        add(&mut s, 0, 14, kind::RUN, 3, 5);
        for _ in 0..4 {
            run_all(&mut s, 3, &[(0, 6)]);
            now += Duration::from_secs(2);
            t.update(s.clone(), now);
        }
        let v = t.view(None).unwrap();
        let sub = v.events.iter().find(|e| e.kind == EventKind::Sub).unwrap();
        assert_eq!(sub.slot, 14);
        assert_eq!(sub.other, Some(6));
        let p6 = v
            .players
            .iter()
            .find(|p| p.side == 0 && p.slot == 6)
            .unwrap();
        assert!(p6.went_off && !p6.on_pitch);
    }

    #[test]
    fn second_yellow_and_new_match_reset() {
        let mut t = Tracker::new();
        let mut s = snap();
        let now = Instant::now();
        run_all(&mut s, 0, &[]);
        add(&mut s, 1, 4, kind::YELLOW, 0, 1);
        t.update(s.clone(), now);
        add(&mut s, 1, 4, kind::YELLOW, 2, 1);
        add(&mut s, 1, 4, kind::RED, 2, 1);
        t.update(s.clone(), now);
        let kinds: Vec<_> = t.events.iter().map(|e| e.kind.clone()).collect();
        assert_eq!(kinds, vec![EventKind::Yellow, EventKind::SecondYellow]);
        // New match: different lineup
        let mut s2 = snap();
        s2.sides[0][0].as_mut().unwrap().id = 99;
        t.update(s2, now);
        assert!(t.events.is_empty());
    }

    #[test]
    fn sub_resolved_after_pause() {
        let mut t = Tracker::new();
        let mut s = snap();
        let mut now = Instant::now();
        run_all(&mut s, 3, &[]);
        t.update(s.clone(), now);
        // Sub comes on while the game is paused for 20 ticks.
        add(&mut s, 0, 14, kind::RUN, 3, 5);
        for _ in 0..20 {
            now += Duration::from_secs(2);
            t.update(s.clone(), now);
        }
        for _ in 0..4 {
            run_all(&mut s, 3, &[(0, 6)]);
            now += Duration::from_secs(2);
            t.update(s.clone(), now);
        }
        let v = t.view(None).unwrap();
        assert_eq!(
            v.events
                .iter()
                .find(|e| e.kind == EventKind::Sub)
                .unwrap()
                .other,
            Some(6)
        );
        assert!(
            !v.players
                .iter()
                .find(|p| p.side == 0 && p.slot == 6)
                .unwrap()
                .on_pitch
        );
    }

    #[test]
    fn ticker_chances_corners_penalties() {
        let mut t = Tracker::new();
        let mut s = snap();
        let now = Instant::now();
        run_all(&mut s, 0, &[]);
        t.update(s.clone(), now);
        // Saved shot by home 9, keeper away 0 saves.
        add(&mut s, 0, 9, kind::SHOT, 1, 1);
        add(&mut s, 0, 9, kind::SHOT_ON_TARGET, 1, 1);
        add(&mut s, 1, 0, kind::SAVE, 1, 1);
        add(&mut s, 0, 7, kind::CORNER, 1, 1);
        // Away 10: penalty scored; away 9: penalty missed (wide).
        add(&mut s, 1, 10, kind::SHOT, 1, 1);
        add(&mut s, 1, 10, kind::SHOT_ON_TARGET, 1, 1);
        add(&mut s, 1, 10, kind::PENALTY_SHOT, 1, 1);
        add(&mut s, 1, 10, kind::GOAL, 1, 1);
        add(&mut s, 1, 10, kind::PENALTY_GOAL, 1, 1);
        add(&mut s, 1, 9, kind::SHOT, 1, 1);
        add(&mut s, 1, 9, kind::PENALTY_SHOT, 1, 1);
        run_all(&mut s, 1, &[]);
        t.update(s.clone(), now);
        let v = t.view(None).unwrap();
        let k: Vec<_> = v
            .events
            .iter()
            .map(|e| (e.kind.clone(), e.side, e.slot, e.detail, e.key))
            .collect();
        assert!(k.contains(&(EventKind::Chance, 0, 9, 0, true)), "{k:?}");
        assert!(k.contains(&(EventKind::Corner, 0, 7, 0, false)));
        assert!(k.contains(&(EventKind::Goal, 1, 10, 1, true)));
        assert!(k.contains(&(EventKind::PenaltyMissed, 1, 9, 0, true)));
        assert!(
            !k.iter().any(|e| e.0 == EventKind::ShotWide),
            "missed penalty must not also be a wide shot"
        );
        let chance = v
            .events
            .iter()
            .find(|e| e.kind == EventKind::Chance)
            .unwrap();
        assert_eq!(chance.other, Some(0));
        assert!(
            v.events
                .iter()
                .any(|e| e.text.contains("from the penalty spot"))
        );
    }

    #[test]
    fn lineup_from_memory_wins() {
        use crate::lineup::{Lineups, QueueEntry};
        let free = QueueEntry {
            in_slot: 0xff,
            out_slot: 0xff,
            status: 0,
        };
        let mut t = Tracker::new();
        let mut s = snap();
        let now = Instant::now();
        run_all(&mut s, 3, &[]);
        // Started mid-match: slot 14 already played, slot 6 went off. Game paused.
        add(&mut s, 0, 14, kind::RUN, 3, 50);
        let mut xi: Vec<u32> = (0..11).collect();
        xi[6] = 14;
        let mut q = vec![free; 6];
        q[0] = QueueEntry {
            in_slot: 14,
            out_slot: 6,
            status: STATUS_DONE,
        };
        q[1] = QueueEntry {
            in_slot: 15,
            out_slot: 2,
            status: STATUS_REQUESTED,
        };
        t.set_lineups(Some(Lineups {
            xi: [xi, (0..11).collect()],
            queue: [q, vec![free; 6]],
            limit: [5, 5],
        }));
        t.can_sub = true;
        t.update(s.clone(), now);
        let v = t.view(None).unwrap();
        let on: Vec<usize> = v
            .players
            .iter()
            .filter(|p| p.side == 0 && p.on_pitch)
            .map(|p| p.slot)
            .collect();
        assert_eq!(on.len(), 11);
        assert!(on.contains(&14) && !on.contains(&6));
        let sub = v.events.iter().find(|e| e.kind == EventKind::Sub).unwrap();
        assert_eq!((sub.slot, sub.other), (14, Some(6)));
        assert_eq!(v.subs_pending.len(), 1);
        assert!(v.subs.available && v.subs.from_memory);
    }

    #[test]
    fn shirt_names() {
        assert_eq!(title_case("B.FERNANDES"), "B.Fernandes");
        assert_eq!(title_case("DE LIGT"), "De Ligt");
        assert_eq!(title_case("ŠEŠKO"), "Šeško");
        assert_eq!(title_case("MAC ALLISTER"), "Mac Allister");
    }

    #[test]
    fn minutes() {
        assert_eq!(minute_from(0, 0.0), 1);
        assert_eq!(minute_from(3, 0.5), 53);
        assert_eq!(minute_from(6, 0.5), 93);
        assert_eq!(minute_from(7, 0.0), 91);
    }
}
