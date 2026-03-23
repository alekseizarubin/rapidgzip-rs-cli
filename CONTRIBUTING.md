# Contributing

`rapidgzip-cli` is the user-facing command-line layer for the `rapidgzip`
stack.

## Development Layout

Current staging assumes a sibling checkout of `rapidgzip-rs`:

```text
rapidgzip-publication/
├── rapidgzip-rs/
└── rapidgzip-cli/
```

The CLI currently depends on `../rapidgzip-rs/crates/rapidgzip`.

## Typical Validation

```bash
cargo test
cargo build --release
```

When changing behavior that affects throughput, stdin handling, HTTP reads, or
index import/export, document the expected user-visible behavior in the pull
request.

## Scope Rules

Changes that touch native decoding internals, C++ bridge code, or upstream
vendor policy belong in `rapidgzip-rs` unless they are strictly packaging work
for the CLI repository.
