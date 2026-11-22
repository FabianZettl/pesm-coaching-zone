//! Reading files straight out of CRI `.cpk` archives, so the player database
//! works without extracting the game's or a mod's CPKs first.
//!
//! Format: `CPK ` header followed by an `@UTF` table (sometimes XOR-obfuscated)
//! that points to a `TOC ` table listing every file. Files may be CRILAYLA-compressed.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One file inside a CPK.
#[derive(Debug, Clone)]
pub struct Entry {
    pub offset: u64,
    pub size: u64,
    pub extract_size: u64,
}

/// An opened CPK: its path and table of contents (lower-case paths with `/`).
#[derive(Debug, Clone)]
pub struct Cpk {
    pub path: PathBuf,
    pub files: HashMap<String, Entry>,
}

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Int(u64),
    Str(String),
    Data(Vec<u8>),
    None,
}

impl Val {
    fn int(&self) -> Option<u64> {
        match self {
            Val::Int(v) => Some(*v),
            _ => None,
        }
    }
    fn str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }
}

type Row = HashMap<String, Val>;

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn be64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Undoes the simple XOR obfuscation some tools apply to `@UTF` tables.
fn deobfuscate(b: &mut [u8]) {
    let mut m: u32 = 0x655f;
    for x in b.iter_mut() {
        *x ^= (m & 0xff) as u8;
        m = m.wrapping_mul(0x4115);
    }
}

/// Parses an `@UTF` table into rows.
fn parse_utf(packet: &[u8]) -> Option<Vec<Row>> {
    let mut b = packet.to_vec();
    if b.get(0..4)? != b"@UTF" {
        deobfuscate(&mut b);
        if b.get(0..4)? != b"@UTF" {
            return None;
        }
    }
    let rows_off = be32(&b, 8)? as usize + 8;
    let strings_off = be32(&b, 12)? as usize + 8;
    let data_off = be32(&b, 16)? as usize + 8;
    let ncols = be16(&b, 24)? as usize;
    let nrows = be32(&b, 28)? as usize;

    let cstr = |o: usize| -> Option<String> {
        let s = b.get(strings_off + o..)?;
        let n = s.iter().position(|c| *c == 0)?;
        Some(String::from_utf8_lossy(&s[..n]).into_owned())
    };
    // Reads one value of `ty` at `*p` and moves `p` on.
    let read = |ty: u8, p: &mut usize| -> Option<Val> {
        let o = *p;
        let (v, n) = match ty {
            0 | 1 => (Val::Int(*b.get(o)? as u64), 1),
            2 | 3 => (Val::Int(be16(&b, o)? as u64), 2),
            4 | 5 | 8 => (Val::Int(be32(&b, o)? as u64), 4),
            6 | 7 | 9 => (Val::Int(be64(&b, o)?), 8),
            0xa => (Val::Str(cstr(be32(&b, o)? as usize)?), 4),
            0xb => {
                let (at, len) = (be32(&b, o)? as usize, be32(&b, o + 4)? as usize);
                (
                    Val::Data(b.get(data_off + at..data_off + at + len)?.to_vec()),
                    8,
                )
            }
            _ => return None,
        };
        *p += n;
        Some(v)
    };

    let mut p = 32;
    let mut cols = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let flags = *b.get(p)?;
        p += 1;
        let name = cstr(be32(&b, p)? as usize)?;
        p += 4;
        let (storage, ty) = (flags & 0xf0, flags & 0x0f);
        let constant = if storage == 0x30 || storage == 0x70 {
            Some(read(ty, &mut p)?)
        } else {
            None
        };
        cols.push((name, storage, ty, constant));
    }
    let row_len = be16(&b, 26)? as usize;
    let mut rows = Vec::with_capacity(nrows);
    for r in 0..nrows {
        let mut p = rows_off + r * row_len;
        let mut row = Row::new();
        for (name, storage, ty, constant) in &cols {
            let v = match storage {
                0x50 => read(*ty, &mut p)?,
                0x30 | 0x70 => constant.clone().unwrap_or(Val::None),
                0x10 => Val::Int(0),
                _ => Val::None,
            };
            row.insert(name.clone(), v);
        }
        rows.push(row);
    }
    Some(rows)
}

