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

pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    let t = crc32_tables();
    crc ^= 0xFFFF_FFFF;
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
    let mut w = BufWriter::with_capacity(2 * 1024 * 1024, out_file);

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
        let file_size = meta.len();
        let mode = file_mode(&meta);
        let mtime = file_mtime(&meta);

        let (_orig_len, _stored_len) = if file_size < 512 * 1024 * 1024 {
            let data = fs::read(&f.abs)?;
            let crc = crc32(&data);
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
            (data.len() as u64, payload.len() as u64)
        } else {
            let header_offset = w.stream_position()?;
            let path_bytes = f.rel.as_bytes();
            w.write_all(&(path_bytes.len() as u16).to_le_bytes())?;
            w.write_all(path_bytes)?;
            w.write_all(&mode.to_le_bytes())?;
            w.write_all(&mtime.to_le_bytes())?;
            w.write_all(&0u64.to_le_bytes())?; // orig_size placeholder
            w.write_all(&0u64.to_le_bytes())?; // stored_size placeholder
            w.write_all(&[0u8])?;              // method placeholder
            w.write_all(&[0u8])?;              // stored_raw placeholder
            w.write_all(&0u32.to_le_bytes())?; // crc placeholder

            let payload_start = w.stream_position()?;
            w.write_all(&0u32.to_le_bytes())?; // num_blocks placeholder

            let mut file = fs::File::open(&f.abs)?;

            // Read 1MB sample from the middle of the file to bypass metadata headers and determine the best method
            let sample_offset = file_size / 2;
            let sample_size = std::cmp::min(file_size - sample_offset, 1 * 1024 * 1024) as usize;
            let mut sample = vec![0u8; sample_size];
            file.seek(SeekFrom::Start(sample_offset))?;
            file.read_exact(&mut sample)?;
            file.seek(SeekFrom::Start(0))?;

            let method = match codec::smart_compress(&sample) {
                Some((_, codec::CompressMethod::Shuffle)) | Some((_, codec::CompressMethod::ShuffleBlk)) => {
                    codec::CompressMethod::ShuffleBlk
                }
                Some((_, codec::CompressMethod::Shuffle2)) | Some((_, codec::CompressMethod::Shuffle2Blk)) => {
                    codec::CompressMethod::Shuffle2Blk
                }
                _ => codec::CompressMethod::Blocked,
            };

            let mut num_blocks = 0u32;
            let mut crc = 0u32;
            let mut orig_size = 0u64;

            if method == codec::CompressMethod::Blocked {
                let mut reader = BufReader::with_capacity(2 * 1024 * 1024, file);
                let mut buffer = vec![0u8; codec::BLOCK_SIZE];
                let mut head = vec![-1i32; codec::LZV2_HASH_SIZE];
                let mut prev = vec![0i32; codec::LZV2_WINDOW_SIZE];

                loop {
                    let mut block_len = 0;
                    while block_len < codec::BLOCK_SIZE {
                        match reader.read(&mut buffer[block_len..]) {
                            Ok(0) => break,
                            Ok(n) => block_len += n,
                            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(e),
                        }
                    }
                    if block_len == 0 {
                        break;
                    }

                    let block = &buffer[..block_len];
                    crc = crc32_update(crc, block);
                    orig_size += block_len as u64;

                    head.fill(-1);
                    let c_opt = codec::deflate_style_encode_with_buffers(block, &mut head, &mut prev);
                    let res = codec::encode_block_result(block, c_opt);
                    w.write_all(&res)?;
                    num_blocks += 1;
                }
            } else {
                // Compute CRC32 sequentially in 64KB chunks
                let mut crc_buf = vec![0u8; 64 * 1024];
                loop {
                    match file.read(&mut crc_buf) {
                        Ok(0) => break,
                        Ok(n) => crc = crc32_update(crc, &crc_buf[..n]),
                        Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                file.seek(SeekFrom::Start(0))?;

                let mut buffer = vec![0u8; codec::BLOCK_SIZE];
                let mut head = vec![-1i32; codec::LZV2_HASH_SIZE];
                let mut prev = vec![0i32; codec::LZV2_WINDOW_SIZE];

                let stride = if method == codec::CompressMethod::ShuffleBlk { 4 } else { 2 };
                let groups = (file_size as usize) / stride;
                let mut prev_byte = [0u8; 4];
                let mut span_buf = Vec::new();

                let num_blocks_expected = ((file_size + codec::BLOCK_SIZE as u64 - 1) / codec::BLOCK_SIZE as u64) as u32;

                for b_idx_exp in 0..num_blocks_expected {
                    let start_byte = b_idx_exp as usize * codec::BLOCK_SIZE;
                    let end_byte = std::cmp::min((b_idx_exp as usize + 1) * codec::BLOCK_SIZE, file_size as usize);
                    let block_len = end_byte - start_byte;

                    let mut curr_i = start_byte;
                    let mut buf_idx = 0;

                    while curr_i < end_byte {
                        if curr_i >= stride * groups {
                            let rem_len = end_byte - curr_i;
                            file.seek(SeekFrom::Start(curr_i as u64))?;
                            file.read_exact(&mut buffer[buf_idx..buf_idx + rem_len])?;
                            buf_idx += rem_len;
                            curr_i += rem_len;
                        } else {
                            let s = curr_i / groups;
                            let lane_end_i = std::cmp::min((s + 1) * groups, end_byte);
                            let chunk_len = lane_end_i - curr_i;

                            let g_start = curr_i % groups;
                            let orig_start_offset = g_start * stride + s;
                            let orig_span_len = (chunk_len - 1) * stride + 1;

                            if span_buf.len() < orig_span_len {
                                span_buf.resize(orig_span_len, 0);
                            }
                            file.seek(SeekFrom::Start(orig_start_offset as u64))?;
                            file.read_exact(&mut span_buf[..orig_span_len])?;

                            for j in 0..chunk_len {
                                let val = span_buf[j * stride];
                                let delta = val.wrapping_sub(prev_byte[s]);
                                buffer[buf_idx] = delta;
                                prev_byte[s] = val;
                                buf_idx += 1;
                            }
                            curr_i = lane_end_i;
                        }
                    }

                    let block = &buffer[..block_len];
                    orig_size += block_len as u64;

                    head.fill(-1);
                    let c_opt = codec::deflate_style_encode_with_buffers(block, &mut head, &mut prev);
                    let res = codec::encode_block_result(block, c_opt);
                    w.write_all(&res)?;
                    num_blocks += 1;
                }
            }

            let payload_end = w.stream_position()?;
            let stored_size = payload_end - payload_start;

            // Rewrite num_blocks
            w.seek(SeekFrom::Start(payload_start))?;
            w.write_all(&num_blocks.to_le_bytes())?;

            // Rewrite header values
            w.seek(SeekFrom::Start(header_offset + 14 + path_bytes.len() as u64))?;
            w.write_all(&orig_size.to_le_bytes())?;
            w.write_all(&stored_size.to_le_bytes())?;
            w.write_all(&[method.to_u8()])?;
            w.write_all(&[0u8])?; // stored_raw = false
            w.write_all(&crc.to_le_bytes())?;

            w.seek(SeekFrom::Start(payload_end))?;

            stats.orig_total += orig_size;
            stats.stored_total += stored_size;

            if verbose {
                let pct = if orig_size == 0 {
                    0.0
                } else {
                    stored_size as f64 / orig_size as f64 * 100.0
                };
                eprintln!(
                    "  adding: {}  ({} -> {} bytes, {:.1}%)",
                    f.rel,
                    orig_size,
                    stored_size,
                    pct
                );
            }
            (orig_size, stored_size)
        };

        stats.files += 1;
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
    let mut r = BufReader::with_capacity(2 * 1024 * 1024, file);

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

fn load_compressed_block<R: Read + Seek>(
    r: &mut R,
    block_index: usize,
    block_headers: &[(usize, usize, u8)],
    block_offsets: &[u64],
    comp_buf: &mut Vec<u8>,
) -> io::Result<Vec<u8>> {
    let (comp_size, orig_size, flag) = block_headers[block_index];
    let offset = block_offsets[block_index];
    r.seek(SeekFrom::Start(offset + 9))?;
    if comp_buf.len() < comp_size {
        comp_buf.resize(comp_size, 0);
    }
    r.read_exact(&mut comp_buf[..comp_size])?;
    if flag == 1 {
        codec::deflate_style_decode(&comp_buf[..comp_size], orig_size)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    } else {
        Ok(comp_buf[..comp_size].to_vec())
    }
}


fn extract_shuffled_blocked<R: Read + Seek, W: Write>(
    r: &mut R,
    meta: &EntryMeta,
    w: &mut W,
    method: codec::CompressMethod,
) -> io::Result<u32> {
    let mut crc = 0u32;
    let num_blocks = read_u32(r)? as usize;
    let mut block_offsets = Vec::with_capacity(num_blocks);
    let mut block_headers = Vec::with_capacity(num_blocks);

    let start_pos = r.stream_position()?;
    let mut curr_pos = start_pos;

    let mut header_buf = [0u8; 9];
    for _ in 0..num_blocks {
        block_offsets.push(curr_pos);
        r.read_exact(&mut header_buf)?;
        let comp_size = u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]) as usize;
        let orig_size = u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]) as usize;
        let flag = header_buf[8];
        block_headers.push((comp_size, orig_size, flag));

        r.seek(SeekFrom::Current(comp_size as i64))?;
        curr_pos += 9 + comp_size as u64;
    }

    let stride = if method == codec::CompressMethod::ShuffleBlk { 4 } else { 2 };
    let groups = (meta.orig_size as usize) / stride;
    let mut prev_byte = [0u8; 4];
    let mut active_blocks = vec![None; stride];
    let mut comp_buf = Vec::new();

    let mut out_buf = vec![0u8; codec::BLOCK_SIZE];
    let mut out_idx = 0;

    for g in 0..groups {
        for s in 0..stride {
            let idx = s * groups + g;
            let b_idx = idx / codec::BLOCK_SIZE;
            let offset_in_block = idx % codec::BLOCK_SIZE;

            let cached_valid = match &active_blocks[s] {
                Some((cached_b, _)) => *cached_b == b_idx,
                None => false,
            };

            if !cached_valid {
                let data = load_compressed_block(r, b_idx, &block_headers, &block_offsets, &mut comp_buf)?;
                active_blocks[s] = Some((b_idx, data));
            }

            let block_data = &active_blocks[s].as_ref().unwrap().1;
            let byte = block_data[offset_in_block];
            let val = byte.wrapping_add(prev_byte[s]);
            prev_byte[s] = val;

            out_buf[out_idx] = val;
            out_idx += 1;
            if out_idx == codec::BLOCK_SIZE {
                crc = crc32_update(crc, &out_buf);
                w.write_all(&out_buf)?;
                out_idx = 0;
            }
        }
    }

    for idx in (stride * groups)..(meta.orig_size as usize) {
        let b_idx = idx / codec::BLOCK_SIZE;
        let offset_in_block = idx % codec::BLOCK_SIZE;

        let cached_valid = match &active_blocks[0] {
            Some((cached_b, _)) => *cached_b == b_idx,
            None => false,
        };

        if !cached_valid {
            let data = load_compressed_block(r, b_idx, &block_headers, &block_offsets, &mut comp_buf)?;
            active_blocks[0] = Some((b_idx, data));
        }

        let block_data = &active_blocks[0].as_ref().unwrap().1;
        let val = block_data[offset_in_block];

        out_buf[out_idx] = val;
        out_idx += 1;
        if out_idx == codec::BLOCK_SIZE {
            crc = crc32_update(crc, &out_buf);
            w.write_all(&out_buf)?;
            out_idx = 0;
        }
    }

    if out_idx > 0 {
        crc = crc32_update(crc, &out_buf[..out_idx]);
        w.write_all(&out_buf[..out_idx])?;
    }

    // Seek the reader to position after the entry's payload
    r.seek(SeekFrom::Start(start_pos - 4 + meta.stored_size))?;
    Ok(crc)
}

