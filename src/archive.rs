// Bounce archive container (.bnc)
//
// A simple, sequential multi-file container. Each member file is compressed
// independently with the Big Bounce smart codec, which keeps `list`, `test`
// and selective `extract` cheap (no need to inflate unrelated members).
//
// On-disk layout (all integers little-endian):
//
//   Archive header:
//     magic       [4]  = "BNC1"
//     version     u8   = 1
//     entry_count u32
//
//   Repeated `entry_count` times:
//     path_len    u16
//     path        [path_len]  UTF-8, '/'-separated, relative
//     mode        u32         unix permission bits
//     mtime       i64         seconds since UNIX epoch
//     orig_size   u64         original (uncompressed) length
//     stored_size u64         payload length on disk
//     method      u8          codec method id (see codec::CompressMethod)
//     stored_raw  u8          1 = payload is the raw bytes (incompressible)
//     crc32       u32         CRC-32 (IEEE) of the original bytes
//     payload     [stored_size]

use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::codec;

pub const MAGIC: &[u8; 4] = b"BNC1";
pub const FORMAT_VERSION: u8 = 1;

// ── CRC-32 (IEEE 802.3) ─────────────────────────────────────────────────────
//
// Slicing-by-8 implementation: processes 8 input bytes per iteration through
// eight precomputed 256-entry tables, reaching multiple GB/s. The tables are
// built once and cached, so verifying large outputs does not bottleneck decode.

fn crc32_tables() -> &'static [[u32; 256]; 8] {
    static TABLES: OnceLock<[[u32; 256]; 8]> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        // Standard CRC-32 table (slice 0).
        let mut n = 0;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            t[0][n] = c;
            n += 1;
        }
        // Derive slices 1..8 from the previous slice.
        let mut n = 0;
        while n < 256 {
            let mut crc = t[0][n];
            let mut i = 1;
            while i < 8 {
                crc = t[0][(crc & 0xFF) as usize] ^ (crc >> 8);
                t[i][n] = crc;
                i += 1;
            }
            n += 1;
        }
        t
    })
}

pub fn crc32(data: &[u8]) -> u32 {
    let t = crc32_tables();
    let mut crc = 0xFFFF_FFFFu32;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        crc ^= u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        crc = t[7][(crc & 0xFF) as usize]
            ^ t[6][((crc >> 8) & 0xFF) as usize]
            ^ t[5][((crc >> 16) & 0xFF) as usize]
            ^ t[4][((crc >> 24) & 0xFF) as usize]
            ^ t[3][c[4] as usize]
            ^ t[2][c[5] as usize]
            ^ t[1][c[6] as usize]
            ^ t[0][c[7] as usize];
    }
    for &b in chunks.remainder() {
        crc = t[0][((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

// ── Entry metadata ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct EntryMeta {
    pub path: String,
    pub mode: u32,
    #[allow(dead_code)]
    pub mtime: i64,
    pub orig_size: u64,
    pub stored_size: u64,
    pub method: u8,
    pub stored_raw: bool,
    pub crc: u32,
}

// ── Input collection ────────────────────────────────────────────────────────

/// A file selected for archiving: its location on disk and the relative path
/// to record inside the archive.
struct InputFile {
    abs: PathBuf,
    rel: String,
}

fn normalize_rel(rel: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in rel.components() {
        use std::path::Component;
        match comp {
            Component::Normal(os) => parts.push(os.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {}
        }
    }
    parts.join("/")
}

fn collect_inputs(inputs: &[String]) -> io::Result<Vec<InputFile>> {
    let mut out = Vec::new();
    for raw in inputs {
        let p = Path::new(raw);
        let meta = fs::symlink_metadata(p)?;
        if meta.is_dir() {
            // Record paths relative to the directory's parent so the top
            // directory name is preserved inside the archive.
            let base = p.parent().unwrap_or_else(|| Path::new(""));
            walk_dir(p, base, &mut out)?;
        } else if meta.is_file() {
            let rel = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| raw.clone());
            out.push(InputFile {
                abs: p.to_path_buf(),
                rel,
            });
        }
        // Symlinks and special files are skipped.
    }
    Ok(out)
}

fn walk_dir(dir: &Path, base: &Path, out: &mut Vec<InputFile>) -> io::Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            walk_dir(&path, base, out)?;
        } else if meta.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            out.push(InputFile {
                abs: path.clone(),
                rel: normalize_rel(rel),
            });
        }
    }
    Ok(())
}

// ── Metadata helpers ────────────────────────────────────────────────────────

fn file_mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

