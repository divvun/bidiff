#!/usr/bin/env bash
set -euo pipefail

# Benchmark script for bidiff
# Downloads test data, builds bidiff, runs benchmarks, prints summary table.

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA="$ROOT/bench/data"
BIDIFF="$ROOT/target/release/bidiff"

mkdir -p "$DATA"

# ── Download helpers ─────────────────────────────────────────────────────────

download() {
    local url="$1" dest="$2" sha256="$3"
    if [ -f "$dest" ]; then
        echo "  exists: $(basename "$dest")"
        return
    fi
    echo "  downloading: $(basename "$dest")"
    curl -fSL --progress-bar -o "$dest" "$url"
    local actual
    actual=$(sha256sum "$dest" | cut -d' ' -f1)
    if [ "$actual" != "$sha256" ]; then
        echo "ERROR: checksum mismatch for $dest"
        echo "  expected: $sha256"
        echo "  got:      $actual"
        rm -f "$dest"
        return 1
    fi
}

decompress_xz() {
    local src="$1"
    local dst="${src%.xz}"
    if [ -f "$dst" ]; then
        echo "  exists: $(basename "$dst")"
        return
    fi
    echo "  decompressing: $(basename "$src")"
    xz -dk "$src"
}

decompress_bz2() {
    local src="$1"
    local dst="${src%.bz2}"
    if [ -f "$dst" ]; then
        echo "  exists: $(basename "$dst")"
        return
    fi
    echo "  decompressing: $(basename "$src")"
    bzip2 -dk "$src"
}

# ── Download test data ───────────────────────────────────────────────────────

echo "=== Downloading test data ==="

download "https://dl.winehq.org/wine/source/4.x/wine-4.18.tar.xz" \
    "$DATA/wine-4.18.tar.xz" \
    "d10b0550215f789655a1c67db91a8afc0b4284416bae1869396f06e2db360e32"
download "https://dl.winehq.org/wine/source/4.x/wine-4.19.tar.xz" \
    "$DATA/wine-4.19.tar.xz" \
    "361abeebb676c65acafdb2bcdc96a7fbd2a7bc8689f7bebbcca97d8ca069ce20"

download "https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-5.3.13.tar.xz" \
    "$DATA/linux-5.3.13.tar.xz" \
    "9f04e53f03d0ead6561195fb71aac18cbee419112ed54f9d4fc1515a5fa5c92f"
download "https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-5.4.tar.xz" \
    "$DATA/linux-5.4.tar.xz" \
    "bf338980b1670bca287f9994b7441c2361907635879169c64ae78364efc5f491"

download "https://ftp.mozilla.org/pub/firefox/releases/71.0b11/linux-x86_64/en-US/firefox-71.0b11.tar.bz2" \
    "$DATA/firefox-71.0b11.tar.bz2" \
    "21cabefb4cbea04b7efe174786357cf77414cadc8a5a7a5bf35066ec32686dc9"
download "https://ftp.mozilla.org/pub/firefox/releases/71.0b12/linux-x86_64/en-US/firefox-71.0b12.tar.bz2" \
    "$DATA/firefox-71.0b12.tar.bz2" \
    "b4c356453d5d1ce770315683d18a77f9888b39470fe08cd77660f9ca061b769b"

# Chrome: download .deb packages from UChicago mirror, extract the chrome binary
CHROME_MIRROR="https://mirror.cs.uchicago.edu/google-chrome/pool/main/g/google-chrome-stable"

extract_chrome() {
    local deb="$1" dest="$2" sha256="$3"
    if [ -f "$dest" ]; then
        echo "  exists: $(basename "$dest")"
        return
    fi
    echo "  extracting: $(basename "$dest") from $(basename "$deb")"
    dpkg-deb --fsys-tarfile "$deb" | tar xf - --to-stdout ./opt/google/chrome/chrome > "$dest"
    local actual
    actual=$(sha256sum "$dest" | cut -d' ' -f1)
    if [ "$actual" != "$sha256" ]; then
        echo "ERROR: checksum mismatch for $dest"
        echo "  expected: $sha256"
        echo "  got:      $actual"
        rm -f "$dest"
        return 1
    fi
}

