# bounce

**bounce** is a fast, zero-dependency file archiver written in Rust. It is based on the **Big Bounce** codec: data "collapses" during compression and "bounces" back exactly to its original state during decompression—reminiscent of the cosmological Big Bounce model.

- Pure Rust, **zero external dependencies** (only the standard library).
- Automatic compression method selection for each file.
- Multi-threaded compression and decompression for large files.
- Integrity verification of each file using **CRC-32**.
- Path traversal protection during extraction.
- Archive file extension: **`.bnc`**.

---

## Build

```bash
cd bounce
cargo build --release
# Binary: target/release/bounce
```

### Native Apple Silicon (ARM64) Build

If you are on an Apple Silicon Mac, your default Rustup toolchain might be running under x86_64 emulation (Rosetta 2). For maximum performance, compile natively using the `aarch64-apple-darwin` target:

```bash
cargo build --release --target aarch64-apple-darwin
# Binary: target/aarch64-apple-darwin/release/bounce
```

System installation (optional):

```bash
cargo install --path .
```

---

## Usage

```bash
bounce <command> [options] <archive> [files...]
```

| Command | Alias | Description |
|---------|-------|-------------|
| `create`  | `c` | Create archive from files and directories |
| `extract` | `x` | Extract archive (completely or selected files) |
| `list`    | `l` | List archive contents |
| `test`    | `t` | Verify archive integrity (CRC-32) |
| `help`    | `h` | Show help |
| `version` | `v` | Show version |

### Options

| Option | Description |
|--------|-------------|
| `-o, --output <dir>` | Directory for extraction (default: current directory) |
| `-c, --stdout` | Output decompressed file(s) directly to stdout |
| `-f, --force` | Overwrite existing files during extraction |
| `-v, --verbose` | Show progress details for each file |
| `-q, --quiet` | Suppress summary line output |

---

## Examples

Create an archive from a file and a directory:

```bash
bounce c backup.bnc report.pdf photos/
```

List contents:

```bash
bounce l backup.bnc
```

```
    Original        Stored   Ratio      Method  Name
----------------------------------------------------------------
      54.0 KB       16.3 KB   30.2%       plain  report.pdf
       1.2 MB      420.0 KB   34.1%    shuf+blk  photos/img01.png
----------------------------------------------------------------
       1.3 MB      436.3 KB   33.8%              2 file(s)
```

Verify integrity:

```bash
bounce t backup.bnc
# backup.bnc: OK (2 file(s) verified)
```

Extract everything into the `restored/` directory:

```bash
bounce x backup.bnc -o restored/
```

Extract a single file:

```bash
bounce x backup.bnc report.pdf -o restored/
```

---

## Theory: The Big Bounce Model in Data Compression

The codec is based on an analogy to the **Big Bounce** cosmological model. Instead of reaching a singularity (an irreversible loss of information), the universe undergoes a phase of maximum compression and then "bounces" back, fully **preserving all information**. Translated to data, this principle states:

> Any data representation can be compressed to a state of minimum redundancy (the "bounce point") and exactly expanded back, provided the compression is driven by a **bijective** (reversible) transformation that preserves all information.

This leads to three practical principles implemented in `bounce`:

1. **Strict Reversibility:** Every stage of the pipeline (LZ77 factorization, Huffman coding, byte-shuffle) is bijective. CRC-32 verification guarantees that the "bounce" restores the original bytes bit-for-bit.
2. **Structural Resonance:** Before entropy coding, data is restructured to maximize its internal periodicity (structural "resonance")—hence the byte-shuffle for floating-point weights and block-wise Huffman trees that adapt to local statistics.
3. **Minimum Redundancy Point:** Among several reversible transformations, the one yielding the smallest footprint is automatically chosen—representing the empirical "bounce point" for the given file.

The theoretical foundation of this algorithm was first published by the author in the journal of **Saint Petersburg State University (SPbSU) in 2007**. The current implementation represents its engineering evolution applied to file and neural network weight compression. The task routing mechanism between reversible transformations is covered by the author's patent (see [NOTICE](NOTICE)).

---

## How Compression Works

The Big Bounce codec is a self-contained DEFLATE-like implementation that applies the principles above:

1. **LZ77** with a 64 KB sliding window finds duplicate sequences.
2. **Huffman coding** encodes literals and distance codes (with an optimal tree generated per 32 KB block).
3. **Byte-shuffle** transforms (stride = 2 and 4) expose the structural redundancy of binary data (such as float32 weights in neural networks).

For each file, the engine tries methods `plain`, `blocked`, `shuf+defl`, `shuf+blk`, `shuf2+defl`, `shuf2+blk` and picks the best one. If the file is incompressible (e.g., already compressed or random data), it is saved unmodified (`stored` mode), ensuring the archive is never larger than the sum of input files plus small headers.

Large files (> 1 MB) are processed using block-wise methods only, enabling parallel processing across CPU cores.

> **Window Limitation:** The LZ77 sliding window is capped at 64 KB, meaning duplicates separated by a larger distance are not deduplicated. For files with long-range duplicates (very large templates, DB dumps), `bounce` will lag behind compressors with larger dictionaries (`xz`, `zstd --long`, `brotli`), but wins in simplicity and speed.

For container format details, see [FORMAT.md](FORMAT.md).

---

## Benchmarks