fn file_mtime(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Create ──────────────────────────────────────────────────────────────────

pub struct CreateStats {
    pub files: usize,
    pub orig_total: u64,
    pub stored_total: u64,
}

pub fn create(
    archive_path: &str,
    inputs: &[String],
    verbose: bool,
) -> io::Result<CreateStats> {
    let files = collect_inputs(inputs)?;
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no regular files to archive",
        ));
    }

    let out_file = fs::File::create(archive_path)?;
    let mut w = BufWriter::new(out_file);

    w.write_all(MAGIC)?;
    w.write_all(&[FORMAT_VERSION])?;
    w.write_all(&(files.len() as u32).to_le_bytes())?;

    let mut stats = CreateStats {
        files: 0,
        orig_total: 0,
        stored_total: 0,
    };

    for f in &files {
        let meta = fs::metadata(&f.abs)?;
        let data = fs::read(&f.abs)?;
        let crc = crc32(&data);
        let mode = file_mode(&meta);
        let mtime = file_mtime(&meta);

        // Compress; fall back to raw storage when it does not help.
        let (payload, method, stored_raw): (Vec<u8>, u8, bool) =
            match codec::smart_compress(&data) {
                Some((c, m)) if c.len() < data.len() => (c, m.to_u8(), false),
                _ => (data.clone(), codec::CompressMethod::Plain.to_u8(), true),
            };

        let path_bytes = f.rel.as_bytes();
        w.write_all(&(path_bytes.len() as u16).to_le_bytes())?;
        w.write_all(path_bytes)?;
        w.write_all(&mode.to_le_bytes())?;
        w.write_all(&mtime.to_le_bytes())?;
        w.write_all(&(data.len() as u64).to_le_bytes())?;
        w.write_all(&(payload.len() as u64).to_le_bytes())?;
        w.write_all(&[method])?;
        w.write_all(&[stored_raw as u8])?;
        w.write_all(&crc.to_le_bytes())?;
        w.write_all(&payload)?;

        stats.files += 1;
        stats.orig_total += data.len() as u64;
        stats.stored_total += payload.len() as u64;

        if verbose {
            let pct = if data.is_empty() {
                0.0
            } else {
                payload.len() as f64 / data.len() as f64 * 100.0
            };
            eprintln!(
                "  adding: {}  ({} -> {} bytes, {:.1}%)",
                f.rel,
                data.len(),
                payload.len(),
                pct
            );
        }
    }

    w.flush()?;
    Ok(stats)
}

// ── Reading ─────────────────────────────────────────────────────────────────

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn read_u16<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}

fn open_archive(archive_path: &str) -> io::Result<(BufReader<fs::File>, u32)> {
    let file = fs::File::open(archive_path)?;
    let mut r = BufReader::new(file);

    let mut magic = [0u8; 4];
    if !read_exact_or_eof(&mut r, &mut magic)? || &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a bounce archive (bad magic)",
        ));
    }
    let mut ver = [0u8; 1];
    r.read_exact(&mut ver)?;
    if ver[0] != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported archive version {}", ver[0]),
        ));
    }
    let count = read_u32(&mut r)?;
    Ok((r, count))
}

fn read_entry_header<R: Read>(r: &mut R) -> io::Result<EntryMeta> {
    let path_len = read_u16(r)? as usize;
    let mut path_buf = vec![0u8; path_len];
    r.read_exact(&mut path_buf)?;
    let path = String::from_utf8_lossy(&path_buf).into_owned();
    let mode = read_u32(r)?;
    let mtime = read_i64(r)?;
    let orig_size = read_u64(r)?;
    let stored_size = read_u64(r)?;
    let mut m = [0u8; 1];
    r.read_exact(&mut m)?;
    let mut raw = [0u8; 1];
    r.read_exact(&mut raw)?;
    let crc = read_u32(r)?;
    Ok(EntryMeta {
        path,
        mode,
        mtime,
        orig_size,
        stored_size,
        method: m[0],
        stored_raw: raw[0] != 0,
        crc,
    })
}

/// Read every entry header (payloads skipped) for listing.
pub fn list_entries(archive_path: &str) -> io::Result<Vec<EntryMeta>> {
    let (mut r, count) = open_archive(archive_path)?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let meta = read_entry_header(&mut r)?;
        r.seek(SeekFrom::Current(meta.stored_size as i64))?;
        out.push(meta);
    }
    Ok(out)
}

fn decode_payload(meta: &EntryMeta, payload: &[u8]) -> io::Result<Vec<u8>> {
    if meta.stored_raw {
        return Ok(payload.to_vec());
    }
    let method = codec::CompressMethod::from_u8(meta.method).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown codec method {}", meta.method),
        )
    })?;
    codec::smart_decompress(payload, method, meta.orig_size as usize)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Verify CRC of every member. Returns the number of files checked.
pub fn test(archive_path: &str, verbose: bool) -> io::Result<usize> {
    let (mut r, count) = open_archive(archive_path)?;
    for _ in 0..count {
        let meta = read_entry_header(&mut r)?;
        let mut payload = vec![0u8; meta.stored_size as usize];
        r.read_exact(&mut payload)?;
        let data = decode_payload(&meta, &payload)?;
        if data.len() as u64 != meta.orig_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: size mismatch ({} != {})",
                    meta.path,
                    data.len(),
                    meta.orig_size
                ),
            ));
        }
        let actual = crc32(&data);
        if actual != meta.crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: CRC mismatch (corrupt)", meta.path),
            ));
        }
        if verbose {
            eprintln!("  OK: {}", meta.path);
        }
    }
    Ok(count as usize)
}

