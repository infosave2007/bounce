# bounce Archive Format (`.bnc`)

Format Version: **1** (magic `BNC1`).

All integers are stored in **little-endian** byte order. Each member file is compressed independently, which keeps `list`, `test`, and selective `extract` operations cheap.

## Archive Header

| Field | Type | Size (bytes) | Value |
|-------|------|--------------|-------|
| `magic` | bytes | 4 | `"BNC1"` |
| `version` | u8 | 1 | `1` |
| `entry_count` | u32 | 4 | Number of entries (files) in the archive |

## File Entry (repeated `entry_count` times)

| Field | Type | Size (bytes) | Value |
|-------|------|--------------|-------|
| `path_len` | u16 | 2 | Length of the file path in bytes |
| `path` | bytes | `path_len` | UTF-8, relative path, `/` separated |
| `mode` | u32 | 4 | File permissions (Unix permission bits) |
| `mtime` | i64 | 8 | Modification time, seconds since UNIX epoch |
| `orig_size` | u64 | 8 | Original (uncompressed) size of the file |
| `stored_size` | u64 | 8 | Stored (payload) size on disk |
| `method` | u8 | 1 | Compression method identifier |
| `stored_raw` | u8 | 1 | `1` if data is stored as raw bytes (no compression), otherwise `0` |
| `crc32` | u32 | 4 | CRC-32 (IEEE) checksum of the original bytes |
| `payload` | bytes | `stored_size` | Compressed or raw file data |

## Compression Methods (`method`)

| ID | Name | Description |
|----|------|-------------|
| 0 | `plain` | LZ77 + Huffman (single-threaded) |
| 1 | `blocked` | Block-wise (32 KB), separate Huffman tree per block (parallelized) |
| 2 | `shuf+defl` | Byte-shuffle (stride 4) + `plain` |
| 3 | `shuf+blk` | Byte-shuffle (stride 4) + `blocked` |
| 4 | `shuf2+defl` | Byte-shuffle (stride 2) + `plain` |
| 5 | `shuf2+blk` | Byte-shuffle (stride 2) + `blocked` |

If `stored_raw = 1`, the `method` field is ignored and the `payload` contains the original unmodified bytes of the file.

## Integrity and Security

During extraction (`extract`) and verification (`test`), the CRC-32 checksum is recalculated for each file and compared with the stored value. A mismatch causes the operation to abort with a non-zero exit code.
File paths are strictly validated to prevent path traversal vulnerability (absolute paths and components like `..` are rejected), preventing files from being written outside the target directory.
