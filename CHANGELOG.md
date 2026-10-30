# Changelog

All notable changes to Memvid will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.3.0] - 2026-10-30

### Added
- `memvidd` `add_document` skips a document whose text the store already holds and reports `status`
  (`added` or `unchanged`) and `same_name`.

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

### Security
- Embedded WAL prevents data corruption
- Atomic commits ensure consistency
- File locking prevents concurrent write conflicts

## [2.2.0] - 2026-10-30

### Added
- memvidd search results carry a `similarity` value (cosine to the query) that is comparable between memories, so results from several memories can be ranked together.

## [2.1.0] - 2026-10-30

### Added
- memvidd semantic search understands Czech and about 100 other languages, including questions asked in one language about documents written in another.
- memvidd extracts ODT, ODS, ODP, EPUB, RTF and XLSB files, readable text from HTML, and the macro and template variants of Word, Excel and PowerPoint files.
- `memvidd --reembed <file.mv2>...` rebuilds memories indexed with the previous embedding model.

### Changed
- Building now requires Rust 1.88 or newer (was 1.85).
- PDF text extraction orders some pages differently, so newly ingested PDFs can chunk and rank slightly differently. Existing `.mv2` files are unaffected.
- memvidd needs the multilingual-e5-base model files (see `docs/memvidd.md`). Memories indexed with the previous model are searched by keywords only and accept no new documents until they are rebuilt with `memvidd --reembed`.

### Fixed
- memvidd no longer fails a search whose question contains `*`, `?`, `!`, quotes, brackets or other query syntax, or whose words match nothing exactly.
- memvidd splits long `.txt`, `.md`, `.json` and other text files into chunks, so semantic search covers the whole file instead of only its beginning.

### Security
- Ingesting a crafted PDF, DOCX, PPTX, XLSX or XLS file can no longer crash the process or exhaust its memory (RUSTSEC-2026-0187, RUSTSEC-2026-0194, RUSTSEC-2026-0195).
- Resolved further advisories in dependencies: `bytes`, `crossbeam-epoch`, `lz4_flex`, `memmap2`, `rand`, `time`, `webpki-root-certs` and the build-time `tar`.

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
