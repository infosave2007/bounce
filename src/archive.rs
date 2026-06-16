// Bounce archive container (.bnc)
//
// A simple, sequential multi-file container. Each member file is compressed
// independently with the Big Bounce smart codec, which keeps `list`, `test`
// and selective `extract` cheap (no need to inflate unrelated members).

#![allow(clippy::needless_range_loop)]
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
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
    pub offset: u64,
}

struct CDEntry {
    path: String,
    offset: u64,
    orig_size: u64,
    stored_size: u64,
    crc: u32,
    method: u8,
    stored_raw: bool,
    mode: u32,
    mtime: i64,
}

fn pack_cd_crc(crc: u32, method: u8, stored_raw: bool, mode: u32) -> u64 {
    (crc as u64)
        | ((method as u64) << 32)
        | ((stored_raw as u64) << 40)
        | (((mode & 0xFFFF) as u64) << 48)
}

fn unpack_cd_crc(val: u64) -> (u32, u8, bool, u32) {
    let crc = val as u32;
    let method = (val >> 32) as u8;
    let stored_raw = ((val >> 40) & 1) != 0;
    let mode = ((val >> 48) & 0xFFFF) as u32;
    (crc, method, stored_raw, mode)
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

// ── Metadata & Disk helpers ────────────────────────────────────────────────────────

#[cfg(unix)]
fn get_free_space(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
        .ok()?;
    
    if !output.status.success() {
        return None;
    }
    
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    lines.next()?; // Skip header
    let data_line = lines.next()?;
    
    let parts: Vec<&str> = data_line.split_whitespace().collect();
    if parts.len() >= 4 {
        // POSIX df -k format: Filesystem 1024-blocks Used Available Capacity Mounted on
        let avail_1k = parts[3].parse::<u64>().ok()?;
        return Some(avail_1k * 1024);
    }
    None
}

#[cfg(windows)]
fn get_free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    
    let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    path_wide.push(0);

    extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let mut free_bytes = 0u64;
    let res = unsafe {
        GetDiskFreeSpaceExW(
            path_wide.as_ptr(),
            &mut free_bytes,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };

    if res != 0 {
        Some(free_bytes)
    } else {
        None
    }
}

#[cfg(not(any(unix, windows)))]
fn get_free_space(_path: &Path) -> Option<u64> {
    None
}

fn format_duration(secs: u64) -> String {
    let m = secs / 60;
    let s = secs % 60;
    if m > 0 { format!("{:02}:{:02}", m, s) } else { format!("{}s", s) }
}

fn print_progress(
    action: &str,
    file_name: &str,
    bytes_processed: u64,
    file_size: u64,
    stored_size: u64,
    start_time: std::time::Instant,
) {
    let elapsed = start_time.elapsed().as_secs_f64();
    if elapsed < 0.1 || bytes_processed == 0 { return; }

    let speed = bytes_processed as f64 / elapsed;
    let speed_mb = speed / 1_048_576.0;
    
    let eta_secs = if speed > 0.0 && file_size > bytes_processed {
        ((file_size - bytes_processed) as f64 / speed) as u64
    } else {
        0
    };
    
    let pct = if file_size == 0 { 0.0 } else { bytes_processed as f64 / file_size as f64 * 100.0 };
    let ratio = if bytes_processed == 0 { 0.0 } else { stored_size as f64 / bytes_processed as f64 * 100.0 };
    
    if stored_size > 0 {
        eprint!(
            "\r\x1b[K  {} {}: {:.1} / {:.1} MB ({:.1}%) | {:.1} MB/s | Ratio: {:.1}% | ETA: {}",
            action,
            file_name,
            bytes_processed as f64 / 1_048_576.0,
            file_size as f64 / 1_048_576.0,
            pct,
            speed_mb,
            ratio,
            format_duration(eta_secs)
        );
    } else {
        eprint!(
            "\r\x1b[K  {} {}: {:.1} / {:.1} MB ({:.1}%) | {:.1} MB/s | ETA: {}",
            action,
            file_name,
            bytes_processed as f64 / 1_048_576.0,
            file_size as f64 / 1_048_576.0,
            pct,
            speed_mb,
            format_duration(eta_secs)
        );
    }
    let _ = std::io::stderr().flush();
}
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

fn compress_stream_blocked<R: Read, W: Write, T: codec::TableIndex>(
    reader: &mut R,
    w: &mut W,
    window_size: usize,
    block_size: usize,
    format_version: u8,
    crc: &mut u32,
    orig_size: &mut u64,
    num_blocks: &mut u32,
    show_progress: bool,
    file_name: &str,
    file_size: u64,
) -> io::Result<()> {
    let start_time = std::time::Instant::now();
    let mut stored_size = 0u64;
    let (hash_bits, hash_size) = codec::get_hash_params(window_size);

    let concurrency = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let batch_size = concurrency * 4;

    loop {
        let mut batch = Vec::with_capacity(batch_size);
        for _ in 0..batch_size {
            let mut buffer = vec![0u8; block_size];
            let mut block_len = 0;
            while block_len < block_size {
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
            buffer.truncate(block_len);
            *crc = crc32_update(*crc, &buffer);
            *orig_size += block_len as u64;
            batch.push(buffer);
        }

        if batch.is_empty() {
            break;
        }

        let compressed_batch = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(batch.len());
            for block in &batch {
                handles.push(s.spawn(move || {
                    let mut head = vec![T::SENTINEL; hash_size];
                    let mut prev = vec![T::SENTINEL; window_size];
                    let mut buffers = codec::CompressBuffers::new();
                    let c_opt = codec::deflate_style_encode_with_buffers(block, &mut head, &mut prev, &mut buffers, window_size, format_version, 0, hash_bits);
                    codec::encode_block_result(block, c_opt, 0, format_version)
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });

        for res in compressed_batch {
            w.write_all(&res)?;
            stored_size += res.len() as u64;
            *num_blocks += 1;
        }

        if show_progress {
            print_progress("compressing", file_name, *orig_size, file_size, stored_size, start_time);
        }
    }
    if show_progress {
        eprint!("\r\x1b[K"); // Clear the line when done
    }
    Ok(())
}

fn compress_stream_shuffled_blocked<R: Read, W: Write, T: codec::TableIndex>(
    reader: &mut R,
    w: &mut W,
    file_size: u64,
    stride: usize,
    window_size: usize,
    block_size: usize,
    format_version: u8,
    crc: &mut u32,
    orig_size: &mut u64,
    num_blocks: &mut u32,
    show_progress: bool,
    file_name: &str,
) -> io::Result<()> {
    let groups = file_size / (stride as u64);
    let (hash_bits, hash_size) = codec::get_hash_params(window_size);
    let mut prev_byte = [0u8; 4];

    // Buffer for each lane before compression
    let mut lane_buffers = vec![Vec::new(); stride];
    // Buffered compressed blocks for each lane removed to avoid OOM
    // Blocks will be streamed to disk

    let concurrency = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let batch_multiplier = concurrency * 4;
    let mut chunk = vec![0u8; block_size * stride * batch_multiplier];

    let start_time = std::time::Instant::now();
    let mut stored_size = 0u64;
    let mut bytes_processed = 0;
    loop {
        // Read up to chunk.len() bytes
        let mut chunk_len = 0;
        while chunk_len < chunk.len() {
            match reader.read(&mut chunk[chunk_len..]) {
                Ok(0) => break,
                Ok(n) => chunk_len += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if chunk_len == 0 {
            break;
        }

        *crc = crc32_update(*crc, &chunk[..chunk_len]);

        // Distribute to lanes
        let mut i = 0;
        while i < chunk_len {
            let curr_file_pos = bytes_processed as u64 + i as u64;
            let stride_u64 = stride as u64;
            if curr_file_pos >= stride_u64 * groups {
                // Remainder bytes are raw-copied to the last lane's buffer
                lane_buffers[stride - 1].push(chunk[i]);
                i += 1;
            } else {
                // Distribute groups of stride bytes
                let limit = std::cmp::min((chunk_len - i) as u64, stride_u64 * groups - curr_file_pos) as usize;
                let num_groups = limit / stride;
                for g in 0..num_groups {
                    for s in 0..stride {
                        if format_version >= 3 && lane_buffers[s].len() % block_size == 0 {
                            prev_byte[s] = 0;
                        }
                        let b = chunk[i + g * stride + s];
                        let delta = b.wrapping_sub(prev_byte[s]);
                        lane_buffers[s].push(delta);
                        prev_byte[s] = b;
                    }
                }
                i += num_groups * stride;
            }
        }
        bytes_processed += chunk_len;

        // Prepare blocks to compress
        let mut blocks_to_compress = Vec::new();
        for s in 0..stride {
            let num_blocks_lane = lane_buffers[s].len() / block_size;
            for b in 0..num_blocks_lane {
                let block_data = lane_buffers[s][b * block_size .. (b + 1) * block_size].to_vec();
                blocks_to_compress.push((s, block_data));
            }
            lane_buffers[s].drain(0 .. num_blocks_lane * block_size);
        }

        if !blocks_to_compress.is_empty() {
            let compressed_batch = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(blocks_to_compress.len());
                for (lane, block) in &blocks_to_compress {
                    handles.push(scope.spawn(move || {
                        let mut head = vec![T::SENTINEL; hash_size];
                        let mut prev = vec![T::SENTINEL; window_size];
                        let mut buffers = codec::CompressBuffers::new();
                        let c_opt = codec::deflate_style_encode_with_buffers(block, &mut head, &mut prev, &mut buffers, window_size, format_version, 0, hash_bits);
                        let res = codec::encode_block_result(block, c_opt, *lane, format_version);
                        (*lane, res)
                    }));
                }
                handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
            });

            for (_lane, res) in compressed_batch {
                stored_size += res.len() as u64;
                w.write_all(&res)?;
                *num_blocks += 1;
            }
        }

        if show_progress {
            print_progress("compressing", file_name, bytes_processed as u64, file_size, stored_size, start_time);
        }
    }

    // Compress any remaining bytes in lane buffers
    let mut final_blocks = Vec::new();
    for s in 0..stride {
        if !lane_buffers[s].is_empty() {
            final_blocks.push((s, lane_buffers[s].clone()));
        }
    }

    if !final_blocks.is_empty() {
        let final_batch = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(final_blocks.len());
            for (lane, block) in &final_blocks {
                handles.push(scope.spawn(move || {
                    let mut head = vec![T::SENTINEL; hash_size];
                    let mut prev = vec![T::SENTINEL; window_size];
                    let mut buffers = codec::CompressBuffers::new();
                    let c_opt = codec::deflate_style_encode_with_buffers(block, &mut head, &mut prev, &mut buffers, window_size, format_version, 0, hash_bits);
                    let res = codec::encode_block_result(block, c_opt, *lane, format_version);
                    (*lane, res)
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });

        for (_lane, res) in final_batch {
            w.write_all(&res)?;
            *num_blocks += 1;
        }
    }

    if show_progress {
        eprint!("\r\x1b[K"); // Clear the line when done
    }

    // Blocks are already written to disk.
    *orig_size = file_size;
    Ok(())
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
    level: u8,
    verbose: bool,
    show_progress: bool,
) -> io::Result<CreateStats> {
    let files = collect_inputs(inputs)?;
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no regular files to archive",
        ));
    }

    let mut total_size = 0u64;
    for f in &files {
        if let Ok(meta) = fs::metadata(&f.abs) {
            total_size += meta.len();
        }
    }
    
    // Assume worst case: no compression + 1% format overhead + 1MB
    let required_space = total_size + (total_size / 100) + 1_048_576;
    let archive_dir = Path::new(archive_path).parent().unwrap_or_else(|| Path::new("."));
    if let Some(free_space) = get_free_space(archive_dir) {
        if free_space < required_space {
            return Err(io::Error::other(
                format!("Not enough free disk space for archive. Required: {:.1} MB, Free: {:.1} MB", required_space as f64 / 1_048_576.0, free_space as f64 / 1_048_576.0),
            ));
        }
    }

    let format_version = 1;
    let window_size = if level == 1 {
        65536
    } else {
        32768usize.checked_shl(level as u32).unwrap_or(usize::MAX / 2)
    };
    // Level 1 uses 256 KB blocks: at 128 KB the per-block Huffman tables cost ~1 pp
    // on text (hundreds of trees over a large file); 256 KB roughly halves that
    // overhead for only ~5% more time, while staying fully parallel.
    let block_size = if level == 1 {
        262144
    } else {
        65536usize.checked_shl(level as u32).unwrap_or(usize::MAX / 2)
    };

    let out_file = fs::File::create(archive_path)?;
    let mut w = BufWriter::with_capacity(2 * 1024 * 1024, out_file);

    w.write_all(MAGIC)?;
    w.write_all(&[format_version])?;
    w.write_all(&(files.len() as u32).to_le_bytes())?;

    let mut stats = CreateStats {
        files: 0,
        orig_total: 0,
        stored_total: 0,
    };

    let concurrency = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let mut cd_entries = Vec::with_capacity(files.len());

    let chunks = files.chunks(concurrency);
    for chunk in chunks {
        // Compress small files in parallel
        let compressed_results = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for f in chunk {
                let meta = fs::metadata(&f.abs)?;
                let file_size = meta.len();
                if file_size < 128 * 1024 * 1024 {
                    let abs_path = f.abs.clone();
                    let handle = s.spawn(move || -> io::Result<Option<(Vec<u8>, Vec<u8>, u32, u8, bool)>> {
                        let data = fs::read(&abs_path)?;
                        let crc = crc32(&data);
                        let (payload, method, stored_raw): (Vec<u8>, u8, bool) =
                            match codec::smart_compress_with_version(&data, window_size, block_size, format_version) {
                                Some((c, m)) if c.len() < data.len() => (c, m.to_u8(), false),
                                _ => (data.clone(), codec::CompressMethod::Plain.to_u8(), true),
                            };
                        Ok(Some((data, payload, crc, method, stored_raw)))
                    });
                    handles.push(Some(handle));
                } else {
                    handles.push(None);
                }
            }

            let mut results = Vec::new();
            for h_opt in handles {
                if let Some(h) = h_opt {
                    results.push(h.join().unwrap()?);
                } else {
                    results.push(None);
                }
            }
            Ok::<_, io::Error>(results)
        })?;

        // Write sequentially
        for (i, f) in chunk.iter().enumerate() {
            let meta = fs::metadata(&f.abs)?;
            let file_size = meta.len();
            let mode = file_mode(&meta);
            let mtime = file_mtime(&meta);

            let offset = w.stream_position()?;

            if let Some((data, payload, crc, method, stored_raw)) = &compressed_results[i] {
                let path_bytes = f.rel.as_bytes();
                w.write_all(&(path_bytes.len() as u16).to_le_bytes())?;
                w.write_all(path_bytes)?;
                w.write_all(&mode.to_le_bytes())?;
                w.write_all(&mtime.to_le_bytes())?;
                w.write_all(&(data.len() as u64).to_le_bytes())?;
                w.write_all(&(payload.len() as u64).to_le_bytes())?;
                w.write_all(&[*method])?;
                w.write_all(&[*stored_raw as u8])?;
                w.write_all(&crc.to_le_bytes())?;
                w.write_all(payload)?;

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

                cd_entries.push(CDEntry {
                    path: f.rel.clone(),
                    offset,
                    orig_size: data.len() as u64,
                    stored_size: payload.len() as u64,
                    crc: *crc,
                    method: *method,
                    stored_raw: *stored_raw,
                    mode,
                    mtime,
                });
            } else {
                // Large file path: sequential block compression
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
                let sample_size = std::cmp::min(file_size - sample_offset, 1024 * 1024) as usize;
                let mut sample = vec![0u8; sample_size];
                file.seek(SeekFrom::Start(sample_offset))?;
                file.read_exact(&mut sample)?;
                file.seek(SeekFrom::Start(0))?;

                let method = match codec::smart_compress_with_version(&sample, window_size, block_size, format_version) {
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

                // Cap block size per method family (see codec::MAX_*_BLOCK_SIZE).
                // Self-describing block headers keep this transparent to the decoder.
                let plain_block = block_size.min(codec::MAX_PLAIN_BLOCK_SIZE);
                let shuffle_block = block_size.min(codec::MAX_SHUFFLE_BLOCK_SIZE);

                if window_size <= 65536 {
                    if method == codec::CompressMethod::Blocked {
                        let mut reader = BufReader::with_capacity(2 * 1024 * 1024, file);
                        compress_stream_blocked::<_, _, u16>(
                            &mut reader,
                            &mut w,
                            window_size,
                            plain_block,
                            format_version,
                            &mut crc,
                            &mut orig_size,
                            &mut num_blocks,
                            show_progress,
                            &f.rel,
                            file_size,
                        )?;
                    } else {
                        let stride = if method == codec::CompressMethod::ShuffleBlk { 4 } else { 2 };
                        let mut reader = BufReader::with_capacity(2 * 1024 * 1024, file);
                        compress_stream_shuffled_blocked::<_, _, u16>(
                            &mut reader,
                            &mut w,
                            file_size,
                            stride,
                            window_size,
                            shuffle_block,
                            format_version,
                            &mut crc,
                            &mut orig_size,
                            &mut num_blocks,
                            show_progress,
                            &f.rel,
                        )?;
                    }
                } else if method == codec::CompressMethod::Blocked {
                    let mut reader = BufReader::with_capacity(2 * 1024 * 1024, file);
                    compress_stream_blocked::<_, _, u32>(
                        &mut reader,
                        &mut w,
                        window_size,
                        plain_block,
                        format_version,
                        &mut crc,
                        &mut orig_size,
                        &mut num_blocks,
                        show_progress,
                        &f.rel,
                        file_size,
                    )?;
                } else {
                    let stride = if method == codec::CompressMethod::ShuffleBlk { 4 } else { 2 };
                    let mut reader = BufReader::with_capacity(2 * 1024 * 1024, file);
                    compress_stream_shuffled_blocked::<_, _, u32>(
                        &mut reader,
                        &mut w,
                        file_size,
                        stride,
                        window_size,
                        shuffle_block,
                        format_version,
                        &mut crc,
                        &mut orig_size,
                        &mut num_blocks,
                        show_progress,
                        &f.rel,
                    )?;
                }

                let payload_end = w.stream_position()?;
                let stored_size = payload_end - payload_start;

                // Rewrite num_blocks
                w.seek(SeekFrom::Start(payload_start))?;
                w.write_all(&num_blocks.to_le_bytes())?;

                // Rewrite header values
                w.seek(SeekFrom::Start(offset + 14 + path_bytes.len() as u64))?;
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

                cd_entries.push(CDEntry {
                    path: f.rel.clone(),
                    offset,
                    orig_size,
                    stored_size,
                    crc,
                    method: method.to_u8(),
                    stored_raw: false,
                    mode,
                    mtime,
                });
            }

            stats.files += 1;
        }
    }

    // Write Central Directory Index
    let cd_start_offset = w.stream_position()?;
    w.write_all(&(cd_entries.len() as u32).to_le_bytes())?;
    for entry in &cd_entries {
        let path_bytes = entry.path.as_bytes();
        w.write_all(&(path_bytes.len() as u16).to_le_bytes())?;
        w.write_all(path_bytes)?;
        w.write_all(&entry.offset.to_le_bytes())?;
        w.write_all(&entry.orig_size.to_le_bytes())?;
        w.write_all(&entry.stored_size.to_le_bytes())?;
        
        if format_version >= 2 {
            w.write_all(&entry.mtime.to_le_bytes())?;
        }

        let crc_packed = pack_cd_crc(entry.crc, entry.method, entry.stored_raw, entry.mode);
        w.write_all(&crc_packed.to_le_bytes())?;
    }
    w.write_all(&cd_start_offset.to_le_bytes())?;

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

fn open_archive(archive_path: &str) -> io::Result<(BufReader<fs::File>, u32, u8)> {
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
    if ver[0] != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported archive version {}", ver[0]),
        ));
    }
    let count = read_u32(&mut r)?;
    Ok((r, count, ver[0]))
}

