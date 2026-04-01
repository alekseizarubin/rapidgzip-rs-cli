# rapidgzip-rs-cli

`rapidgzip-rs-cli` is a cross-platform command-line decompressor for `.gz` and
`.bgz` data built on top of
[`rapidgzip-rs`](https://github.com/alekseizarubin/rapidgzip-rs).

It targets users who want a fast gzip-compatible decoder with explicit control
over parallelism, index import/export, and native I/O strategy.

Companion repositories:

- Library: [`rapidgzip-rs`](https://github.com/alekseizarubin/rapidgzip-rs)
- Benchmarks: [`rapidgzip-rs-benchmarks`](https://github.com/alekseizarubin/rapidgzip-rs-benchmarks)

## Why use this

Compared to `gzip`, `gunzip`, or `pigz`, `rapidgzip-rs-cli` focuses on:

- fast parallel decompression of large `.gz` and `.bgz` files
- explicit import/export of indexes for repeat reads and random-access-friendly workflows
- native I/O mode selection for HDD-oriented sequential reads or SSD-friendly positioned reads
- support for local files, stdin, and compatible HTTP range sources through one CLI surface

## Features

- parallel decompression of local `.gz` and `.bgz` files
- index import from a local file or an HTTP/HTTPS URL (`--import-index`)
- index export to a local file with atomic write and symlink-safe replacement
- stdin support via temporary-file spooling for seekable decode
- HTTP and HTTPS input for compatible remote gzip sources
- benchmark-only mode with native fast-path discard
- explicit native I/O mode selection
- explicit control over in-memory index retention
- counting modes for bytes and lines

## Installation

From crates.io:

```bash
cargo install rapidgzip-rs-cli
```

From a GitHub release:

- download the archive for your platform from the repository releases page
- unpack it
- place `rapidgzip-rs-cli` or `rapidgzip-rs-cli.exe` on your `PATH`

## Quick Start

Decompress a local file into an inferred output path:

```bash
rapidgzip-rs-cli reads.fastq.gz
```

Write to an explicit output file:

```bash
rapidgzip-rs-cli reads.fastq.gz -o reads.fastq
```

Write to stdout explicitly:

```bash
rapidgzip-rs-cli -c reads.fastq.gz | head
```

Benchmark without writing decompressed output:

```bash
rapidgzip-rs-cli reads.fastq.gz --benchmark-only -P 22
```

Count lines in the decompressed stream:

```bash
rapidgzip-rs-cli reads.fastq.gz --count-lines
```

## Behavior Notes

- local file inputs default to an inferred output path such as `reads.fastq.gz -> reads.fastq`
- stdin and HTTP inputs require an explicit `--output <PATH>` or `--stdout` because they have no safe default output path
- stdin is spooled into a temporary file before decode because parallel decode requires a seekable input
- HTTP gzip input requires a server that provides `Content-Length` and HTTP 206 Partial Content (byte-range support)
- `--import-index` accepts both local file paths and HTTP/HTTPS URLs; a URL index is downloaded with a plain GET request, so range support is not required on the index server
- HTTP input without an imported index defaults to sequential buffered reads in `auto` mode to avoid redundant range requests
- URL input without an imported index can use significantly more memory when `-P` is greater than 1; prefer `-P 1` or import an index if memory matters
- local files, stdin, and HTTP inputs share the same CLI surface, but their I/O behavior is not identical

### Web Mode Performance

HTTP decompression is fully functional for both indexed and non-indexed inputs.
Optimal throughput depends on your connection characteristics:

- **Without an index** (`auto` mode): sequential reads are used automatically —
  one continuous HTTP stream with minimal overhead. This is the recommended mode
  for most HTTP servers.
- **With an index**: parallel mode is enabled. Each worker issues independent HTTP
  range requests, so more threads do not always mean higher throughput. On a
  typical internet connection `-P 2` or `-P 4` often performs better than higher
  counts; on high-latency links `-P 1` with `--io-read-mode sequential` is
  frequently fastest.
- **`--chunk-size`** controls how much data each HTTP range request fetches. For
  slow connections a larger value (e.g. `--chunk-size 16777216`) reduces
  per-request overhead; for fast local-network servers a smaller value improves
  responsiveness. The default (4 MiB) is a reasonable starting point.

If throughput is lower than expected, try adjusting `-P` and `--chunk-size` for
your specific connection speed before assuming a bug.

## Platform Support

Current release expectations:

- Linux `x86_64` and `aarch64`: tested in CI
- macOS `x86_64` and `aarch64`: tested in CI
- Windows `x86_64`: tested in CI
- Windows `aarch64`: build-verified in CI

The release workflows still build cross-platform binaries for the broader target matrix.

## Build Requirements

Local builds require:

- a current Rust toolchain
- `cmake`
- `nasm`
- on Linux, standard C/C++ build tooling and zlib development headers

The CI workflows install these dependencies explicitly on Linux, macOS, and Windows.

## Local Development

```bash
git clone https://github.com/alekseizarubin/rapidgzip-rs-cli rapidgzip-rs-cli
cd rapidgzip-rs-cli
cargo test
cargo build --release
```

## Scope

This repository owns:

- the end-user CLI source and tests
- user-facing usage and installation documentation
- release automation for cross-platform CLI binaries

Performance-sensitive decoding changes belong in `rapidgzip-rs`.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license