download "$CHROME_MIRROR/google-chrome-stable_78.0.3904.97-1_amd64.deb" \
    "$DATA/chrome-78.0.3904.97.deb" \
    "77c7627818630d73bedcfc0b38dde145d2554bea2d49616fe9b3cf8eb5f290db"
download "$CHROME_MIRROR/google-chrome-stable_78.0.3904.108-1_amd64.deb" \
    "$DATA/chrome-78.0.3904.108.deb" \
    "06ce37a4ae8bb93c35bf874681e12766f10bdedff96717f6a03fc84d60cbdace"

extract_chrome "$DATA/chrome-78.0.3904.97.deb" "$DATA/chrome-78.0.3904.97" \
    "86350c690c160b5f1a53f419abb6cadb487698364561687a77ade47380a70ade"
extract_chrome "$DATA/chrome-78.0.3904.108.deb" "$DATA/chrome-78.0.3904.108" \
    "8e946772de589abd7699a408af5f7dbde9c883cb5630ac06f4e87212adeae591"

echo ""
echo "=== Decompressing ==="

decompress_xz "$DATA/wine-4.18.tar.xz"
decompress_xz "$DATA/wine-4.19.tar.xz"
decompress_xz "$DATA/linux-5.3.13.tar.xz"
decompress_xz "$DATA/linux-5.4.tar.xz"
decompress_bz2 "$DATA/firefox-71.0b11.tar.bz2"
decompress_bz2 "$DATA/firefox-71.0b12.tar.bz2"

# ── Build bidiff ─────────────────────────────────────────────────────────────

echo ""
echo "=== Building bidiff ==="
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p bidiff-cli 2>&1 | tail -3

# ── Define test pairs ────────────────────────────────────────────────────────

declare -a NAMES=()
declare -a OLDERS=()
declare -a NEWERS=()

add_pair() {
    local name="$1" older="$2" newer="$3"
    if [ -f "$older" ] && [ -f "$newer" ]; then
        NAMES+=("$name")
        OLDERS+=("$older")
        NEWERS+=("$newer")
    else
        echo "  skipping $name (files not found)"
    fi
}

add_pair "Wine 4.18 → 4.19" "$DATA/wine-4.18.tar" "$DATA/wine-4.19.tar"
add_pair "Linux 5.3 → 5.4" "$DATA/linux-5.3.13.tar" "$DATA/linux-5.4.tar"
add_pair "Firefox 71.0b11 → b12" "$DATA/firefox-71.0b11.tar" "$DATA/firefox-71.0b12.tar"
add_pair "Chrome 78.0.3904.97 → 108" "$DATA/chrome-78.0.3904.97" "$DATA/chrome-78.0.3904.108"

# ── Parse cycle output ───────────────────────────────────────────────────────

# cycle output format:
#   zstd         patch 249 KiB         000.121% of 201.4 MiB         dtime 424.123456ms   ptime 125.456789ms   anon 21.5 MiB
parse_cycle() {
    local line="$1"
    # Extract fields by keyword
    PATCH_SIZE=$(echo "$line" | grep -oP 'patch \K[0-9.]+ [A-Za-z]+')
    RATIO=$(echo "$line" | grep -oP '[0-9.]+(?=% of)')
    NEW_SIZE=$(echo "$line" | grep -oP '% of \K[0-9.]+ [A-Za-z]+')
    DTIME_RAW=$(echo "$line" | grep -oP 'dtime \K[0-9.]+[a-zµ]+')
    PTIME_RAW=$(echo "$line" | grep -oP 'ptime \K[0-9.]+[a-zµ]+')
    ANON=$(echo "$line" | grep -oP 'anon \K[0-9.]+ [A-Za-z]+' || echo "")
}

