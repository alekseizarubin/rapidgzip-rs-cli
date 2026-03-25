# Contributing

`rapidgzip-rs-cli` is the user-facing command-line layer for the `rapidgzip`
stack.

## Build Requirements

Local development currently requires:

- a current Rust toolchain
- `cmake`
- `nasm`
- on Linux, standard C/C++ build tooling and zlib development headers

## Typical Validation

```bash
cargo test
cargo build --release
```

## Scope Rules

This repository owns the user-facing CLI behavior, documentation, packaging,
and release automation.

When changing behavior that affects throughput, stdin handling, HTTP reads, or
index import/export, document the expected user-visible behavior in the pull
request.

Changes that touch native decoding internals, C++ bridge code, or upstream
vendor policy belong in `rapidgzip-rs` unless they are strictly packaging work
for the CLI repository.