/// Reads the `@UTF` packet that follows a 16-byte chunk header at `at`.
fn read_packet(f: &mut File, at: u64, magic: &[u8; 4]) -> Result<Vec<u8>> {
    let mut head = [0u8; 16];
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(&mut head)?;
    if &head[0..4] != magic {
        bail!("missing {} chunk", String::from_utf8_lossy(magic));
    }
    let size = u64::from_le_bytes(head[8..16].try_into().unwrap());
    if size > 512 << 20 {
        bail!("table too large");
    }
    let mut buf = vec![0u8; size as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

impl Cpk {
    /// Opens a CPK and reads its table of contents. Fails for encrypted or non-CPK files.
    pub fn open(path: &Path) -> Result<Self> {
        let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let header = parse_utf(&read_packet(&mut f, 0, b"CPK ")?)
            .and_then(|r| r.into_iter().next())
            .context("unreadable CPK header")?;
        let toc = header
            .get("TocOffset")
            .and_then(Val::int)
            .context("no TOC")?;
        let content = header
            .get("ContentOffset")
            .and_then(Val::int)
            .unwrap_or(toc);
        let add = toc.min(content);
        let rows = parse_utf(&read_packet(&mut f, toc, b"TOC ")?).context("unreadable TOC")?;
        let mut files = HashMap::new();
        for r in rows {
            let dir = r.get("DirName").and_then(Val::str).unwrap_or("");
            let Some(name) = r.get("FileName").and_then(Val::str) else {
                continue;
            };
            let full = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}/{name}")
            };
            let (Some(off), Some(size)) = (
                r.get("FileOffset").and_then(Val::int),
                r.get("FileSize").and_then(Val::int),
            ) else {
                continue;
            };
            let extract_size = r.get("ExtractSize").and_then(Val::int).unwrap_or(size);
            files.insert(
                full.replace('\\', "/").to_ascii_lowercase(),
                Entry {
                    offset: off + add,
                    size,
                    extract_size,
                },
            );
        }
        Ok(Self {
            path: path.to_path_buf(),
            files,
        })
    }

    pub fn has(&self, name: &str) -> bool {
        self.files.contains_key(&name.to_ascii_lowercase())
    }

    pub fn size_of(&self, name: &str) -> Option<u64> {
        self.files
            .get(&name.to_ascii_lowercase())
            .map(|e| e.extract_size)
    }

    /// Reads one file, decompressing it if needed.
    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        let e = self
            .files
            .get(&name.to_ascii_lowercase())
            .with_context(|| format!("{name} not in {}", self.path.display()))?;
        if e.size > 1 << 30 {
            bail!("{name} is too large");
        }
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(e.offset))?;
        let mut buf = vec![0u8; e.size as usize];
        f.read_exact(&mut buf)?;
        if buf.starts_with(b"CRILAYLA") {
            return crilayla(&buf).context("decompressing");
        }
        Ok(buf)
    }
}

