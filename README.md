# bidiff

A binary diff and patch library for Rust, distantly derived from the [bsdiff](https://www.daemonology.net/bsdiff/) algorithm.

Instead of suffix arrays, `bidiff` uses a hash-table index with:

- **One memory limit** (`--memory-limit-mb`, `DiffParams::with_memory_limit`) for all of bidiff's own memory: the hash table, the matches scanned ahead, and the zstd compressor. Fixed costs come first, with fewer threads if need be; the hash table stays in RAM if the rest holds it, and otherwise goes in a file in a temporary directory, which the kernel can page to disk under memory pressure. Without a limit, everything is in RAM.
- **Parallel construction** via lock-free CAS insertion
- **Parallel scanning** of chunks ahead of the one being compressed, streaming matches in batches, with a bounded number of chunks in flight
- **Chunked patch format** with independent zstd-compressed chunks for parallel patch application
- **Integrity checks**: the patch header carries the size and BLAKE3 hash of the base and of the result, so applying refuses a wrong base before writing anything and checks the result afterwards. The hashes match `b3sum`.
- **Safe on malformed input**: corrupt, truncated or forged patches return errors rather than panicking, and memory use stays bounded whatever sizes a patch declares: a chunk's zstd window may not exceed 8 MiB, and the parallel path holds at most 128 MiB of decoders
- **No panics**: diffing and patching return errors instead, and clippy's `unwrap_used`, `expect_used`, `panic` and `indexing_slicing` lints are denied outside tests

## Usage

```bash
# Create a patch
bidiff diff old_file new_file patch_file

# Apply a patch
bidiff patch old_file patch_file output_file

# Apply it only if it produces the file you expect
bidiff patch --expect-size 211200000 --expect-hash 36ace40e...ceb75 old_file patch_file output_file

# Round-trip verification (diff then patch, verifies hash match)
bidiff cycle old_file new_file

# Show a patch's format version and its base and result sizes and hashes
bidiff info patch_file
```

The patch format is documented in the `bidiff::patch` module. Version `0x2001` added the hashes; older patches are refused with an "unsupported version" error.

### Options

```
--scan-chunk-mb <N>   Scan chunk size in MB (default: 1)
--block-size <N>      Hash index block size, minimum 4 (default: 32)
--threads <N>         Max threads for building the index and scanning (default: all cores)
--memory-limit-mb <N> Hold bidiff's own memory to N MiB (default: no limit, all in RAM)
--temp-dir <DIR>      Where the hash table goes when it does not fit the limit (default: the system's)
--max                 Maximize compression (zstd level 22)
```

## Benchmarks

### System: AMD Ryzen Threadripper 2950X (32 cores), 60 GiB RAM, Linux 6.12.

Measured before the memory limit, when the hash table was in a file by default and in RAM with `--ram`. The patches are the same now. Memory column shows peak anonymous RSS during diffing.

| Test case | New size | Patch size | Ratio | Patch time | Diff time | Memory | Diff time (RAM) | Memory (RAM) |
|-----------|----------|------------|-------|------------|-----------|--------|-----------------|--------------|
| Wine 4.18 &rarr; 4.19 | 201 MiB | 249 KiB | 0.12% | 0.13s | 0.42s | 21.5 MiB | 0.38s | 149 MiB |
| Linux 5.3 &rarr; 5.4 | 895 MiB | 6.8 MiB | 0.76% | 0.50s | 2.13s | 59.2 MiB | 1.81s | 563 MiB |
| Firefox 71.0b11 &rarr; b12 | 198 MiB | 10.9 MiB | 5.49% | 0.14s | 0.76s | 18.6 MiB | 0.73s | 147 MiB |
| Chrome 78.0.3904.97 &rarr; 108 | 145 MiB | 8.3 MiB | 5.71% | 0.11s | 0.79s | 16.9 MiB | 0.75s | 147 MiB |

#### With `--max` (zstd level 22)

Smaller patches at the cost of much slower diff times. Patch application speed is similar.

| Test case | New size | Patch size | Ratio | Patch time | Diff time | Memory | Diff time (RAM) | Memory (RAM) |
|-----------|----------|------------|-------|------------|-----------|--------|-----------------|--------------|
| Wine 4.18 &rarr; 4.19 | 201 MiB | 203 KiB | 0.10% | 0.12s | 3.7s | 60.9 MiB | 4.2s | 189 MiB |
| Linux 5.3 &rarr; 5.4 | 895 MiB | 6.1 MiB | 0.68% | 0.52s | 1m 4s | 60.6 MiB | 1m 6s | 573 MiB |
| Firefox 71.0b11 &rarr; b12 | 198 MiB | 8.3 MiB | 4.20% | 0.11s | 1m 2s | 62.5 MiB | 58.5s | 189 MiB |
| Chrome 78.0.3904.97 &rarr; 108 | 145 MiB | 5.6 MiB | 3.84% | 0.09s | 1m 18s | 57.6 MiB | 1m 21s | 186 MiB |

### System: Apple M5 Pro (18 cores), 64 GiB RAM, macOS 27.2.

Default settings (1 MiB chunks), with a 64 MiB memory limit and with none. Memory columns show peak anonymous RSS during diffing.

| Test case | New size | Patch size | Ratio | Patch time | Diff time | Memory | Diff time (no limit) | Memory (no limit) |
|-----------|----------|------------|-------|------------|-----------|--------|----------------------|-------------------|
| Wine 4.18 → 4.19 | 201.4 MiB | 249 KiB | 0.121% | 0.070s | 0.144s | 11.9 MiB | 0.154s | 139.8 MiB |
| Linux 5.3 → 5.4 | 894.7 MiB | 6.8 MiB | 0.762% | 0.426s | 1.500s | 14.8 MiB | 1.134s | 525.6 MiB |
| Firefox 71.0b11 → b12 | 197.8 MiB | 10.9 MiB | 5.486% | 0.110s | 0.826s | 14.7 MiB | 0.708s | 141.7 MiB |
| Chrome 78.0.3904.97 → 108 | 145.4 MiB | 57.0 MiB | 39.213% | 0.044s | 0.512s | 13.1 MiB | 0.510s | 13.0 MiB |

### Comparison with bidiff 1.1, bsdiff, and xdelta3

bidiff 1.1 (suffix arrays, single-threaded scan), bsdiff 4.3, xdelta3 3.0.11. Same test system.

#### Patch size

| Test case | New size | bidiff 2.0 | bidiff 1.1 | bsdiff | xdelta3 |
|-----------|----------|------------|------------|--------|---------|
| Wine 4.18 &rarr; 4.19 | 201 MiB | 249 KiB (0.12%) | 180 KiB (0.09%) | **110 KiB (0.05%)** | 256 KiB (0.12%) |
| Linux 5.3 &rarr; 5.4 | 895 MiB | 6.8 MiB (0.76%) | 6.2 MiB (0.69%) | **5.0 MiB (0.56%)** | 5.4 MiB (0.60%) |
| Firefox 71.0b11 &rarr; b12 | 198 MiB | 10.9 MiB (5.49%) | **7.2 MiB (3.66%)** | 7.8 MiB (3.95%) | 21.7 MiB (10.95%) |
| Chrome 78.0.3904.97 &rarr; 108 | 145 MiB | 8.3 MiB (5.71%) | **5.2 MiB (3.59%)** | 5.0 MiB (3.46%) | 18.7 MiB (12.87%) |

#### Diff time

| Test case | bidiff 2.0 | bidiff 1.1 | bsdiff | xdelta3 |
|-----------|------------|------------|--------|---------|
| Wine 4.18 &rarr; 4.19 | **0.42s** | 29.8s | 3m 8s | 1.0s |
| Linux 5.3 &rarr; 5.4 | **2.1s** | 4m 17s | 15m 6s | 8.5s |
| Firefox 71.0b11 &rarr; b12 | **0.76s** | 1m 37s | 4m 47s | 18.8s |
| Chrome 78.0.3904.97 &rarr; 108 | **0.79s** | 1m 23s | 3m 7s | 18.6s |

#### Patch time

| Test case | bidiff 2.0 | bidiff 1.1 | bsdiff | xdelta3 |
|-----------|------------|------------|--------|---------|
| Wine 4.18 &rarr; 4.19 | **0.13s** | 0.69s | 1.29s | 0.42s |
| Linux 5.3 &rarr; 5.4 | **0.50s** | 3.97s | 8.8s | 2.1s |
| Firefox 71.0b11 &rarr; b12 | **0.14s** | 0.84s | 2.4s | 1.5s |
| Chrome 78.0.3904.97 &rarr; 108 | **0.11s** | 0.53s | 1.5s | 1.1s |

bsdiff and bidiff 1.1 produce the smallest patches (suffix array matching) but are orders of magnitude slower to diff &mdash; minutes vs bidiff's sub-second times. xdelta3 is faster than bsdiff but still 2&ndash;25x slower than bidiff and produces the largest patches. bidiff 2.0 trades slightly larger patches for dramatically faster diffing and patching thanks to parallel hash-table scanning and parallel zstd decompression.

## Workspace structure

- **`bidiff`** (root) -- Library crate. Feature flags:
  - `diff` -- diffing engine (rayon, ringbuf, memmap2, tempfile)
  - `patch` -- patch encoding and application (integer-encoding, zstd, blake3)
  - `rayon` -- parallel hashing and parallel `apply_to_slice` (enabled by `diff`)
  - `mmap` -- `apply_to_file`, which applies into a memory-mapped output file of the declared size
  - `diff`, `patch` and `mmap` are enabled by default.
- **`bidiff-cli`** (`cli/`) -- CLI binary with `diff`, `patch`, `cycle`, and `info` subcommands.
- **`fuzz/`** -- a `cargo fuzz` target for patch application; see `fuzz/fuzz_targets/apply.rs`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
