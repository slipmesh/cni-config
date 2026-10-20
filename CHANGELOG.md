# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and follows [Semantic Versioning](https://semver.org/).

## [0.1.6] - 2026-10-20

### Build system 🛠️

- Bump serde_json to 1.0.154 and tokio to 1.54.0

### CI/CD ⚙️

- Build each arch natively instead of with cargo-zigbuild
- Test on the shipped musl target and release the image main built

### Fixed 🐛

- Exit on SIGTERM and replace the conflist atomically

## [0.1.5] - 2026-10-19

### Added ✨

- Log the live heap and resident set every ten minutes
- Keep the memory report at debug level

## [0.1.4] - 2026-10-01

### Build system 🛠️

- Stop tracking Cargo.lock
- Take the latest releases

### Documentation 📚

- Have the release commit move the manifest's image tag too

### Fixed 🐛

- Log the binary's own name and version at start
- Carry the released version in Cargo.toml

## [0.1.3] - 2026-08-27

### Miscellaneous 🧹

- Move markdownlint config to the cli2 file

## [0.1.2] - 2026-08-26

### CI/CD ⚙️

- Build each arch on a runner of its own architecture

### Documentation 📚

- Describe the current routing contract and drop private references
- Describe the routing contract as it is
- Keep the provenance, drop links to repositories that no longer exist

### Miscellaneous 🧹

- Ship the license texts Cargo.toml already claims
- Exclude build/ from markdownlint too

## [0.1.1] - 2026-08-25

### Fixed 🐛

- Never ship a floating image tag, in the repo or in the release asset

## [0.1.0] - 2026-08-11

### Added ✨

- Initial implementation - CNI conflist writer DaemonSet

### Miscellaneous 🧹

- Generate CHANGELOG.md via git-cliff