fn read_entry_header<R: Read + Seek>(r: &mut R) -> io::Result<EntryMeta> {
    let offset = r.stream_position()?;
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
        offset,
    })
}

fn read_cd_index<R: Read + Seek>(
    r: &mut R,
    header_count: u32,
    file_len: u64,
    version: u8,
) -> io::Result<Option<Vec<EntryMeta>>> {
    if file_len < 9 + 8 {
        return Ok(None);
    }
    if r.seek(SeekFrom::Start(file_len - 8)).is_err() {
        return Ok(None);
    }
    let mut cd_offset_bytes = [0u8; 8];
    if r.read_exact(&mut cd_offset_bytes).is_err() {
        return Ok(None);
    }
    let cd_offset = u64::from_le_bytes(cd_offset_bytes);
    if cd_offset < 9 || cd_offset >= file_len - 8 {
        return Ok(None);
    }
    if r.seek(SeekFrom::Start(cd_offset)).is_err() {
        return Ok(None);
    }
    let mut cd_count_bytes = [0u8; 4];
    if r.read_exact(&mut cd_count_bytes).is_err() {
        return Ok(None);
    }
    let cd_count = u32::from_le_bytes(cd_count_bytes);
    if cd_count != header_count {
        return Ok(None);
    }
    let mut entries = Vec::with_capacity(cd_count as usize);
    for _ in 0..cd_count {
        let mut path_len_bytes = [0u8; 2];
        if r.read_exact(&mut path_len_bytes).is_err() {
            return Ok(None);
        }
        let path_len = u16::from_le_bytes(path_len_bytes) as usize;
        let mut path_bytes = vec![0u8; path_len];
        if r.read_exact(&mut path_bytes).is_err() {
            return Ok(None);
        }
        let path = match String::from_utf8(path_bytes) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let mut offset_bytes = [0u8; 8];
        if r.read_exact(&mut offset_bytes).is_err() {
            return Ok(None);
        }
        let offset = u64::from_le_bytes(offset_bytes);
        let mut orig_size_bytes = [0u8; 8];
        if r.read_exact(&mut orig_size_bytes).is_err() {
            return Ok(None);
        }
        let orig_size = u64::from_le_bytes(orig_size_bytes);
        let mut stored_size_bytes = [0u8; 8];
        if r.read_exact(&mut stored_size_bytes).is_err() {
            return Ok(None);
        }
        let stored_size = u64::from_le_bytes(stored_size_bytes);

        let mtime = if version >= 2 {
            let mut mtime_bytes = [0u8; 8];
            if r.read_exact(&mut mtime_bytes).is_err() {
                return Ok(None);
            }
            i64::from_le_bytes(mtime_bytes)
        } else {
            0
        };

        let mut crc_val_bytes = [0u8; 8];
        if r.read_exact(&mut crc_val_bytes).is_err() {
            return Ok(None);
        }
        let crc_val = u64::from_le_bytes(crc_val_bytes);
        let (crc, method, stored_raw, mode) = unpack_cd_crc(crc_val);
        if offset >= cd_offset {
            return Ok(None);
        }
        entries.push(EntryMeta {
            path,
            mode,
            mtime,
            orig_size,
            stored_size,
            method,
            stored_raw,
            crc,
            offset,
        });
    }
    if let Ok(pos) = r.stream_position() {
        if pos == file_len - 8 {
            return Ok(Some(entries));
        }
    }
    Ok(None)
}

