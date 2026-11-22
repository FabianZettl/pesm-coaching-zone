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
    /// team id -> kits (from the mod's `UniColor.bin`, next to the pesdb).
    pub kits: HashMap<u32, Vec<Kit>>,
    pub folder: PathBuf,
}

/// One kit's two main colours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Kit {
    /// 0 = home, 1 = away, 2 = third, 0x10 = goalkeeper
    pub kind: u16,
    pub color1: [u8; 3],
    pub color2: [u8; 3],
}

/// `UniColor.bin`: per team u32 id, u8 count, then `count` entries of
/// u16 kit kind, RGB colour 1, RGB colour 2.
pub fn parse_kits(data: &[u8]) -> HashMap<u32, Vec<Kit>> {
    let mut m = HashMap::new();
    let mut i = 0;
    while i + 5 <= data.len() {
        let id = u32_at(data, i);
        let n = data[i + 4] as usize;
        let end = i + 5 + n * 8;
        if n == 0 || n > 32 || end > data.len() {
            break;
        }
        let kits = (0..n)
            .map(|k| {
                let e = &data[i + 5 + k * 8..i + 13 + k * 8];
                Kit {
                    kind: u16::from_le_bytes([e[0], e[1]]),
                    color1: [e[2], e[3], e[4]],
                    color2: [e[5], e[6], e[7]],
                }
            })
            .collect();
        m.insert(id, kits);
        i = end;
    }
    m
}

