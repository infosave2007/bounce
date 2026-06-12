#!/usr/bin/env bash
# Public comparative benchmark: bounce vs standard archivers on public datasets.
# Downloads standard test files and generates Markdown tables for README.

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

# Auto-install dependencies if missing
MISSING=()
for cmd in curl unzip gzip lz4 zstd bzip2 brotli; do
    if ! command -v "$cmd" &> /dev/null; then
        MISSING+=("$cmd")
    fi
done

if [ ${#MISSING[@]} -gt 0 ]; then
    echo "Installing missing dependencies: ${MISSING[*]}..."
    if command -v brew &> /dev/null; then
        brew install "${MISSING[@]}" || { echo "Failed to install dependencies with brew."; exit 1; }
    elif command -v apt-get &> /dev/null; then
        sudo apt-get update && sudo apt-get install -y "${MISSING[@]}" || { echo "Failed to install dependencies with apt-get."; exit 1; }
    else
        echo "Error: Could not find 'brew' or 'apt-get' to install missing dependencies (${MISSING[*]}). Please install them manually."
        exit 1
    fi
fi

if [[ ! -f "$BOUNCE" ]]; then
    echo "Building bounce..."
    cargo build --release || { echo "Failed to build bounce. Is Rust installed?"; exit 1; }
    if [[ "$(uname -m)" == "arm64" && "$(uname -s)" == "Darwin" && -f "$BOUNCE_DIR/target/aarch64-apple-darwin/release/bounce" ]]; then
        BOUNCE="$BOUNCE_DIR/target/aarch64-apple-darwin/release/bounce"
    else
        BOUNCE="$BOUNCE_DIR/target/release/bounce"
    fi
fi

BENCH_DIR="$BOUNCE_DIR/.bench_public"
mkdir -p "$BENCH_DIR"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
DEC_RUNS=3

now() { python3 -c 'import time;print(f"{time.time():.5f}")'; }
mbps() { python3 -c "d=$2-$1; print(f'{$3/1048576/d:.1f}' if d>0 else 'inf')"; }
format_mb() { python3 -c "print(f'{$1/1048576:.1f} MB' if $1 >= 1048576 else f'{$1/1024:.1f} KB')"; }

# Download datasets
echo "Downloading datasets..."
if [[ ! -f "$BENCH_DIR/enwik8" ]]; then
    curl -# -L "http://mattmahoney.net/dc/enwik8.zip" -o "$BENCH_DIR/enwik8.zip"
    unzip -q "$BENCH_DIR/enwik8.zip" -d "$BENCH_DIR"
fi
if [[ ! -f "$BENCH_DIR/model.safetensors" ]]; then
    curl -# -L "https://huggingface.co/distilbert/distilbert-base-uncased/resolve/main/model.safetensors" -o "$BENCH_DIR/model.safetensors"
fi
if [[ ! -f "$BENCH_DIR/silesia.tar" ]]; then
    curl -# -L "http://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip" -o "$BENCH_DIR/silesia.zip"
    mkdir -p "$BENCH_DIR/silesia"
    unzip -q "$BENCH_DIR/silesia.zip" -d "$BENCH_DIR/silesia"
    tar -cf "$BENCH_DIR/silesia.tar" -C "$BENCH_DIR/silesia" .
fi
if [[ ! -f "$BENCH_DIR/citylots.json" ]]; then
    curl -# -L "https://raw.githubusercontent.com/zemirco/sf-city-lots-json/master/citylots.json" -o "$BENCH_DIR/citylots.json"
fi

if [[ ! -f "$BENCH_DIR/employees.sql" ]]; then
    echo "Downloading real SQL database dump..."
    curl -# -L "https://github.com/datacharmer/test_db/archive/refs/heads/master.zip" -o "$BENCH_DIR/test_db.zip"
    echo "Extracting..."
    mkdir -p "$BENCH_DIR/test_db"
    unzip -q "$BENCH_DIR/test_db.zip" -d "$BENCH_DIR/test_db"
    echo "Inlining database dump into a single employees.sql file..."
    BENCH_DIR="$BENCH_DIR" python3 -c '
import os, sys, re
bench_dir = os.environ["BENCH_DIR"]
db_dir = os.path.join(bench_dir, "test_db", "test_db-master")
main_sql = os.path.join(db_dir, "employees.sql")
out_sql = os.path.join(bench_dir, "employees.sql")

with open(main_sql, "r", encoding="utf-8", errors="ignore") as f:
    content = f.read()

def replacer(match):
    filename = match.group(1).strip().split()[0].strip("\x27\x22;")
    filepath = os.path.join(db_dir, filename)
    if os.path.exists(filepath):
        with open(filepath, "r", encoding="utf-8", errors="ignore") as dump_f:
            return dump_f.read()
    return match.group(0)

pattern = r"^[ \t]*(?:source|\\\.)[ \t]+([^;\n]+)(?:;)?"
content_inlined = re.sub(pattern, replacer, content, flags=re.MULTILINE)
with open(out_sql, "w", encoding="utf-8") as f:
    f.write(content_inlined)
'
    rm -rf "$BENCH_DIR/test_db.zip" "$BENCH_DIR/test_db"
fi

if [[ ! -f "$BENCH_DIR/video.mp4" ]]; then
    curl -# -L "https://download.blender.org/peach/bigbuckbunny_movies/BigBuckBunny_320x180.mp4" -o "$BENCH_DIR/video.mp4"
fi


run_tool() {
    local label="$1" orig="$2" out="$3" comp_cmd="$4" decomp_cmd="$5"
    local t0 t1 sz ratio cmbps

    t0="$(now)"; eval "$comp_cmd" >/dev/null 2>&1; t1="$(now)"
    if [[ ! -s "$out" ]]; then
        printf "| %s | (failed) | - | - | - |\n" "$label"
        return
    fi
    sz="$(stat -f%z "$out" 2>/dev/null || stat -c%s "$out")"
    ratio="$(python3 -c "print(f'{$sz/$orig*100:.1f}%')")"
    cmbps="$(mbps "$t0" "$t1" "$orig") MB/s"
    local formatted_sz="$(format_mb "$sz")"

    # Best-of-N decompression to /dev/null.
    local best="" d0 d1 cur
    local i=0
    while [[ $i -lt $DEC_RUNS ]]; do
        d0="$(now)"; eval "$decomp_cmd" >/dev/null 2>&1; d1="$(now)"
        cur="$(mbps "$d0" "$d1" "$orig")"
        best="$(python3 -c "b='$best'; c=$cur; print(c if b=='' or c>float(b) else b)")"
        i=$((i + 1))
    done

    # Bold the bounce row
    if [[ "$label" == *"bounce"* ]]; then
        printf "| **%s** | **%s** | **%s** | **%s** | **%s MB/s** |\n" "$label" "$formatted_sz" "$ratio" "$cmbps" "$best"
    else
        printf "| %s | %s | %s | %s | %s MB/s |\n" "$label" "$formatted_sz" "$ratio" "$cmbps" "$best"
    fi
    rm -f "$out"
}

bench_file() {
    local title="$1" name="$2" path="$3" heavy="$4" level="${5:-2}"
    if [[ ! -f "$path" ]]; then return; fi
    local orig; orig="$(stat -f%z "$path" 2>/dev/null || stat -c%s "$path")"
    local formatted_orig="$(format_mb "$orig")"
    
    echo
    echo "### $title — $formatted_orig (\`$name\`)"
    echo
    echo "| Tool | Size | Ratio | C (Speed) | D (Speed) |"
    echo "|------|-----:|------:|----------:|----------:|"
    
    run_tool "bounce -$level"   "$orig" "$TMP/o.bnc" \
        "'$BOUNCE' c '$TMP/o.bnc' -$level '$path' -q" \
        "'$BOUNCE' x '$TMP/o.bnc' -c"

}

echo "Running public benchmarks... (This may take a while depending on your CPU)"

bench_file "Text / XML (enwik8)" "enwik8" "$BENCH_DIR/enwik8" 0
bench_file "Safetensors Model Weights" "model.safetensors" "$BENCH_DIR/model.safetensors" 1
bench_file "Silesia Corpus (Mixed/Code)" "silesia.tar" "$BENCH_DIR/silesia.tar" 0
bench_file "Database Dump (SQL)" "employees.sql" "$BENCH_DIR/employees.sql" 0
bench_file "Structured Data (JSON)" "citylots.json" "$BENCH_DIR/citylots.json" 0
bench_file "Compressed Video (Fallback Test)" "video.mp4" "$BENCH_DIR/video.mp4" 1 9

echo
echo "Done!"
