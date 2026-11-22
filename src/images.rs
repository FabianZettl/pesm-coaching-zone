//! Club crests and player photos from the game's mod folders and CPK archives.
//!
//! The game keeps them under `common/render/symbol/`:
//! - crests: `flag/e_<team id, 6 digits>_<variant>.(png|dds)`
//! - player photos: `player/<player id>.(png|dds)`
//!
//! Search order (the first place that has a file wins):
//! 1. active `cpk.root` folders from sider.ini, in their order,
//! 2. `.cpk` archives in `<game>/download` and `<game>/Data` (a later archive wins),
//! 3. the other `SiderAddons/livecpk` folders,
//! 4. `SiderAddons/content/**/render/symbol`.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cpk::Cpk;

/// Crest variants, best first.
const CREST_VARIANTS: [&str; 9] = [
    "r_l", "r_w_l", "r_b_l", "r", "r_w", "r_b", "r_ll", "r_w_ll", "r_b_ll",
];
/// Only these sub-folders of `render/symbol` are indexed.
const KINDS: [&str; 2] = ["flag", "player"];

enum Loc {
    File(PathBuf),
    Cpk(usize, String),
}

pub struct Images {
    cpks: Vec<Cpk>,
    /// `flag/e_000396_r_l.png` (lower case) -> (source rank, where it is).
    index: HashMap<String, (usize, Loc)>,
    sources: usize,
    cache: Mutex<HashMap<String, Option<Arc<Vec<u8>>>>>,
}