fn pale(c: [u8; 3]) -> bool {
    let (mx, mn) = (*c.iter().max().unwrap(), *c.iter().min().unwrap());
    mn > 0xa0 && mx - mn < 0x30
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn dist(a: [u8; 3], b: [u8; 3]) -> u32 {
    (0..3)
        .map(|i| (a[i] as i32 - b[i] as i32).unsigned_abs())
        .sum()
}

/// Where the database sits inside a CPK.
pub const PESDB_IN_CPK: &str = "common/etc/pesdb";
const KITS_IN_CPK: &str = "common/character0/model/character/uniform/team/UniColor.bin";

/// Unpacks a WESYS container, or returns the bytes as they are if they aren't one.
pub fn unpack_or_raw(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() >= 16 && &raw[3..8] == b"WESYS" {
        unpack(raw)
    } else {
        Ok(raw.to_vec())
    }
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
    /// Loads the database from a pesdb folder (the one containing Player.bin)
    /// or from a `.cpk` that contains `common/etc/pesdb/Player.bin`.
    pub fn load(source: &Path) -> Result<Self> {
        let is_cpk = source
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("cpk"));
        let (read, kits_raw): (Box<dyn Fn(&str) -> Result<Vec<u8>>>, Option<Vec<u8>>) = if is_cpk {
            let c = crate::cpk::Cpk::open(source)?;
            let kits = c.read(KITS_IN_CPK).ok();
            (
                Box::new(move |n: &str| c.read(&format!("{PESDB_IN_CPK}/{n}"))),
                kits,
            )
        } else {
            let folder = source.to_path_buf();
            let kits = source
                .parent()
                .and_then(Path::parent)
                .map(|common| common.join("character0/model/character/uniform/team/UniColor.bin"))
                .and_then(|p| std::fs::read(p).ok());
            (
                Box::new(move |n: &str| {
                    let p = folder.join(n);
                    std::fs::read(&p).with_context(|| format!("reading {}", p.display()))
                }),
                kits,
            )
        };
        let get = |n: &str| read(n).and_then(|raw| unpack_or_raw(&raw));
        Ok(Self {
            players: parse_players(&get("Player.bin")?),
            teams: get("Team.bin").map(|d| parse_teams(&d)).unwrap_or_default(),
            assignments: get("PlayerAssignment.bin")
                .map(|d| parse_assignments(&d))
                .unwrap_or_default(),
            kits: kits_raw
                .and_then(|raw| unpack_or_raw(&raw).ok())
                .map(|d| parse_kits(&d))
                .unwrap_or_default(),
            folder: source.to_path_buf(),
        })
    }

    /// A colour for each side, from the teams' kits: the home kit's main colour
    /// (its second colour when the first is white or grey), and the away team's
    /// change kit when both would look alike.
    pub fn team_colors(&self, teams: [Option<u32>; 2]) -> [Option<String>; 2] {
        let pick = |t: Option<u32>, kind: u16| {
            let k = self.kits.get(&t?)?.iter().find(|k| k.kind == kind)?;
            Some(if pale(k.color1) && !pale(k.color2) {
                k.color2
            } else {
                k.color1
            })
        };
        let home = pick(teams[0], 0);
        let mut away = pick(teams[1], 0);
        if let (Some(h), Some(a)) = (home, away)
            && (dist(h, a) < 120 || (pale(h) && pale(a)))
        {
            away = pick(teams[1], 1).or(pick(teams[1], 2)).or(away);
        }
        [home.map(hex), away.map(hex)]
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
/// `sider.ini` may sit in the game folder or in `SiderAddons/` (FL27); `cpk.root` paths are
/// relative to the folder of the ini.
/// Finds the player database for a game folder: first an extracted one in a
/// sider `cpk.root` folder, then inside the `.cpk` files under the game folder
/// (and the sider roots). Among CPKs, the one with the biggest `Player.bin` wins,
/// since that is the most complete database.
pub fn find_in_game_folder(game: &Path) -> Option<PathBuf> {
    let inis = [
        game.join("sider.ini"),
        game.join("SiderAddons").join("sider.ini"),
    ];
    if let Some(db) = inis.iter().find_map(|ini| from_sider_ini(ini)) {
        return Some(db);
    }
    let mut dirs = vec![game.to_path_buf()];
    for ini in &inis {
        dirs.extend(sider_roots(ini));
    }
    let player = format!("{PESDB_IN_CPK}/Player.bin");
    let mut best: Option<(u64, PathBuf)> = None;
    let mut seen = std::collections::HashSet::new();
    for d in dirs {
        for p in crate::cpk::find_files(&d, 4) {
            if !seen.insert(p.clone()) {
                continue;
            }
            let Ok(c) = crate::cpk::Cpk::open(&p) else {
                continue;
            };
            if let Some(n) = c.size_of(&player)
                && best.as_ref().is_none_or(|(b, _)| n > *b)
            {
                best = Some((n, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Kit colours for a game folder, wherever they are: extracted in a sider
/// `cpk.root` folder, or inside a `.cpk` (the biggest `UniColor.bin` wins).
pub fn find_kits(game: &Path) -> HashMap<u32, Vec<Kit>> {
    let inis = [
        game.join("sider.ini"),
        game.join("SiderAddons").join("sider.ini"),
    ];
    let roots: Vec<PathBuf> = inis.iter().flat_map(|i| sider_roots(i)).collect();
    let parse = |raw: Vec<u8>| unpack_or_raw(&raw).ok().map(|d| parse_kits(&d));
    for r in &roots {
        if let Some(k) = std::fs::read(r.join(KITS_IN_CPK)).ok().and_then(parse)
            && !k.is_empty()
        {
            return k;
        }
    }
    let mut best: Option<(u64, crate::cpk::Cpk)> = None;
    let mut seen = std::collections::HashSet::new();
    for d in std::iter::once(game.to_path_buf()).chain(roots) {
        for p in crate::cpk::find_files(&d, 4) {
            if !seen.insert(p.clone()) {
                continue;
            }
            let Ok(c) = crate::cpk::Cpk::open(&p) else {
                continue;
            };
            if let Some(n) = c.size_of(KITS_IN_CPK)
                && best.as_ref().is_none_or(|(b, _)| n > *b)
            {
                best = Some((n, c));
            }
        }
    }
    best.and_then(|(_, c)| c.read(KITS_IN_CPK).ok())
        .and_then(parse)
        .unwrap_or_default()
}

/// The `cpk.root` folders named in a sider.ini.
pub(crate) fn sider_roots(ini_path: &Path) -> Vec<PathBuf> {
    let Ok(ini) = std::fs::read_to_string(ini_path) else {
        return vec![];
    };
    let Some(base) = ini_path.parent() else {
        return vec![];
    };
    ini.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with(';') && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim() == "cpk.root")
        .map(|(_, v)| {
            let v = v.trim().trim_matches('"').replace('\\', "/");
            if Path::new(&v).is_absolute() || v.contains(':') {
                PathBuf::from(&v)
            } else {
                base.join(v.trim_start_matches("./"))
            }
        })
        .collect()
}

fn from_sider_ini(ini_path: &Path) -> Option<PathBuf> {
    let ini = std::fs::read_to_string(ini_path).ok()?;
    let base = ini_path.parent()?;
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
            base.join(v.trim_start_matches("./"))
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
    fn finds_pesdb_via_sideraddons_ini() {
        let game = std::env::temp_dir().join(format!("cz-game-{}", std::process::id()));
        let db = game.join("SiderAddons/livecpk/ModB/common/etc/pesdb");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::create_dir_all(game.join("SiderAddons/livecpk/ModA")).unwrap();
        std::fs::write(db.join("Player.bin"), b"x").unwrap();
        std::fs::write(
            game.join("SiderAddons/sider.ini"),
            "; comment\ncpk.root = \".\\livecpk\\ModA\"\ncpk.root = \".\\livecpk\\ModB\"\n",
        )
        .unwrap();
        assert_eq!(find_in_game_folder(&game), Some(db.clone()));
        std::fs::remove_dir_all(&game).ok();
    }

    #[test]
    fn rejects_non_wesys() {
        assert!(unpack(b"hello world, not a container").is_err());
    }

    #[test]
    fn kits_and_team_colors() {
        let mut d = vec![];
        let mut team = |id: u32, kits: &[(u16, [u8; 3], [u8; 3])]| {
            d.extend(id.to_le_bytes());
            d.push(kits.len() as u8);
            for (k, a, b) in kits {
                d.extend(k.to_le_bytes());
                d.extend(a);
                d.extend(b);
            }
        };
        team(
            126,
            &[
                (0, [0xef, 0xcf, 0x10], [0x28, 0x28, 0x28]),
                (1, [0x28; 3], [0xef, 0xcf, 0x10]),
            ],
        );
        team(
            109,
            &[(0, [0xd6; 3], [0xd6; 3]), (1, [0x21, 0x48, 0x51], [0; 3])],
        );
        team(
            110,
            &[
                (0, [0xf0, 0xd0, 0x20], [0; 3]),
                (1, [0x10, 0x20, 0xa0], [0xff; 3]),
            ],
        );
        let kits = parse_kits(&d);
        assert_eq!(kits.len(), 3);
        let db = Database {
            kits,
            ..Default::default()
        };
        assert_eq!(
            db.team_colors([Some(126), Some(109)]),
            [Some("#efcf10".into()), Some("#d6d6d6".into())]
        );
        // Two yellow teams: the away side wears its change kit.
        assert_eq!(
            db.team_colors([Some(126), Some(110)])[1],
            Some("#1020a0".into())
        );
        assert_eq!(
            db.team_colors([None, Some(126)]),
            [None, Some("#efcf10".into())]
        );
    }

    #[test]
    fn loads_from_a_cpk_in_the_game_folder() {
        let mut p = vec![0u8; PLAYER_REC];
        p[8..12].copy_from_slice(&42u32.to_le_bytes());
        p[68..74].copy_from_slice(b"Novak\0");
        let small = vec![0u8; PLAYER_REC];
        let game = std::env::temp_dir().join(format!("cz-game-cpk-{}", std::process::id()));
        std::fs::create_dir_all(game.join("download")).unwrap();
        let sm = pack(&small);
        // A small database in one CPK, the full one in another: the bigger one wins.
        std::fs::write(
            game.join("download/dt80_a.cpk"),
            crate::cpk::tests::build(&[("common/etc/pesdb/Player.bin", &sm)], false),
        )
        .unwrap();
        let big = pack(&{
            let mut v = p.clone();
            v.extend(vec![0u8; PLAYER_REC]);
            v
        });
        std::fs::write(
            game.join("download/fl27_db.cpk"),
            crate::cpk::tests::build(&[("common/etc/pesdb/Player.bin", &big)], true),
        )
        .unwrap();
        std::fs::write(game.join("download/broken.cpk"), b"not a cpk").unwrap();
        let found = find_in_game_folder(&game).unwrap();
        assert!(found.ends_with("download/fl27_db.cpk"), "{found:?}");
        let db = Database::load(&found).unwrap();
        assert_eq!(db.players[&42].name, "Novak");
        std::fs::remove_dir_all(&game).ok();
    }

    #[test]
    fn finds_kits_in_another_cpk() {
        let game = std::env::temp_dir().join(format!("cz-kits-{}", std::process::id()));
        std::fs::create_dir_all(&game).unwrap();
        let mut k = 126u32.to_le_bytes().to_vec();
        k.push(1);
        k.extend([0, 0, 0xef, 0xcf, 0x10, 0x28, 0x28, 0x28]);
        std::fs::write(
            game.join("kits.cpk"),
            crate::cpk::tests::build(&[(KITS_IN_CPK, &pack(&k))], false),
        )
        .unwrap();
        let kits = find_kits(&game);
        assert_eq!(kits[&126][0].color1, [0xef, 0xcf, 0x10]);
        std::fs::remove_dir_all(&game).ok();
    }
}
