#!/usr/bin/env bash
# Comparative benchmark: bounce vs standard archivers.
#
# Reports compression ratio, compression speed and decompression speed.
# To isolate codec throughput from disk-write cost, decompression is timed
# writing to /dev/null and the best of 3 runs is reported (this amortizes
# process-launch overhead). Compression is timed once. Run from repo root:
#
#   bash bounce/benchmark.sh
#
# Requires: gzip, bzip2, xz, zstd, brotli, lz4 and a release build of bounce.
set -u

BOUNCE_DIR="$(dirname "$0")"
export PATH="$BOUNCE_DIR/.bin:$PATH"

BOUNCE=""
if [[ "$(uname -m)" == "arm64" && "$(uname -s)" == "Darwin" ]]; then
    if [[ -f "$BOUNCE_DIR/target/aarch64-apple-darwin/release/bounce" ]]; then
        BOUNCE="$BOUNCE_DIR/target/aarch64-apple-darwin/release/bounce"
    fi
fi
if [[ -z "$BOUNCE" ]]; then
    BOUNCE="$BOUNCE_DIR/target/release/bounce"
fi

ONLY_BOUNCE=0
for arg in "$@"; do
    if [[ "$arg" == "--only-bounce" ]]; then
        ONLY_BOUNCE=1
    fi
done

BENCH_DIR="${BENCH_DIR:-.bench}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
DEC_RUNS=3

now() { python3 -c 'import time;print(f"{time.time():.5f}")'; }
mbps() { python3 -c "d=$2-$1; print(f'{$3/1048576/d:.1f}' if d>0 else 'inf')"; }

# run_tool <label> <orig_size> <out_file> <comp_cmd> <decomp_cmd>
# comp_cmd writes <out_file>; decomp_cmd reads it and writes to stdout.
run_tool() {
    local label="$1" orig="$2" out="$3" comp_cmd="$4" decomp_cmd="$5"
    local t0 t1 sz ratio cmbps

    t0="$(now)"; eval "$comp_cmd" >/dev/null 2>&1; t1="$(now)"
    if [[ ! -s "$out" ]]; then
        printf "  %-18s   (failed)\n" "$label"
        return
    fi
    sz="$(stat -f%z "$out")"
    ratio="$(python3 -c "print(f'{$sz/$orig*100:.1f}')")"
    cmbps="$(mbps "$t0" "$t1" "$orig")"

    # Best-of-N decompression to /dev/null.
    local best="" d0 d1 cur
    local i=0
    while [[ $i -lt $DEC_RUNS ]]; do
        d0="$(now)"; eval "$decomp_cmd" >/dev/null 2>&1; d1="$(now)"
        cur="$(mbps "$d0" "$d1" "$orig")"
        best="$(python3 -c "b='$best'; c=$cur; print(c if b=='' or c>float(b) else b)")"
        i=$((i + 1))
    done

    printf "  %-18s %12d B  %6s%%  C:%8s MB/s  D:%8s MB/s\n" \
        "$label" "$sz" "$ratio" "$cmbps" "$best"
    rm -f "$out"
}

bench_file() {
    local name="$1" path="$2" heavy="$3"
    if [[ ! -f "$path" ]]; then
        echo
        echo "### $name — (file not found: $path)"
        return
    fi
    local orig; orig="$(stat -f%z "$path")"
    echo
    echo "### $name — $(python3 -c "print(f'{$orig/1048576:.2f} MB')") ($orig bytes)"
    printf "  %-18s %12s    %6s  %12s  %12s\n" "tool" "size" "ratio" "compress" "decompress"
    printf "  %s\n" "---------------------------------------------------------------------------"

    run_tool "bounce"   "$orig" "$TMP/o.bnc" \
        "'$BOUNCE' c '$TMP/o.bnc' -2 '$path' -q" \
        "'$BOUNCE' x '$TMP/o.bnc' -c"

    if [[ "$ONLY_BOUNCE" == "1" ]]; then
        return
    fi

    run_tool "gzip -9"  "$orig" "$TMP/o.gz" \
        "gzip -9 -c '$path' > '$TMP/o.gz'" \
        "gzip -dc '$TMP/o.gz'"
    run_tool "lz4 -9"   "$orig" "$TMP/o.lz4" \
        "lz4 -9 -c '$path' > '$TMP/o.lz4'" \
        "lz4 -dc '$TMP/o.lz4'"
    run_tool "zstd -19" "$orig" "$TMP/o.zst" \
        "zstd -19 -T0 -c '$path' > '$TMP/o.zst'" \
        "zstd -dc -T0 '$TMP/o.zst'"
    run_tool "bzip2 -9" "$orig" "$TMP/o.bz2" \
        "bzip2 -9 -c '$path' > '$TMP/o.bz2'" \
        "bzip2 -dc '$TMP/o.bz2'"
    if [[ "$heavy" == "1" ]]; then
        # Near-incompressible data: higher levels add no ratio but huge time,
        # so use fast multi-threaded levels for the >100MB case.
        run_tool "xz -2 -T0"   "$orig" "$TMP/o.xz" \
            "xz -2 -T0 -c '$path' > '$TMP/o.xz'" \
            "xz -dc -T0 '$TMP/o.xz'"
        run_tool "brotli -q 5" "$orig" "$TMP/o.br" \
            "brotli -q 5 -c '$path' > '$TMP/o.br'" \
            "brotli -dc '$TMP/o.br'"
    else
        run_tool "xz -9e"       "$orig" "$TMP/o.xz" \
            "xz -9e -c '$path' > '$TMP/o.xz'" \
            "xz -dc '$TMP/o.xz'"
        run_tool "brotli -q 11" "$orig" "$TMP/o.br" \
            "brotli -q 11 -c '$path' > '$TMP/o.br'" \
            "brotli -dc '$TMP/o.br'"
    fi
}

echo "=================================================================="
echo " bounce comparative benchmark — $(uname -m), $(sysctl -n machdep.cpu.brand_string)"
echo " decompression: best of $DEC_RUNS runs, output to /dev/null"
echo "=================================================================="



SAFETENSORS_PATH="/Users/oleg/Downloads/cortiq-coder-12b/model-mtp.safetensors"
if [[ -f "$SAFETENSORS_PATH" ]]; then
    bench_file "Safetensors Model Weights" "$SAFETENSORS_PATH" 1
fi

bench_file "Text (markdown corpus)"    "$BENCH_DIR/text.dat"     0
bench_file "Source code (Go + Rust)"   "$BENCH_DIR/code.dat"     0
bench_file "LLM weights (gguf Q4_K_M)" "$BENCH_DIR/weights.gguf" 1

echo
echo "Done. C = compression speed, D = decompression speed (best of $DEC_RUNS)."