Comparative benchmark run on a **MacBook Air M4 (10 cores, 24 GB RAM, arm64)**. All utilities (`bounce`, `gzip`, `bzip2`, `lz4`, `zstd`, `xz`, `brotli`) run fully natively as `arm64`/`arm64e` binaries on the Apple M4 CPU.

To run the benchmark:
```bash
bash bounce/benchmark.sh
```
Or to run for **bounce only** to skip other utilities:
```bash
bash bounce/benchmark.sh --only-bounce
```

### Text — 1.32 MB (Repeating Markdown Corpus)

| Tool | Size | Ratio | C (Compression) | D (Decompression) |
|------|-----:|------:|----------------:|------------------:|
| **bounce** | 466,947 | 33.8% | 17.6 MB/s | 36.6 MB/s |
| gzip -9 | 404,664 | 29.3% | 15.3 MB/s | 37.2 MB/s |
| lz4 -9 | 456,579 | 33.1% | 2.7 MB/s | 36.9 MB/s |
| zstd -19 | 45,772 | 3.3% | 3.3 MB/s | 36.2 MB/s |
| bzip2 -9 | 123,625 | 9.0% | 12.7 MB/s | 26.4 MB/s |
| xz -9e | 44,092 | 3.2% | 1.7 MB/s | 31.6 MB/s |
| brotli -q 11 | 42,122 | 3.1% | 0.9 MB/s | 35.8 MB/s |

### Source Code — 0.51 MB (Go + Rust)

| Tool | Size | Ratio | C (Compression) | D (Decompression) |
|------|-----:|------:|----------------:|------------------:|
| **bounce** | 100,126 | 18.9% | 4.6 MB/s | 13.8 MB/s |
| gzip -9 | 101,897 | 19.2% | 10.2 MB/s | 14.2 MB/s |
| lz4 -9 | 122,076 | 23.0% | 11.2 MB/s | 13.8 MB/s |
| zstd -19 | 84,994 | 16.0% | 4.6 MB/s | 13.8 MB/s |
| bzip2 -9 | 85,010 | 16.0% | 8.2 MB/s | 11.8 MB/s |
| xz -9e | 82,852 | 15.6% | 1.9 MB/s | 11.5 MB/s |
| brotli -q 11 | 80,957 | 15.3% | 1.0 MB/s | 14.1 MB/s |

### LLM Weights — 1024 MB (Slice of `cortiq-coder-12b-Q4_K_M.gguf`)

Quantized weights are close to random data—serving as an honest test on a large binary payload.

| Tool | Size | Ratio | C (Compression) | D (Decompression) |
|------|-----:|------:|----------------:|------------------:|
| **bounce** | 1,066,254,392 | 99.3% | 93.2 MB/s | **1313.6 MB/s** |
| gzip -9 | 1,063,316,152 | 99.0% | 42.2 MB/s | 513.4 MB/s |
| lz4 -9 | 1,066,181,306 | 99.3% | 214.9 MB/s | **3435.4 MB/s** |
| zstd -19 -T0 | 1,060,561,565 | 98.8% | 21.0 MB/s | 1247.8 MB/s |
| bzip2 -9 | 1,068,613,893 | 99.5% | 14.1 MB/s | 32.7 MB/s |
| xz -2 -T0 | 1,065,339,096 | 99.2% | 13.9 MB/s | 731.9 MB/s |
| brotli -q 5 | 1,065,151,521 | 99.2% | 587.7 MB/s | **4173.0 MB/s** |

### Text — 500 MB (Large compressible text, reference run)

| Tool | Size | Ratio | C (Compression) | D (Decompression) |
|------|-----:|------:|----------------:|------------------:|
| **bounce** | 177,208,958 | 33.8% | 36.6 MB/s | 310.0 MB/s |
| gzip -9 | 153,398,124 | 29.2% | 26.5 MB/s | 1350.3 MB/s |
| lz4 -9 | 172,959,945 | 33.0% | 222.9 MB/s | 2210.0 MB/s |
| zstd -19 | 117,374 | 0.02% | 1917.8 MB/s | 5008.1 MB/s |
| xz -9 -T0 | 207,792 | 0.04% | 28.7 MB/s | 2774.3 MB/s |
| brotli -q 11 | 42,529 | 0.01% | 41.6 MB/s | 1539.2 MB/s |

**Honest Conclusions:**

- **Decompression:** Native ARM64 decompression of `bounce` reaches **~1.31 GB/s** on large payloads. While this is a massive speedup compared to Rosetta emulation, it remains slower than highly optimized native-speed streaming engines like `lz4` and `brotli` (which reach 3-4 GB/s natively) due to the overhead of per-block Huffman decoding and full-file CRC-32 verification.
- **Compression:** On large binary data, `bounce` is fast (due to parallelized blocks and automatic method routing) but limits duplicates to a 64 KB window. Hence, on files with long-range duplicates (e.g., 500 MB text), it achieves 33.8% ratio while large-dictionary tools (`zstd`, `xz`, `brotli`) compress it to nearly zero.
- **Safety:** On incompressible inputs, `bounce` guarantees that files do not bloat by falling back to `stored` (raw payload) mode.

---

## Tests

```bash
cargo test --release
```

Tests cover: round-trip compression/decompression, known CRC-32 vectors, full CLI integration flow (`create` → `list` → `test` → `extract`), and path traversal mitigation.

---

## License and Patent

Distributed under the **Apache License 2.0**—see [LICENSE](LICENSE).

The task routing mechanism used in this project is covered by the author's patent. See [NOTICE](NOTICE).