impl Images {
    pub fn empty() -> Self {
        Self {
            cpks: vec![],
            index: HashMap::new(),
            sources: 0,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Builds the index for a game folder (the one with FL_2027.exe and SiderAddons).
    pub fn from_game(game: &Path) -> Self {
        let mut me = Self::empty();
        let sider = find_ci(game, "SiderAddons");
        let inis = [
            game.join("sider.ini"),
            sider.clone().unwrap_or_default().join("sider.ini"),
        ];
        let active: Vec<PathBuf> = inis
            .iter()
            .flat_map(|i| crate::pesdb::sider_roots(i))
            .filter_map(|p| resolve_ci(&p))
            .collect();
        for r in &active {
            if let Some(sym) = symbol_dir(r) {
                me.add_dir(&sym);
            }
            me.add_faces(r);
        }
        let mut archives = vec![];
        for sub in ["download", "Data"] {
            if let Some(d) = find_ci(game, sub) {
                archives.extend(crate::cpk::find_files(&d, 1));
            }
        }
        // download/ patches come after Data/; within a folder the later name wins.
        archives.sort_by_key(|p| {
            let in_download = p
                .components()
                .any(|c| c.as_os_str().eq_ignore_ascii_case("download"));
            (!in_download, std::cmp::Reverse(p.clone()))
        });
        for a in archives {
            me.add_cpk(a);
        }
        if let Some(lc) = sider.as_ref().and_then(|s| find_ci(s, "livecpk"))
            && let Ok(rd) = std::fs::read_dir(&lc)
        {
            let mut others: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && !active.contains(p))
                .collect();
            others.sort();
            for o in others {
                if let Some(sym) = symbol_dir(&o) {
                    me.add_dir(&sym);
                }
                me.add_faces(&o);
            }
        }
        if let Some(content) = sider.as_ref().and_then(|s| find_ci(s, "content")) {
            let mut found = vec![];
            find_symbol_dirs(&content, 5, &mut found);
            found.sort();
            for s in found {
                me.add_dir(&s);
            }
        }
        me
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn crest_count(&self) -> usize {
        self.index.keys().filter(|k| k.starts_with("flag/")).count()
    }

    pub fn photo_count(&self) -> usize {
        self.index
            .keys()
            .filter(|k| k.starts_with("player/"))
            .count()
    }

    fn add(&mut self, key: String, loc: Loc) {
        self.index.entry(key).or_insert((self.sources, loc));
    }

    /// Indexes a `render/symbol` folder.
    fn add_dir(&mut self, symbol: &Path) {
        let Ok(rd) = std::fs::read_dir(symbol) else {
            return;
        };
        self.sources += 1;
        for e in rd.flatten() {
            let kind = e.file_name().to_string_lossy().to_ascii_lowercase();
            if !KINDS.contains(&kind.as_str()) || !e.path().is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(e.path()) else {
                continue;
            };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_ascii_lowercase();
                if name.ends_with(".png") || name.ends_with(".dds") {
                    self.add(format!("{kind}/{name}"), Loc::File(f.path()));
                }
            }
        }
    }

    /// Facepacks: `<root>/Asset/model/character/face/real/<player id>/portrait.dds`.
    fn add_faces(&mut self, root: &Path) {
        let Some(real) = ["Asset", "model", "character", "face", "real"]
            .iter()
            .try_fold(root.to_path_buf(), |p, n| find_ci(&p, n))
        else {
            return;
        };
        let Ok(rd) = std::fs::read_dir(&real) else {
            return;
        };
        self.sources += 1;
        for e in rd.flatten() {
            let Ok(id) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            for ext in ["dds", "png"] {
                if let Some(f) = find_ci(&e.path(), &format!("portrait.{ext}"))
                    && f.is_file()
                {
                    self.add(format!("player/{id}.{ext}"), Loc::File(f));
                    break;
                }
            }
        }
    }

    fn add_cpk(&mut self, path: PathBuf) {
        let Ok(c) = Cpk::open(&path) else {
            return;
        };
        self.sources += 1;
        let i = self.cpks.len();
        let mut any = false;
        let mut keys: Vec<(String, String)> = vec![];
        for name in c.files.keys() {
            if let Some(k) = portrait_key(name) {
                keys.push((k, name.clone()));
                continue;
            }
            let Some(pos) = name.find("render/symbol/") else {
                continue;
            };
            let key = &name[pos + "render/symbol/".len()..];
            if KINDS.iter().any(|k| key.starts_with(&format!("{k}/")))
                && (key.ends_with(".png") || key.ends_with(".dds"))
            {
                keys.push((key.to_string(), name.clone()));
            }
        }
        for (k, n) in keys {
            any = true;
            self.add(k, Loc::Cpk(i, n));
        }
        if any {
            self.cpks.push(c);
        }
    }

    fn load(&self, key: &str) -> Option<Vec<u8>> {
        let raw = match &self.index.get(key)?.1 {
            Loc::File(p) => std::fs::read(p).ok()?,
            Loc::Cpk(i, n) => self.cpks[*i].read(n).ok()?,
        };
        let raw = crate::pesdb::unpack_or_raw(&raw).ok()?;
        if raw.starts_with(b"\x89PNG") {
            Some(raw)
        } else {
            dds_to_png(&raw).ok()
        }
    }

    /// The best of `keys` as PNG bytes: the earliest source wins, then the order of `keys`.
    /// Results are cached.
    fn first(&self, cache_key: String, keys: impl Iterator<Item = String>) -> Option<Arc<Vec<u8>>> {
        if let Some(hit) = self.cache.lock().unwrap().get(&cache_key) {
            return hit.clone();
        }
        let mut have: Vec<(usize, usize, String)> = keys
            .enumerate()
            .filter_map(|(i, k)| self.index.get(&k).map(|(rank, _)| (*rank, i, k)))
            .collect();
        have.sort();
        let found = have.iter().find_map(|(_, _, k)| self.load(k)).map(Arc::new);
        self.cache.lock().unwrap().insert(cache_key, found.clone());
        found
    }

    /// Club crest as PNG.
    pub fn crest(&self, team: u32) -> Option<Arc<Vec<u8>>> {
        self.first(
            format!("t{team}"),
            CREST_VARIANTS
                .iter()
                .flat_map(move |v| ["png", "dds"].map(|x| format!("flag/e_{team:06}_{v}.{x}"))),
        )
    }

    /// Player photo as PNG.
    pub fn photo(&self, player: u32) -> Option<Arc<Vec<u8>>> {
        self.first(
            format!("p{player}"),
            ["png", "dds"]
                .into_iter()
                .map(move |x| format!("player/{player}.{x}")),
        )
    }
}

/// `.../face/real/<id>/portrait.dds` (lower case) -> `player/<id>.dds`.
fn portrait_key(name: &str) -> Option<String> {
    let pos = name.find("face/real/")?;
    let (id, file) = name[pos + "face/real/".len()..].split_once('/')?;
    let id: u32 = id.parse().ok()?;
    let ext = file.strip_prefix("portrait.")?;
    matches!(ext, "dds" | "png").then(|| format!("player/{id}.{ext}"))
}

/// `dir/<name>`, matching the name without regard to case.
fn find_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.exists() {
        return Some(direct);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .map(|e| e.path())
}

/// The real path for a path whose case may be wrong (sider.ini paths come from Windows).
fn resolve_ci(p: &Path) -> Option<PathBuf> {
    if p.exists() {
        return Some(p.to_path_buf());
    }
    let mut cur = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::Normal(n) => cur = find_ci(&cur, &n.to_string_lossy())?,
            other => cur.push(other.as_os_str()),
        }
    }
    Some(cur)
}

/// `<root>/common/render/symbol`, any case.
fn symbol_dir(root: &Path) -> Option<PathBuf> {
    let c = find_ci(root, "common")?;
    let r = find_ci(&c, "render")?;
    find_ci(&r, "symbol")
}

