# rapidgzip-cli Publishing Backlog

## Before First Public Push

- replace the local path dependency in `Cargo.toml` with a public dependency
  model based on either crates.io or a pinned `rapidgzip-rs` git tag
- replace README placeholder GitHub URLs with the final namespace
- decide on the public binary naming strategy (`rapidgzip-cli` vs `rapidgzip`)
- verify that the pinned `RAPIDGZIP_RS_REF` exists in the public `rapidgzip-rs`
  repository before enabling GitHub Actions
- add installation instructions for GitHub Releases and, if desired,
  `cargo install`

## Before First Binary Release

- confirm the release workflow on Linux x86_64, Linux arm64, macOS x86_64,
  macOS arm64, Windows x86_64, and Windows arm64
- add release notes and changelog conventions
- decide whether to sign release artifacts
- define minimum smoke-test coverage for local files, stdin, and HTTP inputs
