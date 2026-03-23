# rapidgzip-cli

`rapidgzip-cli` is the standalone command-line repository for the `rapidgzip`
stack.

It sits in the middle of the publication pyramid:

1. `rapidgzip-rs`
2. `rapidgzip-cli`
3. `rapidgzip-benchmarks`

Companion repositories:

- Library: `rapidgzip-rs` (`https://github.com/alekseizarubin/rapidgzip-rs`)
- Benchmarks: `rapidgzip-benchmarks` (`https://github.com/alekseizarubin/rapidgzip-benchmarks`)

Replace `alekseizarubin` before the first public push if the final GitHub namespace
changes.

## Current Status

This repository is extracted from the staging monorepo and currently depends on
its sibling checkout of `rapidgzip-rs`:

```text
rapidgzip-publication/
├── rapidgzip-rs/
└── rapidgzip-cli/
```

Before the first public release, switch the dependency in [Cargo.toml](Cargo.toml)
from the local path to either:

- a published crates.io version of `rapidgzip`, or
- a pinned public git tag from `rapidgzip-rs`

## Binary Builds

The repository is prepared to build release binaries for these GitHub-hosted
runner targets:

- Linux x86_64
- Linux arm64
- macOS x86_64
- macOS arm64
- Windows x86_64
- Windows arm64

Current release workflow behavior:

- every tagged `v*` release builds all platform binaries from source
- non-Windows targets are packaged as `.tar.gz`
- Windows targets are packaged as `.zip`
- packaged archives are uploaded both as workflow artifacts and GitHub Release assets

## Features

- parallel decompression of local `.gz` and `.bgz` files
- index import and export for fast repeat reads
- stdin support via temporary-file spooling for seekable decode
- HTTP range-reader support for remote inputs
- quiet benchmark mode with native fast-path discard

## Local Development

Current staging build assumes the sibling repository layout shown above.

```bash
git clone https://github.com/alekseizarubin/rapidgzip-rs rapidgzip-rs
git clone https://github.com/alekseizarubin/rapidgzip-cli rapidgzip-cli
cd rapidgzip-cli
cargo test
cargo build --release
```

## Usage

```bash
cargo run --release -- input.fastq.gz --quiet --parallelism 22
cargo run --release -- input.fastq.bgz --import-index reads.gzi --quiet
cargo run --release -- https://example.org/input.fastq.gz --quiet
```

## Repository Scope

This repository owns:

- the end-user CLI source and tests
- user-facing usage and installation documentation
- release automation for cross-platform CLI binaries

Performance-sensitive decoding changes belong in `rapidgzip-rs`.

## Publishing Backlog

See [docs/PUBLISHING_BACKLOG.md](docs/PUBLISHING_BACKLOG.md).