/// CRILAYLA decompression (backwards LZ with a raw 0x100-byte header at the end).
pub fn crilayla(input: &[u8]) -> Result<Vec<u8>> {
    if input.len() < 0x110 || !input.starts_with(b"CRILAYLA") {
        bail!("not CRILAYLA data");
    }
    let usize_ = u32::from_le_bytes(input[8..12].try_into().unwrap()) as usize;
    let hdr = u32::from_le_bytes(input[12..16].try_into().unwrap()) as usize;
    let raw = input
        .get(hdr + 0x10..hdr + 0x110)
        .context("CRILAYLA header out of range")?;
    let mut out = vec![0u8; usize_ + 0x100];
    out[..0x100].copy_from_slice(raw);

    let mut pos = input.len() as isize - 0x100 - 1;
    let (mut pool, mut left) = (0u8, 0u32);
    let mut bits = |n: u32| -> Result<u32> {
        let mut v = 0u32;
        let mut got = 0;
        while got < n {
            if left == 0 {
                if pos < 0 {
                    bail!("CRILAYLA stream ended early");
                }
                pool = input[pos as usize];
                left = 8;
                pos -= 1;
            }
            let take = left.min(n - got);
            v = (v << take) | ((pool as u32 >> (left - take)) & ((1 << take) - 1));
            left -= take;
            got += take;
        }
        Ok(v)
    };
    let end = 0x100 + usize_ - 1;
    let mut done = 0usize;
    const LENS: [u32; 4] = [2, 3, 5, 8];
    while done < usize_ {
        if bits(1)? == 1 {
            let mut from = (end - done) + bits(13)? as usize + 3;
            let mut len = 3usize;
            let mut level = 0;
            while level < LENS.len() {
                let v = bits(LENS[level])?;
                len += v as usize;
                if v != (1 << LENS[level]) - 1 {
                    break;
                }
                level += 1;
            }
            if level == LENS.len() {
                loop {
                    let v = bits(8)?;
                    len += v as usize;
                    if v != 255 {
                        break;
                    }
                }
            }
            for _ in 0..len {
                if done >= usize_ || from >= out.len() {
                    bail!("bad CRILAYLA back-reference");
                }
                out[end - done] = out[from];
                from -= 1;
                done += 1;
            }
        } else {
            out[end - done] = bits(8)? as u8;
            done += 1;
        }
    }
    Ok(out)
}

