# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
