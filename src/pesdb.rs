//! Player and team names from the game database (pesdb), read-only.
//!
//! The game memory only holds player ids. Names, shirt numbers and positions come from
//! `Player.bin`, `Team.bin` and `PlayerAssignment.bin` (WESYS containers: 16-byte header,
//! then a zlib stream).

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

const PLAYER_REC: usize = 312;
const TEAM_REC: usize = 1532;
const ASSIGN_REC: usize = 16;

pub const POSITIONS: [&str; 13] = [
    "GK", "CB", "LB", "RB", "DMF", "CMF", "LMF", "RMF", "AMF", "LWF", "RWF", "SS", "CF",
];

#[derive(Debug, Clone, Default)]
pub struct PlayerInfo {
    pub name: String,
    pub shirt_name: String,
    pub position: Option<&'static str>,
}

#[derive(Debug, Default)]
pub struct Database {
    pub players: HashMap<u32, PlayerInfo>,
    pub teams: HashMap<u32, String>,
    /// player id -> list of (team id, shirt number)
    pub assignments: HashMap<u32, Vec<(u32, u32)>>,
    pub folder: PathBuf,
}

pub fn unpack(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() < 16 || &raw[3..8] != b"WESYS" {
        bail!("not a WESYS container");
    }
    let mut out = vec![];
    flate2::read::ZlibDecoder::new(&raw[16..])
        .read_to_end(&mut out)
        .context("zlib")?;
    Ok(out)
}

fn cstr(b: &[u8]) -> String {
    let n = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).trim().to_string()
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn bits(rec: &[u8], bit: usize, width: u32) -> u32 {
    let byte = bit >> 3;
    let mut w = [0u8; 4];
    let end = (byte + 4).min(rec.len());
    w[..end - byte].copy_from_slice(&rec[byte..end]);
    (u32::from_le_bytes(w) >> (bit & 7)) & ((1 << width) - 1)
}

pub fn parse_players(data: &[u8]) -> HashMap<u32, PlayerInfo> {
    data.chunks_exact(PLAYER_REC)
        .filter_map(|r| {
            let id = u32_at(r, 8);
            (id != 0).then(|| {
                let pos = bits(r, 434, 4) as usize;
                (
                    id,
                    PlayerInfo {
                        name: cstr(&r[68..129]),
                        shirt_name: cstr(&r[129..190]),
                        position: POSITIONS.get(pos).copied(),
                    },
                )
            })
        })
        .collect()
}

pub fn parse_teams(data: &[u8]) -> HashMap<u32, String> {
    data.chunks_exact(TEAM_REC)
        .filter_map(|r| {
            let id = u32_at(r, 8);
            let mut name = cstr(&r[368..438]);
            if name.is_empty() {
                name = cstr(&r[158..228]);
            }
            (id != 0 && !name.is_empty()).then_some((id, name))
        })
        .collect()
}

pub fn parse_assignments(data: &[u8]) -> HashMap<u32, Vec<(u32, u32)>> {
    let mut m: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    for r in data.chunks_exact(ASSIGN_REC) {
        let (player, team, field) = (u32_at(r, 4), u32_at(r, 8), u32_at(r, 12));
        if player != 0 {
            m.entry(player)
                .or_default()
                .push((team, (field & 0x7f) + 1));
        }
    }
    m
}

impl Database {
    /// Loads a pesdb folder (the one containing Player.bin).
    pub fn load(folder: &Path) -> Result<Self> {
        let read = |n: &str| -> Result<Vec<u8>> {
            let p = folder.join(n);
            unpack(&std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?)
        };
        Ok(Self {
            players: parse_players(&read("Player.bin")?),
            teams: read("Team.bin")
                .map(|d| parse_teams(&d))
                .unwrap_or_default(),
            assignments: read("PlayerAssignment.bin")
                .map(|d| parse_assignments(&d))
                .unwrap_or_default(),
            folder: folder.to_path_buf(),
        })
    }

    /// The team most of these players belong to (players also appear in national teams).
    pub fn team_of(&self, ids: &[u32]) -> Option<u32> {
        let mut votes: HashMap<u32, usize> = HashMap::new();
        for id in ids {
            for (t, _) in self.assignments.get(id).into_iter().flatten() {
                *votes.entry(*t).or_default() += 1;
            }
        }
        votes
            .into_iter()
            .max_by_key(|(t, n)| (*n, std::cmp::Reverse(*t)))
            .map(|(t, _)| t)
    }

    pub fn shirt_number(&self, player: u32, team: Option<u32>) -> Option<u32> {
        let a = self.assignments.get(&player)?;
        team.and_then(|t| a.iter().find(|(x, _)| *x == t))
            .or(a.first())
            .map(|(_, n)| *n)
    }
}

/// Finds the pesdb folder that the game actually uses: the first `cpk.root` line in
/// `sider.ini` whose folder contains `common/etc/pesdb/Player.bin`.
pub fn find_in_game_folder(game: &Path) -> Option<PathBuf> {
    let ini = std::fs::read_to_string(game.join("sider.ini")).ok()?;
    for line in ini.lines() {
        let l = line.trim();
        if l.starts_with(';') || l.starts_with('#') {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        if k.trim() != "cpk.root" {
            continue;
        }
        let v = v.trim().trim_matches('"').replace('\\', "/");
        let root = if Path::new(&v).is_absolute() || v.contains(':') {
            PathBuf::from(&v)
        } else {
            game.join(v.trim_start_matches("./"))
        };
        let db = root.join("common/etc/pesdb");
        if db.join("Player.bin").is_file() {
            return Some(db);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    pub fn pack(data: &[u8]) -> Vec<u8> {
        let mut out = b"\0\0\0WESYS\0\0\0\0\0\0\0\0".to_vec();
        let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
        e.write_all(data).unwrap();
        out.extend(e.finish().unwrap());
        out
    }

    #[test]
    fn reads_players_teams_assignments() {
        let mut p = vec![0u8; PLAYER_REC * 2];
        p[8..12].copy_from_slice(&42u32.to_le_bytes());
        p[68..74].copy_from_slice(b"Novak\0");
        // position CF (12) at bit 434: byte 54, shift 2
        p[54] |= 12 << 2;
        let mut t = vec![0u8; TEAM_REC];
        t[8..12].copy_from_slice(&396u32.to_le_bytes());
        t[368..378].copy_from_slice(b"Sunderland");
        let mut a = vec![0u8; ASSIGN_REC * 2];
        a[4..8].copy_from_slice(&42u32.to_le_bytes());
        a[8..12].copy_from_slice(&396u32.to_le_bytes());
        a[12..16].copy_from_slice(&(0x2C00u32 | 8).to_le_bytes());
        a[20..24].copy_from_slice(&42u32.to_le_bytes());
        a[24..28].copy_from_slice(&50u32.to_le_bytes());

        let dir = std::env::temp_dir().join(format!("cz-pesdb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Player.bin"), pack(&p)).unwrap();
        std::fs::write(dir.join("Team.bin"), pack(&t)).unwrap();
        std::fs::write(dir.join("PlayerAssignment.bin"), pack(&a)).unwrap();
        let db = Database::load(&dir).unwrap();
        assert_eq!(db.players[&42].name, "Novak");
        assert_eq!(db.players[&42].position, Some("CF"));
        assert_eq!(db.teams[&396], "Sunderland");
        assert_eq!(db.shirt_number(42, Some(396)), Some(9));
        assert_eq!(db.team_of(&[42]), Some(50)); // tie -> lower id
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_wesys() {
        assert!(unpack(b"hello world, not a container").is_err());
    }
}