/// All `.cpk` files under `dir`, a few folders deep.
pub fn find_files(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                out.extend(find_files(&p, depth - 1));
            }
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("cpk")) {
            out.push(p);
        }
    }
    out.sort();
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Builds a small uncompressed `@UTF` table. Columns are all per-row strings or u64s.
    fn utf(name: &str, cols: &[(&str, bool)], rows: &[Vec<Val>]) -> Vec<u8> {
        let mut strings = b"<NULL>\0".to_vec();
        let mut s = |x: &str| {
            let o = strings.len() as u32;
            strings.extend(x.as_bytes());
            strings.push(0);
            o
        };
        let tname = s(name);
        let mut schema = vec![];
        for (c, is_str) in cols {
            schema.push(if *is_str { 0x5a } else { 0x56 });
            schema.extend(s(c).to_be_bytes());
        }
        let row_len: usize = cols.iter().map(|(_, st)| if *st { 4 } else { 8 }).sum();
        let mut rowb = vec![];
        for r in rows {
            for v in r {
                match v {
                    Val::Str(x) => rowb.extend(s(x).to_be_bytes()),
                    Val::Int(x) => rowb.extend(x.to_be_bytes()),
                    _ => unreachable!(),
                }
            }
        }
        let rows_off = 32 + schema.len();
        let strings_off = rows_off + rowb.len();
        let data_off = strings_off + strings.len();
        let mut b = b"@UTF".to_vec();
        b.extend(((data_off - 8) as u32).to_be_bytes());
        b.extend(((rows_off - 8) as u32).to_be_bytes());
        b.extend(((strings_off - 8) as u32).to_be_bytes());
        b.extend(((data_off - 8) as u32).to_be_bytes());
        b.extend(tname.to_be_bytes());
        b.extend((cols.len() as u16).to_be_bytes());
        b.extend((row_len as u16).to_be_bytes());
        b.extend((rows.len() as u32).to_be_bytes());
        b.extend(schema);
        b.extend(rowb);
        b.extend(strings);
        b
    }

    fn chunk(magic: &[u8; 4], packet: &[u8]) -> Vec<u8> {
        let mut c = magic.to_vec();
        c.extend([0xff, 0, 0, 0]);
        c.extend((packet.len() as u64).to_le_bytes());
        c.extend(packet);
        c
    }

    /// Writes a CPK holding `files` (stored uncompressed) and returns its bytes.
    pub fn build(files: &[(&str, &[u8])], obfuscate: bool) -> Vec<u8> {
        let content_at = 0x800u64;
        let mut content: Vec<u8> = vec![];
        let mut rows = vec![];
        for (path, data) in files {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            rows.push(vec![
                Val::Str(dir.into()),
                Val::Str(name.into()),
                Val::Int(data.len() as u64),
                Val::Int(data.len() as u64),
                Val::Int(content_at + content.len() as u64),
            ]);
            content.extend(*data);
        }
        let toc_at = content_at + content.len() as u64;
        // FileOffset is relative to min(TocOffset, ContentOffset) = ContentOffset here.
        for r in rows.iter_mut() {
            if let Val::Int(o) = r[4] {
                r[4] = Val::Int(o - content_at);
            }
        }
        let mut toc = utf(
            "CpkTocInfo",
            &[
                ("DirName", true),
                ("FileName", true),
                ("FileSize", false),
                ("ExtractSize", false),
                ("FileOffset", false),
            ],
            &rows,
        );
        if obfuscate {
            deobfuscate(&mut toc);
        }
        let header = utf(
            "CpkHeader",
            &[("ContentOffset", false), ("TocOffset", false)],
            &[vec![Val::Int(content_at), Val::Int(toc_at)]],
        );
        let mut out = chunk(b"CPK ", &header);
        out.resize(content_at as usize, 0);
        out.extend(content);
        out.extend(chunk(b"TOC ", &toc));
        out
    }

    /// A CRILAYLA stream with only literals (enough to check the bit reader and layout).
    fn crilayla_literals(data: &[u8]) -> Vec<u8> {
        let (head, body) = data.split_at(0x100);
        // Bits in reading order: body is produced from its end backwards.
        let mut bits = vec![];
        for &byte in body.iter().rev() {
            bits.push(0u8);
            for i in (0..8).rev() {
                bits.push((byte >> i) & 1);
            }
        }
        while bits.len() % 8 != 0 {
            bits.push(0);
        }
        // The reader takes bytes from the end of the stream towards its start.
        let mut stream: Vec<u8> = bits
            .chunks(8)
            .map(|c| c.iter().fold(0u8, |a, b| (a << 1) | b))
            .collect();
        stream.reverse();
        let mut out = b"CRILAYLA".to_vec();
        out.extend((body.len() as u32).to_le_bytes());
        out.extend((stream.len() as u32).to_le_bytes());
        out.extend(&stream);
        out.extend(head);
        out
    }

    #[test]
    fn reads_files_from_a_cpk() {
        let dir = std::env::temp_dir().join(format!("cz-cpk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for obf in [false, true] {
            let p = dir.join(format!("t{obf}.cpk"));
            std::fs::write(
                &p,
                build(
                    &[
                        ("common/etc/pesdb/Player.bin", b"players"),
                        ("common/etc/pesdb/Team.bin", b"teams!"),
                    ],
                    obf,
                ),
            )
            .unwrap();
            let c = Cpk::open(&p).unwrap();
            assert!(c.has("common/etc/pesdb/player.bin"));
            assert_eq!(c.read("common/etc/pesdb/Player.bin").unwrap(), b"players");
            assert_eq!(c.read("common/etc/pesdb/Team.bin").unwrap(), b"teams!");
        }
        assert_eq!(find_files(&dir, 1).len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn crilayla_roundtrip_literals() {
        let data: Vec<u8> = (0..700u32).map(|i| (i * 7 % 251) as u8).collect();
        assert_eq!(crilayla(&crilayla_literals(&data)).unwrap(), data);
    }
}