fn extract_entry_payload<R: Read + Seek, W: Write>(
    r: &mut R,
    meta: &EntryMeta,
    w: &mut W,
) -> io::Result<u32> {
    let mut crc = 0u32;

    if meta.stored_raw {
        let mut remaining = meta.stored_size;
        let mut buffer = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let to_read = std::cmp::min(remaining, buffer.len() as u64) as usize;
            r.read_exact(&mut buffer[..to_read])?;
            let chunk = &buffer[..to_read];
            crc = crc32_update(crc, chunk);
            w.write_all(chunk)?;
            remaining -= to_read as u64;
        }
        return Ok(crc);
    }

    if meta.stored_size < 512 * 1024 * 1024 {
        // High-speed path: read compressed payload into RAM and decompress in parallel using multiple CPU cores
        let mut payload = vec![0u8; meta.stored_size as usize];
        r.read_exact(&mut payload)?;
        let data = decode_payload(meta, &payload)?;
        crc = crc32(&data);
        w.write_all(&data)?;
        return Ok(crc);
    }

    // Streaming path (OOM protection for >= 512MB files)
    let method = codec::CompressMethod::from_u8(meta.method).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown codec method {}", meta.method),
        )
    })?;

    match method {
        codec::CompressMethod::Blocked => {
            let num_blocks = read_u32(r)? as usize;
            let mut comp_buf = Vec::new();

            let mut header = [0u8; 9];
            for _ in 0..num_blocks {
                r.read_exact(&mut header)?;
                let comp_size = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
                let orig_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
                let flag = header[8];

                if comp_buf.len() < comp_size {
                    comp_buf.resize(comp_size, 0);
                }
                r.read_exact(&mut comp_buf[..comp_size])?;

                if flag == 1 {
                    let decomp_vec = codec::deflate_style_decode(&comp_buf[..comp_size], orig_size)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    crc = crc32_update(crc, &decomp_vec);
                    w.write_all(&decomp_vec)?;
                } else {
                    let decomp = &comp_buf[..comp_size];
                    crc = crc32_update(crc, decomp);
                    w.write_all(decomp)?;
                }
            }
        }
        codec::CompressMethod::ShuffleBlk | codec::CompressMethod::Shuffle2Blk => {
            crc = extract_shuffled_blocked(r, meta, w, method)?;
        }
        _ => {
            let mut payload = vec![0u8; meta.stored_size as usize];
            r.read_exact(&mut payload)?;
            let data = decode_payload(meta, &payload)?;
            crc = crc32(&data);
            w.write_all(&data)?;
        }
    }

    Ok(crc)
}