/// Read every entry header (payloads skipped) for listing.
pub fn list_entries(archive_path: &str) -> io::Result<Vec<EntryMeta>> {
    let file = fs::File::open(archive_path)?;
    let file_len = file.metadata()?.len();
    let (mut r, count, version) = open_archive(archive_path)?;
    if let Ok(Some(entries)) = read_cd_index(&mut r, count, file_len, version) {
        return Ok(entries);
    }
    r.seek(SeekFrom::Start(9))?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let meta = read_entry_header(&mut r)?;
        r.seek(SeekFrom::Current(meta.stored_size as i64))?;
        out.push(meta);
    }
    Ok(out)
}

fn extract_entry_payload<R: Read + Seek + Send, W: Write>(
    r: &mut R,
    meta: &EntryMeta,
    w: &mut W,
    version: u8,
) -> io::Result<u32> {
    let method = codec::CompressMethod::from_u8(meta.method).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown codec method {}", meta.method),
        )
    })?;

    let start_pos = r.stream_position()?;

    let crc = if meta.stored_raw {
        let mut data = vec![0u8; meta.stored_size as usize];
        r.read_exact(&mut data)?;
        let c = crc32(&data);
        w.write_all(&data)?;
        c
    } else if meta.stored_size < 128 * 1024 * 1024 {
        let mut comp_data = vec![0u8; meta.stored_size as usize];
        r.read_exact(&mut comp_data)?;
        let decomp = codec::smart_decompress_with_version(&comp_data, method, meta.orig_size as usize, version)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let c = crc32(&decomp);
        w.write_all(&decomp)?;
        c
    } else {
        let mut dec = codec::DecompressReader::new(
            &mut *r,
            method,
            meta.stored_size,
            meta.orig_size,
            meta.stored_raw,
            version,
        );

        let start_time = std::time::Instant::now();
        let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<Option<Vec<u8>>>>(8);
        let (tx_pool, rx_pool) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
        for _ in 0..16 {
            tx_pool.send(vec![0u8; 1024 * 1024]).unwrap();
        }

        let res = std::thread::scope(|s| {
            let reader_thread = s.spawn(move || {
                loop {
                    let mut buf = rx_pool.recv().unwrap();
                    unsafe { buf.set_len(1024 * 1024); } // safe because it was fully initialized before
                    match dec.read(&mut buf) {
                        Ok(0) => {
                            let _ = tx.send(Ok(None));
                            break;
                        }
                        Ok(n) => {
                            buf.truncate(n);
                            if tx.send(Ok(Some(buf))).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            });

            let mut c = 0u32;
            let mut decomp_bytes = 0u64;

            for msg in rx {
                match msg {
                    Ok(Some(buf)) => {
                        c = crc32_update(c, &buf);
                        w.write_all(&buf)?;
                        decomp_bytes += buf.len() as u64;
                        print_progress("extracting", &meta.path, decomp_bytes, meta.orig_size, 0, start_time);
                        let _ = tx_pool.send(buf);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(e),
                }
            }
            eprint!("\r\x1b[K"); // Clear the line when done
            let _ = reader_thread.join();
            Ok::<_, io::Error>(c)
        });
        res?
    };

    r.seek(SeekFrom::Start(start_pos + meta.stored_size))?;
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
    let (mut r, count, version) = open_archive(archive_path)?;
    for _ in 0..count {
        let meta = read_entry_header(&mut r)?;
        let mut sink = CountingWriter {
            inner: io::sink(),
            count: 0,
        };
        let actual_crc = extract_entry_payload(&mut r, &meta, &mut sink, version)?;
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
    let file = fs::File::open(archive_path)?;
    let file_len = file.metadata()?.len();
    let (mut r, count, version) = open_archive(archive_path)?;
    let dest = Path::new(dest_dir);
    let mut extracted = 0usize;

    if let Ok(Some(entries)) = read_cd_index(&mut r, count, file_len, version) {
        let total_orig_size: u64 = entries.iter()
            .filter(|e| filter.is_empty() || filter.iter().any(|f| f == &e.path))
            .map(|e| e.orig_size)
            .sum();
        
        if let Some(free_space) = get_free_space(dest) {
            if free_space < total_orig_size {
                return Err(io::Error::other(
                    format!("Not enough free disk space for extraction. Required: {:.1} MB, Free: {:.1} MB", total_orig_size as f64 / 1_048_576.0, free_space as f64 / 1_048_576.0),
                ));
            }
        }
    }

    if !filter.is_empty() {
        if let Ok(Some(entries)) = read_cd_index(&mut r, count, file_len, version) {
            for entry in &entries {
                let wanted = filter.iter().any(|f| f == &entry.path);
                if !wanted {
                    continue;
                }

                r.seek(SeekFrom::Start(entry.offset))?;
                let meta = read_entry_header(&mut r)?;

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

                let actual_crc = extract_entry_payload(&mut r, &meta, &mut w, version)?;
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
            return Ok(extracted);
        }
    }

    r.seek(SeekFrom::Start(9))?;
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

        let actual_crc = extract_entry_payload(&mut r, &meta, &mut w, version)?;
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
    let file = fs::File::open(archive_path)?;
    let file_len = file.metadata()?.len();
    let (mut r, count, version) = open_archive(archive_path)?;
    let mut extracted = 0usize;

    if !filter.is_empty() {
        if let Ok(Some(entries)) = read_cd_index(&mut r, count, file_len, version) {
            for entry in &entries {
                let wanted = filter.iter().any(|f| f == &entry.path);
                if !wanted {
                    continue;
                }

                r.seek(SeekFrom::Start(entry.offset))?;
                let meta = read_entry_header(&mut r)?;

                let mut w = CountingWriter {
                    inner: &mut *out,
                    count: 0,
                };

                let actual_crc = extract_entry_payload(&mut r, &meta, &mut w, version)?;

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
            return Ok(extracted);
        }
    }

    r.seek(SeekFrom::Start(9))?;
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

        let actual_crc = extract_entry_payload(&mut r, &meta, &mut w, version)?;

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
        let stats = create(archive.to_str().unwrap(), &inputs, 1, false, false).unwrap();
        assert_eq!(stats.files, 3);

        // 1. Verify list entries (should read from CD)
        let entries = list_entries(archive.to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "src/a.txt");
        assert_eq!(entries[1].path, "src/empty");
        assert_eq!(entries[2].path, "src/nested/b.bin");

        // 2. Verify test (integrity scan)
        let verified = test(archive.to_str().unwrap(), false).unwrap();
        assert_eq!(verified, 3);

        // 3. Verify selective extraction (uses CD seeking)
        let dest_sel = dir.join("restored_sel");
        let n_sel = extract(
            archive.to_str().unwrap(),
            dest_sel.to_str().unwrap(),
            &["src/a.txt".to_string()],
            false,
            false,
        )
        .unwrap();
        assert_eq!(n_sel, 1);
        assert!(dest_sel.join("src/a.txt").exists());
        assert!(!dest_sel.join("src/nested/b.bin").exists());
        assert_eq!(fs::read(dest_sel.join("src/a.txt")).unwrap(), text.as_bytes());

        // 4. Verify full extraction
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

        // 5. Test backward compatibility: truncate CD index from end of file
        // Read file length and find where CD index starts
        let file_data = fs::read(&archive).unwrap();
        let len = file_data.len();
        let cd_offset = u64::from_le_bytes(file_data[len - 8..].try_into().unwrap()) as usize;
        
        // Truncate the file to exclude the CD index
        let truncated_archive = dir.join("truncated.bnc");
        fs::write(&truncated_archive, &file_data[..cd_offset]).unwrap();

        // Verify that we can still list entries successfully (via fallback)
        let entries_fallback = list_entries(truncated_archive.to_str().unwrap()).unwrap();
        assert_eq!(entries_fallback.len(), 3);
        assert_eq!(entries_fallback[0].path, "src/a.txt");

        // Verify that we can still extract from the truncated archive (via fallback)
        let dest_fallback = dir.join("restored_fallback");
        let n_fallback = extract(
            truncated_archive.to_str().unwrap(),
            dest_fallback.to_str().unwrap(),
            &["src/nested/b.bin".to_string()],
            false,
            false,
        )
        .unwrap();
        assert_eq!(n_fallback, 1);
        assert!(dest_fallback.join("src/nested/b.bin").exists());

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