fn restore_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

fn safe_join(dest: &Path, rel: &str) -> io::Result<PathBuf> {
    // Guard against path traversal: reject absolute paths and any `..`.
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refusing absolute path in archive: {}", rel),
        ));
    }
    for comp in rel_path.components() {
        use std::path::Component;
        if matches!(comp, Component::ParentDir | Component::Prefix(_)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("refusing unsafe path in archive: {}", rel),
            ));
        }
    }
    Ok(dest.join(rel_path))
}

/// Extract members to `dest_dir`. If `filter` is non-empty, only members whose
/// path is in the filter set are extracted.
pub fn extract(
    archive_path: &str,
    dest_dir: &str,
    filter: &[String],
    force: bool,
    verbose: bool,
) -> io::Result<usize> {
    let (mut r, count) = open_archive(archive_path)?;
    let dest = Path::new(dest_dir);
    let mut extracted = 0usize;

    for _ in 0..count {
        let meta = read_entry_header(&mut r)?;

        let wanted = filter.is_empty() || filter.iter().any(|f| f == &meta.path);
        if !wanted {
            r.seek(SeekFrom::Current(meta.stored_size as i64))?;
            continue;
        }

        let mut payload = vec![0u8; meta.stored_size as usize];
        r.read_exact(&mut payload)?;
        let data = decode_payload(&meta, &payload)?;

        let actual = crc32(&data);
        if actual != meta.crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: CRC mismatch (corrupt), aborting", meta.path),
            ));
        }

        let out_path = safe_join(dest, &meta.path)?;
        if out_path.exists() && !force {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists (use -f to overwrite)", out_path.display()),
            ));
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&out_path, &data)?;
        restore_mode(&out_path, meta.mode);

        extracted += 1;
        if verbose {
            eprintln!("  extracted: {}", meta.path);
        }
    }

    Ok(extracted)
}

/// Extract member payloads to a writer (e.g. stdout), concatenated in archive
/// order. If `filter` is non-empty, only the named members are written. CRC is
/// still verified for every emitted member.
pub fn extract_to_writer<W: Write>(
    archive_path: &str,
    out: &mut W,
    filter: &[String],
) -> io::Result<usize> {
    let (mut r, count) = open_archive(archive_path)?;
    let mut extracted = 0usize;

    for _ in 0..count {
        let meta = read_entry_header(&mut r)?;

        let wanted = filter.is_empty() || filter.iter().any(|f| f == &meta.path);
        if !wanted {
            r.seek(SeekFrom::Current(meta.stored_size as i64))?;
            continue;
        }

        let mut payload = vec![0u8; meta.stored_size as usize];
        r.read_exact(&mut payload)?;
        let data = decode_payload(&meta, &payload)?;

        if crc32(&data) != meta.crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: CRC mismatch (corrupt), aborting", meta.path),
            ));
        }

        out.write_all(&data)?;
        extracted += 1;
    }

    out.flush()?;
    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        d.push(format!("bounce_test_{}_{}", tag, nanos));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn crc32_known_vector() {
        // CRC-32/IEEE of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn create_list_test_extract_roundtrip() {
        let dir = tmp_dir("rt");
        let src = dir.join("src");
        fs::create_dir_all(src.join("nested")).unwrap();

        let text: String = (0..2000)
            .map(|i| format!("compressible line {}\n", i % 50))
            .collect();
        fs::write(src.join("a.txt"), text.as_bytes()).unwrap();
        fs::write(src.join("nested/b.bin"), vec![7u8; 40_000]).unwrap();
        fs::write(src.join("empty"), b"").unwrap();

        let archive = dir.join("out.bnc");
        let inputs = vec![src.to_string_lossy().into_owned()];
        let stats = create(archive.to_str().unwrap(), &inputs, false).unwrap();
        assert_eq!(stats.files, 3);

        let entries = list_entries(archive.to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 3);

        let verified = test(archive.to_str().unwrap(), false).unwrap();
        assert_eq!(verified, 3);

        let dest = dir.join("restored");
        let n = extract(
            archive.to_str().unwrap(),
            dest.to_str().unwrap(),
            &[],
            false,
            false,
        )
        .unwrap();
        assert_eq!(n, 3);

        let restored_a = fs::read(dest.join("src/a.txt")).unwrap();
        assert_eq!(restored_a, text.as_bytes());
        let restored_b = fs::read(dest.join("src/nested/b.bin")).unwrap();
        assert_eq!(restored_b, vec![7u8; 40_000]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_traversal() {
        let dest = Path::new("/tmp/whatever");
        assert!(safe_join(dest, "../etc/passwd").is_err());
        assert!(safe_join(dest, "/etc/passwd").is_err());
        assert!(safe_join(dest, "ok/sub/file.txt").is_ok());
    }
}