struct CountingWriter<W> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner.write_all(buf)?;
        self.count += buf.len() as u64;
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Verify CRC of every member. Returns the number of files checked.
pub fn test(archive_path: &str, verbose: bool) -> io::Result<usize> {
    let (mut r, count) = open_archive(archive_path)?;
    for _ in 0..count {
        let meta = read_entry_header(&mut r)?;
        let mut sink = CountingWriter {
            inner: io::sink(),
            count: 0,
        };
        let actual_crc = extract_entry_payload(&mut r, &meta, &mut sink)?;
        if sink.count != meta.orig_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: size mismatch ({} != {})",
                    meta.path,
                    sink.count,
                    meta.orig_size
                ),
            ));
        }
        if actual_crc != meta.crc {
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

        let out_file = fs::File::create(&out_path)?;
        let mut w = CountingWriter {
            inner: BufWriter::with_capacity(2 * 1024 * 1024, out_file),
            count: 0,
        };

        let actual_crc = extract_entry_payload(&mut r, &meta, &mut w)?;
        w.flush()?;

        if w.count != meta.orig_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: size mismatch ({} != {})", meta.path, w.count, meta.orig_size),
            ));
        }

        if actual_crc != meta.crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: CRC mismatch (corrupt), aborting", meta.path),
            ));
        }

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

        let mut w = CountingWriter {
            inner: &mut *out,
            count: 0,
        };

        let actual_crc = extract_entry_payload(&mut r, &meta, &mut w)?;

        if w.count != meta.orig_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: size mismatch ({} != {})", meta.path, w.count, meta.orig_size),
            ));
        }

        if actual_crc != meta.crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: CRC mismatch (corrupt), aborting", meta.path),
            ));
        }

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
