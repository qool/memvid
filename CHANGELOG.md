# Changelog

All notable changes to Memvid will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial public release of Memvid core library
- Single-file `.mv2` format for portable AI memory
- Full-text search with BM25 ranking (Tantivy)
- Vector similarity search with HNSW
- PDF, DOCX, XLSX document ingestion
- CLIP visual embeddings for image search
- Whisper audio transcription
- Timeline queries for chronological browsing
- Crash-safe WAL-based writes
- Blake3 checksums for data integrity
- Ed25519 signatures for authenticity
- Optional AES-256-GCM encryption

### Changed
- Building now requires Rust 1.88 or newer (was 1.85).
- PDF text extraction orders some pages differently, so newly ingested PDFs can chunk and rank slightly differently. Existing `.mv2` files are unaffected.

### Security
- Ingesting a crafted PDF, DOCX, PPTX, XLSX or XLS file can no longer crash the process or exhaust its memory (RUSTSEC-2026-0187, RUSTSEC-2026-0194, RUSTSEC-2026-0195).
- Resolved further advisories in dependencies: `bytes`, `crossbeam-epoch`, `lz4_flex`, `memmap2`, `rand`, `time`, `webpki-root-certs` and the build-time `tar`.
- Embedded WAL prevents data corruption
- Atomic commits ensure consistency
- File locking prevents concurrent write conflicts

## [2.0.0] - 2026-01-05

### Added
- Complete rewrite in Rust for performance and safety
- New `.mv2` file format (single-file, no sidecars)
- Append-only frame-based architecture
- Built-in full-text and vector search
- Cross-platform support (macOS, Linux, Windows)

### Changed
- Migrated from Python to Rust
- New API design focused on simplicity
- Improved memory efficiency

### Removed
- Legacy Python implementation
- QR code video encoding (replaced with efficient binary format)

---

[Unreleased]: https://github.com/memvid/memvid/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/memvid/memvid/releases/tag/v2.0.0