# Convert Rust Duration debug format to seconds
to_seconds() {
    local raw="$1"
    if [[ "$raw" =~ ^([0-9.]+)µs$ ]]; then
        echo "${BASH_REMATCH[1]}" | awk '{printf "%.3f", $1 / 1000000}'
    elif [[ "$raw" =~ ^([0-9.]+)ms$ ]]; then
        echo "${BASH_REMATCH[1]}" | awk '{printf "%.2f", $1 / 1000}'
    elif [[ "$raw" =~ ^([0-9.]+)s$ ]]; then
        echo "${BASH_REMATCH[1]}" | awk '{printf "%.1f", $1}'
    else
        echo "$raw"
    fi
}

# Format seconds for display: seconds if <60, Xm Ys if >=60
fmt_time() {
    local secs="$1"
    local int_secs
    int_secs=$(echo "$secs" | awk '{printf "%d", $1}')
    if [ "$int_secs" -ge 60 ]; then
        local mins=$((int_secs / 60))
        local remainder=$((int_secs % 60))
        echo "${mins}m ${remainder}s"
    else
        echo "${secs}s"
    fi
}

# ── Run benchmarks ───────────────────────────────────────────────────────────

echo ""
echo "=== Running benchmarks ==="

declare -a R_NAME=()
declare -a R_NEW_SIZE=()
declare -a R_PATCH_SIZE=()
declare -a R_RATIO=()
declare -a R_PTIME=()
declare -a R_DTIME=()
declare -a R_MEM=()
declare -a R_DTIME_RAM=()
declare -a R_MEM_RAM=()

for i in "${!NAMES[@]}"; do
    name="${NAMES[$i]}"
    older="${OLDERS[$i]}"
    newer="${NEWERS[$i]}"

    echo ""
    echo "--- $name ---"

    # Default mode (file-backed hash table)
    echo "  file-backed + anon measurement..."
    output=$("$BIDIFF" cycle "$older" "$newer" --with-anon 2>/dev/null)
    echo "  $output"
    parse_cycle "$output"

    R_NAME+=("$name")
    R_NEW_SIZE+=("$NEW_SIZE")
    R_PATCH_SIZE+=("$PATCH_SIZE")
    R_RATIO+=("$RATIO")
    R_PTIME+=("$(to_seconds "$PTIME_RAW")")
    R_DTIME+=("$(to_seconds "$DTIME_RAW")")
    R_MEM+=("$ANON")

    # RAM mode
    echo "  RAM mode + anon measurement..."
    output=$("$BIDIFF" cycle "$older" "$newer" --ram --with-anon 2>/dev/null)
    echo "  $output"
    parse_cycle "$output"

    R_DTIME_RAM+=("$(to_seconds "$DTIME_RAW")")
    R_MEM_RAM+=("$ANON")
done

# ── Print summary table ─────────────────────────────────────────────────────

echo ""
echo ""
echo "=== Results ==="
echo ""
printf "| %-28s | %8s | %12s | %6s | %10s | %10s | %10s | %15s | %12s |\n" \
    "Test case" "New size" "Patch size" "Ratio" "Patch time" "Diff time" "Memory" "Diff time (RAM)" "Memory (RAM)"
printf "|%-30s|%10s|%14s|%8s|%12s|%12s|%12s|%17s|%14s|\n" \
    "------------------------------" "----------" "--------------" "--------" "------------" "------------" "------------" "-----------------" "--------------"

for i in "${!R_NAME[@]}"; do
    printf "| %-28s | %8s | %12s | %5s%% | %10s | %10s | %10s | %15s | %12s |\n" \
        "${R_NAME[$i]}" \
        "${R_NEW_SIZE[$i]}" \
        "${R_PATCH_SIZE[$i]}" \
        "${R_RATIO[$i]}" \
        "$(fmt_time "${R_PTIME[$i]}")" \
        "$(fmt_time "${R_DTIME[$i]}")" \
        "${R_MEM[$i]}" \
        "$(fmt_time "${R_DTIME_RAM[$i]}")" \
        "${R_MEM_RAM[$i]}"
done

echo ""
echo "Done."
