# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-rc.1] - 2026-09-01

### Added

- Program-owned Windows service for durable scheduling, backup, verification,
  retention, repair, notification, and recovery work.
- PostgreSQL, MySQL, MariaDB, and SQLite providers with whole-database and
  explicit-table backup scopes where supported.
- Signed manifests, required-replica evidence, encrypted streaming artifacts,
  bounded recovery, and crash reconciliation.
- Tauri control-plane Console with bounded inventory pages, fail-closed
  mutation gates, health projections, backup scope editing, and recovery flows.
- Hardened Windows installation, repair, update, and uninstall contracts with
  package-manifest, native-tool, ACL, and exact-artifact verification.
- Korean, English, and Japanese README guides with deterministic Console
  screenshots and an MIT license owned by dannykkh.

### Security

- Native backup tools and dependency closures are bound to verified package
  identities and held through supervised Windows Job Object execution.
- IPC mutations use durable idempotency records and ambiguous outcomes fail
  closed instead of replaying destructive operations.
- Restore, retention, and native process operations enforce bounded deadlines,
  cancellation, fencing, and authoritative pre-destructive revalidation.

### Known limitations

- This release candidate supports Windows only. macOS and Linux installers and
  runtimes are not available.
- A public production-signed installer is not yet published. Production
  promotion remains parked until signing, exact-artifact qualification, and
  operational stop metrics are approved.
- SQL Server remains a candidate provider and is not enabled for production
  backup execution.
- This release candidate must not be the sole production backup mechanism.
