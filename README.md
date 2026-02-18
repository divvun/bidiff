# bidiff

A binary diff and patch library for Rust, distantly derived from the [bsdiff](https://www.daemonology.net/bsdiff/) algorithm.

Instead of suffix arrays, `bidiff` uses a hash-table index with:

- **File-backed mmap** by default -- the kernel can page the hash table to disk under memory pressure, keeping anonymous memory usage low
- **Optional `--ram` mode** for faster diffs when memory is plentiful
- **Parallel construction** via lock-free CAS insertion
- **Parallel scanning** via rayon with ring-buffer channels for streaming matches
- **Chunked patch format** with independent zstd-compressed chunks for parallel patch application

## Usage

```bash
# Create a patch
bidiff diff old_file new_file patch_file

# Apply a patch
bidiff patch old_file patch_file output_file

# Round-trip verification (diff then patch, verifies hash match)
bidiff cycle old_file new_file
```

### Options

```
--scan-chunk-mb <N>   Scan chunk size in MB (default: 1)
--block-size <N>      Hash index block size, minimum 4 (default: 32)
--threads <N>         Max threads for parallel scanning (default: all cores)
--ram                 Keep hash table in RAM (faster, uses more memory)
--max                 Maximize compression (zstd level 22)
```

## Benchmarks

System: AMD Ryzen Threadripper 2950X (32 cores), 60 GiB RAM, Linux 6.12.

Default settings (1 MiB chunks, file-backed hash table). Memory column shows peak anonymous RSS during diffing.

| Test case | New size | Patch size | Ratio | Patch time | Diff time | Memory | Diff time (RAM) | Memory (RAM) |
|-----------|----------|------------|-------|------------|-----------|--------|-----------------|--------------|
| Wine 4.18 &rarr; 4.19 | 201 MiB | 249 KiB | 0.12% | 125ms | 424ms | 21.5 MiB | 380ms | 149 MiB |
| Linux 5.3 &rarr; 5.4 | 895 MiB | 6.8 MiB | 0.76% | 503ms | 2,132ms | 59.2 MiB | 1,811ms | 563 MiB |
| Firefox 71.0b11 &rarr; b12 | 198 MiB | 10.9 MiB | 5.49% | 136ms | 757ms | 18.6 MiB | 730ms | 147 MiB |
| Chrome 78.0.3904.97 &rarr; 108 | 145 MiB | 8.3 MiB | 5.71% | 110ms | 789ms | 16.9 MiB | 751ms | 147 MiB |

### With `--max` (zstd level 22)

Smaller patches at the cost of much slower diff times. Patch application speed is similar.

| Test case | New size | Patch size | Ratio | Patch time | Diff time | Memory | Diff time (RAM) | Memory (RAM) |
|-----------|----------|------------|-------|------------|-----------|--------|-----------------|--------------|
| Wine 4.18 &rarr; 4.19 | 201 MiB | 203 KiB | 0.10% | 119ms | 3.7s | 60.9 MiB | 4.2s | 189 MiB |
| Linux 5.3 &rarr; 5.4 | 895 MiB | 6.1 MiB | 0.68% | 518ms | 64.3s | 60.6 MiB | 66.2s | 573 MiB |
| Firefox 71.0b11 &rarr; b12 | 198 MiB | 8.3 MiB | 4.20% | 114ms | 61.5s | 62.5 MiB | 58.5s | 189 MiB |
| Chrome 78.0.3904.97 &rarr; 108 | 145 MiB | 5.6 MiB | 3.84% | 92ms | 78.4s | 57.6 MiB | 80.8s | 186 MiB |

## Workspace structure

- **`bidiff`** (root) -- Library crate. Feature flags:
  - `diff` -- diffing engine (rayon, ringbuf, memmap2, tempfile)
  - `patch` -- patch encoding and application (integer-encoding, zstd)
  - Both enabled by default.
- **`bidiff-cli`** (`cli/`) -- CLI binary with `diff`, `patch`, and `cycle` subcommands.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
