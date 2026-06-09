# bounce

![Build Status](https://img.shields.io/badge/build-passing-brightgreen)
![License](https://img.shields.io/badge/license-Apache%202.0-blue)
![Rust](https://img.shields.io/badge/rust-1.70%2B-orange)

**bounce** is a fast, zero-dependency file archiver written in pure Rust. Built on the **Big Bounce** codec with dynamic method routing, multi-threading, and optimized byte-shuffling for large binary payloads. It is designed for maximum speed and safety.

## Key Features

- **Zero External Dependencies**: Pure Rust implementation using only the standard library.
- **Cross-Platform**: Fully supports macOS, Linux, and Windows (x86_64, aarch64, x86).
- **Multi-Threaded Pipelining**: Concurrent I/O and CPU execution. While the disk reads the next chunk, CPU threads decode the current payload.
- **Smart Routing**: Analyzes file entropy and automatically selects the optimal compression strategy (LZ77, Huffman, Byte-Shuffling, or raw storage).
- **Security First**: Built-in protection against path traversal attacks. Every file is strictly verified via CRC-32 checksums.
- **Pre-flight Disk Checks**: Analyzes available disk space via native OS APIs before operations begin to prevent out-of-space crashes.
- Archive file extension: **`.bnc`**.

## Primary Use Cases

`bounce` is engineered to handle massive, dense binary files where traditional archivers struggle with either speed or memory overhead.

- **Large Language Models (LLMs) & Neural Networks**: Excels at compressing and decompressing gigabyte-sized AI weights (`.safetensors`, `.gguf`, `.pt`, `.bin`). The byte-shuffle transform effortlessly aligns `float16`/`float32` structures, delivering extreme decompression speeds (~1.3 GB/s) critical for rapid model-loading pipelines.
- **High-Performance Computing (HPC) & Big Data**: Rapid archiving of binary datasets, memory dumps, and telemetry data where maximizing sequential disk I/O throughput is paramount.
- **Game Development & Asset Bundling**: Fast packing and unpacking of large binary asset archives (textures, geometry, audio banks) thanks to asynchronous buffer pools and zero external dependencies.

---

## Installation

### From Source

```bash
git clone https://github.com/your-repo/bounce.git
cd bounce
cargo build --release
```
The binary will be available at `target/release/bounce`.

### macOS Apple Silicon (M1/M2/M3/M4)
For maximum performance on ARM64 Macs, ensure you compile natively rather than running under Rosetta 2 emulation:

```bash
cargo build --release --target aarch64-apple-darwin
```

### System-wide Installation
```bash
cargo install --path .
```

---

## Usage

```bash
bounce <command> [options] <archive> [files...]
```

### Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `create`  | `c` | Create archive from files and directories |
| `extract` | `x` | Extract archive (completely or selected files) |
| `list`    | `l` | List archive contents |
| `test`    | `t` | Verify archive integrity (CRC-32) |

### Options

| Option | Description |
|--------|-------------|
| `-1 ... -9` | Compression level (default: `-1`). `-1` is fastest, `-9` provides maximum compression via larger search windows |
| `-o, --output <dir>` | Directory for extraction (default: current directory) |
| `-c, --stdout` | Output decompressed file(s) directly to stdout |
| `-f, --force` | Overwrite existing files during extraction |
| `-v, --verbose` | Show progress details for each file |
| `-q, --quiet` | Suppress summary line output |

### Compression Levels Example

*Effect of compression levels on a 50.7 MB highly compressed video file (`.mp4`):*

| Level | Window / Block Size | Compressed Size | Ratio |
|-------|---------------------|-----------------|-------|
| `-1` | 64 KB / 128 KB | 49.8 MB | 98.2% |
| `-2` | 128 KB / 128 KB | 49.8 MB | 98.2% |
| `-3` | 256 KB / 256 KB | 49.8 MB | 98.2% |
| `-5` | 1 MB / 1 MB | 49.0 MB | 96.5% |
| `-6` | 2 MB / 2 MB | 47.9 MB | 94.4% |
| `-7` | 4 MB / 4 MB | 47.7 MB | 94.0% |
| `-8` | 8 MB / 8 MB | 46.8 MB | 92.3% |
| `-9` | 16 MB / 16 MB | 46.8 MB | 92.1% |

### Examples

**Compress files and directories:**
```bash
bounce c backup.bnc report.pdf photos/
```

**Extract an archive to a specific directory:**
```bash
bounce x backup.bnc -o restored/
```

**List archive contents:**
```bash
bounce l backup.bnc
```

**Test archive integrity:**
```bash
bounce t backup.bnc
```

---

## Architecture & Algorithm

`bounce` implements a DEFLATE-like algorithm augmented with dynamic data restructuring and multi-threading:

1. **Smart Routing (Pre-flight Analysis)**
   Before compressing a file, `bounce` calculates its Shannon entropy, bit density, and periodicity. Based on this profile, the router dynamically selects the optimal pipeline (e.g., enabling/disabling LZ77, or selecting stride-based byte shuffling). This avoids wasting CPU cycles on incompressible data.

2. **Byte-Shuffling for Structured Data**
   For structured binary data like floating-point neural network weights (`float32`/`float16`), the codec applies a byte-shuffle transform (stride = 2 or 4). This aligns the exponent and mantissa bytes across the dataset, exposing massive structural redundancy to the entropy encoder.

3. **Block-Level Concurrency**
   Large files are chunked into independent blocks. This enables lock-free, multi-threaded compression and decompression.
   
4. **Asynchronous Pipelining**
   The extraction engine uses a multi-threaded pipeline: a background thread reads data sequentially from the disk, decodes it concurrently, and passes it via zero-copy buffer pools to the main thread for CRC verification and disk writing. This ensures the SSD and CPU are utilized simultaneously without bottlenecking each other.

> **Note on Dictionary Size:** To maintain low memory overhead and high speed, the LZ77 sliding window is capped at 64 KB. While `bounce` excels at compressing dense binary data and local redundancies, it may yield slightly lower compression ratios than tools with massive dictionaries (`zstd --long`, `xz`) on text files with highly distanced duplicates.

---

## Benchmarks

Comparative benchmarks run on an **Apple MacBook Air M4 (10 cores, 24 GB RAM, arm64)**.
Run benchmarks locally using: `bash benchmark.sh`

### Safetensors Model Weights — 450.05 MB (`model-mtp.safetensors`)
Unquantized IEEE-754 weights. Demonstrates the efficiency of the byte-shuffle transform.

| Tool | Size | Ratio | C (Compression) | D (Decompression) |
|------|-----:|------:|----------------:|------------------:|
| **bounce** | **339.3 MB** | **71.9%** | **110.7 MB/s** | **~1.3 GB/s** |
| gzip -9 | 374.3 MB | 79.3% | 18.1 MB/s | 352.9 MB/s |
| lz4 -9 | 468.5 MB | 99.3% | 189.5 MB/s | 2044.8 MB/s |
| zstd -19 | 359.4 MB | 76.2% | 12.4 MB/s | 383.5 MB/s |

### Text — 1.32 MB (Repeating Markdown Corpus)

| Tool | Size | Ratio | C (Compression) | D (Decompression) |
|------|-----:|------:|----------------:|------------------:|
| **bounce** | 408 KB | 29.6% | 20.7 MB/s | 31.8 MB/s |
| gzip -9 | 404 KB | 29.3% | 15.5 MB/s | 36.0 MB/s |
| lz4 -9 | 456 KB | 33.1% | 20.4 MB/s | 36.7 MB/s |

---

## License & Patent

Distributed under the **Apache License 2.0** — see [LICENSE](LICENSE).

The dynamic task routing classification mechanism (Signal Reconstruction Resonance) is covered by US Patent Application **USA 19/452,440 (Jan 19, 2026)**. See [NOTICE](NOTICE) for details.
