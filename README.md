# bounce

<p align="center">
  <img src="assets/logo.png" alt="bounce cosmic logo" width="400">
</p>

[![Crates.io](https://img.shields.io/crates/v/nvg-bounce.svg)](https://crates.io/crates/nvg-bounce)
[![CI](https://github.com/infosave2007/bounce/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/infosave2007/bounce/actions/workflows/ci.yml)
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

- **Large Language Models (LLMs) & Neural Networks**: Excels at compressing and decompressing gigabyte-sized AI weights (`.safetensors`, `.gguf`, `.pt`, `.bin`). The byte-shuffle transform effortlessly aligns `float16`/`float32` structures, delivering extreme decompression speeds (~1.1 GB/s) critical for rapid model-loading pipelines.
- **High-Performance Computing (HPC) & Big Data**: Rapid archiving of binary datasets, memory dumps, and telemetry data where maximizing sequential disk I/O throughput is paramount.
- **Game Development & Asset Bundling**: Fast packing and unpacking of large binary asset archives (textures, geometry, audio banks) thanks to asynchronous buffer pools and zero external dependencies.

## FAQ & Known Limitations

**When should I NOT use `bounce`?**
While `bounce` is highly optimized for specific payloads, it is not a silver bullet. You should stick to traditional archivers (like `tar.gz`, `zstd`, or `7z`) if:
- **Your data is already compressed:** Files like `.mp4`, `.jpg`, `.gz`, or `.zip` have maximum entropy. `bounce` will detect this and fall back to raw storage to save CPU cycles, but you won't gain any compression.
- **You are compressing huge text corpora:** For massive source code repositories or pure text logs, tools with massive dictionaries (`zstd --long`, `xz`) will achieve better ratios. `bounce` limits its LZ77 sliding window to 64 KB to keep memory overhead near zero and decompression speeds astronomical.
- **You need extreme compression ratios regardless of speed:** `bounce` prioritizes I/O throughput (speed) over squeezing out the absolute last byte.

---

## Installation

### 1. Pre-compiled Binaries (Recommended)
You can download pre-compiled binaries for macOS, Linux, and Windows from the [GitHub Releases](../../releases) page. No Rust installation is required.

### 2. Via Cargo (crates.io)
If you have the Rust toolchain installed, you can install `bounce` directly from crates.io:
```bash
cargo install nvg-bounce
```
*(Note: The installed binary will be available as `bounce` in your terminal)*

### 3. Build from Source
To build the latest development version:
```bash
git clone https://github.com/infosave2007/bounce.git
cd bounce
cargo build --release
# The binary will be at target/release/bounce
```

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
| `-1 ... -N` | Compression level (default: `-1`). `-1` is fastest, `-N` scales exponentially (e.g. `-10` = 32 MB window). |
| `-o, --output <dir>` | Directory for extraction (default: current directory) |
| `-c, --stdout` | Output decompressed file(s) directly to stdout |
| `-f, --force` | Overwrite existing files during extraction |
| `-v, --verbose` | Show progress details for each file |
| `-q, --quiet` | Suppress summary line output |

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

### Text / XML (enwik8) — 95.4 MB (`enwik8`)
*100 million bytes of English Wikipedia XML text.*

| Tool | Size | Ratio | C (Speed) | D (Speed) |
|------|-----:|------:|----------:|----------:|
| **bounce -2** | **34.6 MB** | **36.2%** | **195.0 MB/s** | **934.3 MB/s** |
| zstd -3 | 33.8 MB | 35.4% | 209.2 MB/s | 1050.0 MB/s |
| gzip -9 | 34.8 MB | 36.5% | 34.4 MB/s | 800.3 MB/s |
| lz4 -9 | 40.3 MB | 42.3% | 161.4 MB/s | 1844.3 MB/s |
| zstd -19 | 25.7 MB | 26.9% | 5.7 MB/s | 1035.3 MB/s |
| bzip2 -9 | 27.7 MB | 29.0% | 22.8 MB/s | 58.4 MB/s |
| brotli -q 11 | 24.5 MB | 25.7% | 0.8 MB/s | 568.9 MB/s |

### Safetensors Model Weights — 255.5 MB (`model.safetensors`)
*IEEE-754 neural network weights. Demonstrates the effectiveness of the byte-shuffle transform.*

| Tool | Size | Ratio | C (Speed) | D (Speed) |
|------|-----:|------:|----------:|----------:|
| **bounce -2** | **218.1 MB** | **85.3%** | **172.4 MB/s** | **1069.0 MB/s** |
| zstd -3 | 235.3 MB | 92.1% | 2235.9 MB/s | 1121.8 MB/s |
| gzip -9 | 235.6 MB | 92.2% | 38.6 MB/s | 492.9 MB/s |
| lz4 -9 | 255.5 MB | 100.0% | 371.8 MB/s | 3668.9 MB/s |
| zstd -19 | 235.2 MB | 92.1% | 30.6 MB/s | 1138.6 MB/s |
| bzip2 -9 | 241.5 MB | 94.5% | 15.8 MB/s | 32.2 MB/s |
| brotli -q 5 | 235.1 MB | 92.0% | 199.4 MB/s | 212.6 MB/s |

### Silesia Corpus (Mixed/Code) — 202.2 MB (`silesia.tar`)
*A mixed corpus of source code, book text, binaries, and database files.*

| Tool | Size | Ratio | C (Speed) | D (Speed) |
|------|-----:|------:|----------:|----------:|
| **bounce -2** | **65.4 MB** | **32.3%** | **256.6 MB/s** | **953.1 MB/s** |
| zstd -3 | 63.2 MB | 31.3% | 1541.6 MB/s | 1307.3 MB/s |
| gzip -9 | 64.5 MB | 31.9% | 19.4 MB/s | 972.1 MB/s |
| lz4 -9 | 74.4 MB | 36.8% | 252.6 MB/s | 2248.4 MB/s |
| zstd -19 | 50.4 MB | 24.9% | 14.0 MB/s | 1303.0 MB/s |
| bzip2 -9 | 52.0 MB | 25.7% | 20.9 MB/s | 64.7 MB/s |
| brotli -q 11 | 47.5 MB | 23.5% | 0.8 MB/s | 560.1 MB/s |

### Database Dump (SQL) — 164.3 MB (`employees.sql`)
*A concatenated, inlined SQL dump of MySQL sample database containing real employees records.*

| Tool | Size | Ratio | C (Speed) | D (Speed) |
|------|-----:|------:|----------:|----------:|
| **bounce -2** | **35.1 MB** | **21.3%** | **168.7 MB/s** | **1141.0 MB/s** |
| zstd -3 | 37.9 MB | 23.1% | 1671.0 MB/s | 1076.9 MB/s |
| gzip -9 | 33.2 MB | 20.2% | 7.6 MB/s | 1318.1 MB/s |
| lz4 -9 | 46.9 MB | 28.5% | 114.3 MB/s | 2168.5 MB/s |
| zstd -19 | 18.4 MB | 11.2% | 5.6 MB/s | 1721.2 MB/s |
| bzip2 -9 | 25.5 MB | 15.5% | 24.2 MB/s | 86.6 MB/s |
| brotli -q 11 | 17.1 MB | 10.4% | 0.7 MB/s | 822.1 MB/s |

### Structured Data (JSON) — 181.0 MB (`citylots.json`)
*Large structured JSON dataset containing geographical features of San Francisco city lots.*

| Tool | Size | Ratio | C (Speed) | D (Speed) |
|------|-----:|------:|----------:|----------:|
| **bounce -2** | **19.0 MB** | **10.5%** | **452.1 MB/s** | **1375.4 MB/s** |
| zstd -3 | 17.7 MB | 9.8% | 2558.5 MB/s | 2303.5 MB/s |
| gzip -9 | 21.2 MB | 11.7% | 46.8 MB/s | 2191.9 MB/s |
| lz4 -9 | 23.6 MB | 13.1% | 628.2 MB/s | 2746.0 MB/s |
| zstd -19 | 12.3 MB | 6.8% | 14.4 MB/s | 2802.1 MB/s |
| bzip2 -9 | 17.8 MB | 9.8% | 14.1 MB/s | 104.1 MB/s |
| brotli -q 11 | 11.9 MB | 6.6% | 1.3 MB/s | 1367.3 MB/s |

### Compressed Video (Fallback Test) — 61.7 MB (`video.mp4`)
*H.264 compressed video. Already compressed high-entropy file to test safety/fallback detection at maximum level (-9).*

| Tool | Size | Ratio | C (Speed) | D (Speed) |
|------|-----:|------:|----------:|----------:|
| **bounce -9** | **61.4 MB** | **99.6%** | **204.8 MB/s** | **564.8 MB/s** |
| zstd -3 | 61.6 MB | 99.8% | 1595.0 MB/s | 2957.4 MB/s |
| gzip -9 | 61.5 MB | 99.7% | 58.6 MB/s | 950.1 MB/s |
| lz4 -9 | 61.6 MB | 99.9% | 325.2 MB/s | 2171.2 MB/s |
| zstd -19 | 61.2 MB | 99.2% | 14.8 MB/s | 2099.5 MB/s |
| bzip2 -9 | 61.6 MB | 99.9% | 15.2 MB/s | 31.7 MB/s |
| brotli -q 5 | 61.5 MB | 99.8% | 608.5 MB/s | 2367.1 MB/s |

---

## Theory & Origin

The name **bounce** and the codec's architectural framework are inspired by the [Vacuum-Matter Fluctuation (VMF) theory](https://github.com/infosave2007/vmf) (Null-Vector Gravity).

In cosmology, the "Big Bounce" describes a cyclical universe. Unlike standard models where a collapsing universe ends in a singularity, the VMF framework proves that macroscopic melting halts collapse at a critical density ($\rho_c$), causing a bounce. This is governed by the modified Friedmann equation:

$$ H^2 \propto \rho \left(1 - \frac{\rho}{\rho_c}\right) $$

where the critical density is mathematically tied to the **Golden Ratio**: $\rho_c \propto \frac{3 - \sqrt{5}}{2}$.

**Cosmological "Bounce" as Codec Architecture:**
The `bounce` algorithm implements this physical model as a reproducible computational pipeline:
- **Collapse:** The discrete evolution of the bitstring "collapses" data into ultra-dense blocks.
- **Critical Density:** The archiver avoids informational "singularity" by halting compression at a critical entropy threshold, leveraging golden ratio proportions for optimal packing limits.
- **Bounce:** The compressed data expands back exactly to its original state during rapid lossless decompression.

The NVG-bounce serves as a powerful mathematical abstraction: internally, it represents a deep cosmological model of information evolution; externally, it operates as an extremely fast utility that seamlessly compresses and restores your data.

---

## License & Patent

Distributed under the **Apache License 2.0** — see [LICENSE](LICENSE).

The dynamic task routing classification mechanism (Signal Reconstruction Resonance) is covered by US Patent Application **USA 19/452,440 (Jan 19, 2026)**. See [NOTICE](NOTICE) for details.

---

## ❤️ Support the Project
If you find this project helpful, you can support its development with a donation via Tribute:

👉 https://t.me/tribute/app?startapp=dzX1

Every donation helps keep bounce evolving. Thank you! 🙌