fn find_symbol_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if e.file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case("render")
            && let Some(s) = find_ci(&p, "symbol")
        {
            out.push(s);
        } else if depth > 0 {
            find_symbol_dirs(&p, depth - 1, out);
        }
    }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

type BlockDecoder = fn(&[u8], usize, usize, &mut [u32]) -> Result<(), &'static str>;

/// Decodes a DDS texture (BC1-BC5, BC7 or plain 24/32-bit) to PNG.
pub fn dds_to_png(dds: &[u8]) -> Result<Vec<u8>> {
    if dds.len() < 128 || &dds[..4] != b"DDS " {
        bail!("not a DDS image");
    }
    let height = u32_at(dds, 12) as usize;
    let width = u32_at(dds, 16) as usize;
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        bail!("odd DDS size {width}x{height}");
    }
    let pf_flags = u32_at(dds, 80);
    let four = &dds[84..88];
    let mut data = &dds[128..];
    let mut px = vec![0u32; width * height];
    use texture2ddecoder as t;
    let bc = |f: BlockDecoder, data: &[u8], px: &mut [u32]| {
        f(data, width, height, px).map_err(|e| anyhow::anyhow!("DDS: {e}"))
    };
    let plain = |data: &[u8], px: &mut [u32], bits: usize, masks: [u32; 4]| -> Result<()> {
        let bpp = bits / 8;
        if data.len() < width * height * bpp {
            bail!("DDS data too short");
        }
        let take = |v: u32, m: u32| -> u8 {
            if m == 0 {
                return 255;
            }
            let s = m.trailing_zeros();
            let max = m >> s;
            (((v & m) >> s) * 255 / max) as u8
        };
        for (i, p) in px.iter_mut().enumerate() {
            let mut b = [0u8; 4];
            b[..bpp].copy_from_slice(&data[i * bpp..i * bpp + bpp]);
            let v = u32::from_le_bytes(b);
            let [r, g, bl, a] = masks.map(|m| take(v, m));
            *p = u32::from_le_bytes([bl, g, r, a]);
        }
        Ok(())
    };
    match four {
        b"DXT1" => bc(t::decode_bc1a, data, &mut px)?,
        b"DXT2" | b"DXT3" => bc(t::decode_bc2, data, &mut px)?,
        b"DXT4" | b"DXT5" => bc(t::decode_bc3, data, &mut px)?,
        b"ATI1" | b"BC4U" => bc(t::decode_bc4, data, &mut px)?,
        b"ATI2" | b"BC5U" => bc(t::decode_bc5, data, &mut px)?,
        b"DX10" => {
            if dds.len() < 148 {
                bail!("short DX10 header");
            }
            let fmt = u32_at(dds, 128);
            data = &dds[148..];
            match fmt {
                70..=72 => bc(t::decode_bc1a, data, &mut px)?,
                73..=75 => bc(t::decode_bc2, data, &mut px)?,
                76..=78 => bc(t::decode_bc3, data, &mut px)?,
                79..=81 => bc(t::decode_bc4, data, &mut px)?,
                82..=84 => bc(t::decode_bc5, data, &mut px)?,
                97..=99 => bc(t::decode_bc7, data, &mut px)?,
                27..=29 => plain(data, &mut px, 32, [0xff, 0xff00, 0xff0000, 0xff000000])?,
                87 | 90 | 91 => plain(data, &mut px, 32, [0xff0000, 0xff00, 0xff, 0xff000000])?,
                _ => bail!("DDS format {fmt} not supported"),
            }
        }
        _ if pf_flags & 0x40 != 0 => {
            let bits = u32_at(dds, 88) as usize;
            if bits != 24 && bits != 32 {
                bail!("DDS with {bits} bits per pixel not supported");
            }
            let alpha = if pf_flags & 0x1 != 0 {
                u32_at(dds, 104)
            } else {
                0
            };
            plain(
                data,
                &mut px,
                bits,
                [u32_at(dds, 92), u32_at(dds, 96), u32_at(dds, 100), alpha],
            )?
        }
        _ => bail!("DDS type {:?} not supported", String::from_utf8_lossy(four)),
    }
    let rgba: Vec<u8> = px
        .iter()
        .flat_map(|p| {
            let [b, g, r, a] = p.to_le_bytes();
            [r, g, b, a]
        })
        .collect();
    let mut out = vec![];
    {
        let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().context("PNG header")?;
        w.write_image_data(&rgba).context("PNG data")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dds_plain(w: u32, h: u32, pixel: [u8; 4]) -> Vec<u8> {
        let mut d = vec![0u8; 128];
        d[..4].copy_from_slice(b"DDS ");
        d[4..8].copy_from_slice(&124u32.to_le_bytes());
        d[12..16].copy_from_slice(&h.to_le_bytes());
        d[16..20].copy_from_slice(&w.to_le_bytes());
        d[76..80].copy_from_slice(&32u32.to_le_bytes());
        d[80..84].copy_from_slice(&0x41u32.to_le_bytes());
        d[88..92].copy_from_slice(&32u32.to_le_bytes());
        // A8R8G8B8: bytes in memory are B, G, R, A.
        d[92..96].copy_from_slice(&0xff0000u32.to_le_bytes());
        d[96..100].copy_from_slice(&0xff00u32.to_le_bytes());
        d[100..104].copy_from_slice(&0xffu32.to_le_bytes());
        d[104..108].copy_from_slice(&0xff000000u32.to_le_bytes());
        for _ in 0..w * h {
            d.extend(pixel);
        }
        d
    }

    fn png_pixel(png_bytes: &[u8]) -> (u32, u32, [u8; 4]) {
        let dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        (info.width, info.height, buf[..4].try_into().unwrap())
    }

    #[test]
    fn decodes_plain_and_bc1_dds() {
        let png = dds_to_png(&dds_plain(4, 2, [0x10, 0x20, 0xc0, 0xff])).unwrap();
        assert_eq!(png_pixel(&png), (4, 2, [0xc0, 0x20, 0x10, 0xff]));

        // BC1 block: both end colours pure red (0xf800), all indices 0.
        let mut d = dds_plain(4, 4, [0; 4]);
        d.truncate(128);
        d[80..84].copy_from_slice(&4u32.to_le_bytes());
        d[84..88].copy_from_slice(b"DXT1");
        d.extend([0x00, 0xf8, 0x00, 0xf8, 0, 0, 0, 0]);
        let (w, h, p) = png_pixel(&dds_to_png(&d).unwrap());
        assert_eq!((w, h), (4, 4));
        assert_eq!(p[0], 0xff);
        assert_eq!(p[1], 0);
        assert!(dds_to_png(b"nope").is_err());
    }

    #[test]
    fn finds_crests_and_photos_in_order() {
        let game = std::env::temp_dir().join(format!("cz-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&game);
        let sider = game.join("SiderAddons");
        let world = sider.join("livecpk/World/common/render/symbol");
        std::fs::create_dir_all(world.join("flag")).unwrap();
        std::fs::create_dir_all(sider.join("livecpk/Off/common/render/symbol/player")).unwrap();
        std::fs::create_dir_all(game.join("download")).unwrap();
        std::fs::write(
            sider.join("sider.ini"),
            "[sider]\n#cpk.root = \".\\livecpk\\Off\"\ncpk.root = \".\\livecpk\\world\"\n",
        )
        .unwrap();
        let png_a = dds_to_png(&dds_plain(1, 1, [0, 0, 255, 255])).unwrap();
        let png_b = dds_to_png(&dds_plain(1, 1, [255, 0, 0, 255])).unwrap();
        // The world has a plain crest for 71581; the CPK has a better variant for 396.
        std::fs::write(world.join("flag/e_071581_r.png"), &png_a).unwrap();
        std::fs::write(
            game.join("download/dt99.cpk"),
            crate::cpk::tests::build(
                &[
                    (
                        "common/render/symbol/flag/e_000396_r_l.dds",
                        &dds_plain(2, 2, [0, 255, 0, 255]),
                    ),
                    ("common/render/symbol/flag/e_071581_r_l.png", &png_b),
                ],
                false,
            ),
        )
        .unwrap();
        std::fs::write(
            sider.join("livecpk/Off/common/render/symbol/player/42.png"),
            &png_b,
        )
        .unwrap();
        let face = sider.join("livecpk/World/Asset/model/character/face/real/77");
        std::fs::create_dir_all(&face).unwrap();
        std::fs::write(face.join("portrait.dds"), dds_plain(1, 1, [9, 9, 9, 255])).unwrap();
        let im = Images::from_game(&game);
        assert_eq!(png_pixel(&im.photo(77).unwrap()), (1, 1, [9, 9, 9, 255]));
        assert_eq!(im.crest_count(), 3);
        assert_eq!(png_pixel(&im.crest(396).unwrap()), (2, 2, [0, 255, 0, 255]));
        // The active world folder beats the CPK, even with a lesser variant.
        assert_eq!(*im.crest(71581).unwrap(), png_a);
        assert_eq!(*im.photo(42).unwrap(), png_b);
        assert!(im.photo(43).is_none());
        assert!(im.crest(1).is_none());
        std::fs::remove_dir_all(&game).ok();
    }
}
