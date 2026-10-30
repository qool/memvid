# memvidd — arxcore-v2 sidecar

`memvidd` is a small binary target in this crate that wraps `memvid-core` behind a
line-oriented JSON protocol, so a non-Rust host (the arxcore-v2 Node server) can create,
fill and query a `.mv2` file without linking Rust.

- Branch: `mikulas/memvidd-sidecar`
- Source: [`src/bin/memvidd.rs`](../src/bin/memvidd.rs)
- Wire protocol: `arxcore-v2/docs/SIDECAR_PROTOCOL.md` (newline-delimited JSON on stdin/stdout,
  logs on stderr)

## Rebase notes (2026-09-14)

Rebased onto `upstream/main` (115 commits, `2.0.133` -> `2.0.140`; upstream has since tagged up
to `v2.0.157` - this branch tracks whatever `upstream/main` resolved to at rebase time, not
every tag). One conflict (`Cargo.toml` - upstream and this branch both appended new
`[[example]]`/`[[bin]]` entries after `text_embedding`; resolved by keeping both). One compile
break: `SearchRequest` gained `acl_context: Option<AclContext>` and
`acl_enforcement_mode: AclEnforcementMode` (upstream's new frame-level ACL feature, unrelated to
anything we use) - fixed with `acl_context: None` and `acl_enforcement_mode: Default::default()`
(`Audit` mode, which never blocks a result) in the one place `run_query` builds a request.
Nothing else needed to change; full regression (basic CRUD, hybrid search + dedup, encryption
round trip, live end-to-end through arxcore-v2 + Ollama) re-verified green after the fix.

Follow-up (same day): adopted the native reader registry for document extraction — see
**Native document extraction** below. `arxcore-v2`'s own `mammoth`/`pdf-parse`/`exceljs`
dependencies are gone as a result.

Noteworthy upstream changes still NOT adopted (tracked for a future pass): stricter
vector-index/model binding (`80a2544`) — the WAL corruption and stale-`frame_id` fixes were
already picked up for free by the rebase itself. (Adopted 2026-10-30 together with the
multilingual model, see **Model binding and re-embedding**.)

## Security dependency bump (2026-10-30)

Upstream has not released a fix. As of 2026-10-30 the newest `memvid-core` on crates.io is
2.0.140 (2026-05-27), and the `@memvid/sdk`, `memvid-cli` and PyPI `memvid-sdk` 2.0.160 builds
from the same day embed the same vulnerable parser versions (checked in the SDK's native
binary). This branch therefore bumps them directly (upstream issue #242; upstream PR #256 covers
only `lopdf` and `pdf-extract`):

| Crate | Was | Now | Advisory |
|---|---|---|---|
| `lopdf` | 0.38 / 0.39 | 0.42 | RUSTSEC-2026-0187: stack overflow on deeply nested PDF objects |
| `pdf-extract` | 0.10 | 0.12 | pulled in the old `lopdf` |
| `quick-xml` | 0.30 / 0.31 | 0.41 | RUSTSEC-2026-0194 / -0195: quadratic time, unbounded allocation |
| `calamine` | 0.22 | 0.36 | pulled in the old `quick-xml` |

The versions were picked so the build tree has one version of each crate. A `quick-xml` 0.31
remains in `Cargo.lock` only through the optional `pdf_oxide` feature, which memvidd does not
build. `calamine` 0.36 needs Rust 1.88, so `rust-version` moved from 1.85 to 1.88 (the pinned
toolchain is still 1.90). The same pass took semver-compatible lockfile patches for `bytes`,
`crossbeam-epoch`, `lz4_flex`, `memmap2`, `rand`, `time`, `webpki-root-certs` and the build-time
`tar`, with no code changes.

Code adaptations, all in `src/reader/`:

- quick-xml 0.41 reports every entity reference (`&amp;`, `&#233;`) as a separate
  `Event::GeneralRef`, and `BytesText::unescape` is gone. The DOCX and PPTX readers now join
  the pieces of a text node back together before trimming, so `Tom &amp; Jerry` still comes out
  as `Tom & Jerry`. A unit test in each reader covers this.
- `trim_text` moved to `reader.config_mut()`. Attribute values use
  `decoded_and_normalized_value(XmlVersion::Implicit1_0, ...)`, which is what the deprecated
  `decode_and_unescape_value` delegates to.
- calamine replaced the `DataType` enum with `Data`, `worksheet_range` returns a `Result`
  instead of `Option<Result>`, and `DateTime` and `Duration` merged into
  `DateTime(ExcelDateTime)`. Output is unchanged, because both were already rendered as the
  same number.

Verified by running the old and the new memvidd on the same inputs. DOCX, PPTX, XLSX and
`bitcoin.pdf` give identical search hits, and the total frame count is unchanged. The one
behavioral difference is PDF text extraction in `pdf-extract` 0.12, which orders some page text
differently, so newly ingested PDFs can chunk and rank slightly differently. Existing `.mv2`
files are not touched.

Deliberately left for later, because the vulnerable code paths are not reachable from memvidd:

- `zip` 7.2 / 8.6 (fixed in 9.0.5): the advisories concern extracting archives to disk,
  symlinks, permissions and legacy decoders. memvid only reads entries in memory and builds
  `zip` with deflate only.
- `zstd` 0.13 / `zstd-safe` 7.2 (fixed in 0.14 / 8.2): memvid only calls `encode_all` and
  `decode_all`, none of the affected APIs (seekable, dictionaries, multithreaded contexts).
- `lru` 0.12 comes from `tantivy` and can only move with it.
- `bincode`, `paste`, `ttf-parser` and `block-buffer` have advisories without a fixed release,
  mostly "unmaintained".

When rebasing onto an upstream release that already contains these bumps, prefer upstream's
`Cargo.toml` versions and drop the ones from this pass.

## Build

```bash
cargo build --features vec --bin memvidd            # debug -> target/debug/memvidd
cargo build --release --features vec --bin memvidd  # -> target/release/memvidd
```

The `[[bin]]` requires the `vec` feature to compile at all (it links ONNX Runtime and
`tokenizers`, which are feature-gated) - see **Semantic search** below for why that doesn't
mean an embedding model is mandatory at runtime.

The binary lives in `src/bin/memvidd/`: `main.rs` (protocol), `embed.rs` (the embedding
model), `formats.rs` (file extraction) and `reembed.rs` (the `--reembed` mode, see **Model
binding and re-embedding**).

`encryption` (`.mv2e` password-based encryption, see **Encryption** below) is an additional
opt-in feature on top of `vec`, not a replacement for it:

```bash
cargo build --features "vec encryption" --bin memvidd
```

A `--features vec` build (no `encryption`) behaves identically to before `encryption` existed —
same `open` result shape, and a `passphrase` param is rejected with `ENCRYPTION_NOT_BUILT`
rather than silently ignored.

## Methods

| method | params | result |
|---|---|---|
| `ping` | `{}` | `{pong, version, embeds:"internal"\|"none", embed_model}` - `embed_model` is `"multilingual-e5-base"`, or `null` when running lexical-only |
| `open` | `{path, create, passphrase?}` | `{path, documents, created, vector_search, needs_reembed, encrypted?}` - `needs_reembed` marks a store whose vectors come from another embedding model (see **Model binding and re-embedding**); `vector_search` is true when semantic search is on for this store. Closes any previously-open store first (so re-opening the same path is safe), then sets the one active store; `create:true` makes+commits an empty `.mv2` when the file is absent. `passphrase` treats `path` as an encrypted `.mv2e` capsule — see **Encryption** below; `encrypted` is only present in `encryption`-feature builds |
| `add_document` | `{content?, content_base64?, metadata:{source,...}}` | `{chunks:1, seq}` — exactly one of `content` (plain UTF-8) / `content_base64` (raw file bytes, natively extracted — see **Native document extraction**) is required. `chunks` is always 1 here (one ingest call per document); `memvid-core` may still split the document into several search-indexed frames internally — see **Chunking** below. Every metadata key is stored as a frame tag |
| `commit` | `{}` | `{documents}` — in encrypted mode, also re-locks the durable `.mv2e` (see **Encryption**) |
| `search` | `{query, k?}` | `{hits:[{score, content, metadata:{source,uri,frame_id,rank,matches,similarity}}], total_hits, elapsed_ms}` - `score` is only `1/rank` of the fused list; `similarity` is the cosine between the query and the hit (`null` without an embedder), see **Cross-memory comparable similarity** |
| `stats` | `{}` | `{path, documents, vector_search, needs_reembed}` - about the *active* store |
| `inspect` | `{path}` | `{path, documents: int\|null}` — reads any `.mv2`'s document count via `Memvid::open_read_only` (a shared, not exclusive, lock) without disturbing the active store; `null` (not an error) for anything unreadable |
| `close` | `{}` | `{}` — drops the active store, process stays up; in encrypted mode, best-effort deletes the plaintext scratch file (see **Encryption**) |

Errors: `{"id":N,"ok":false,"error":{"message","code"}}` with `code` in
`BAD_PARAMS | MV2_NOT_OPEN | IO_ERROR | INTERNAL | BAD_PASSPHRASE | ENCRYPTION_NOT_BUILT |
NEEDS_REEMBED`. `BAD_PASSPHRASE` and `ENCRYPTION_NOT_BUILT` only occur when `open` is called
with a `passphrase` - see **Encryption**. `NEEDS_REEMBED` is `add_document` on a store with
`needs_reembed: true`.

## Smoke test

```bash
printf '%s\n' \
  '{"id":1,"method":"ping","params":{}}' \
  '{"id":2,"method":"open","params":{"path":"/tmp/smoke.mv2","create":true}}' \
  '{"id":3,"method":"add_document","params":{"content":"Payment terms are net 30 days from invoice date.","metadata":{"source":"contract.txt"}}}' \
  '{"id":4,"method":"add_document","params":{"content":"The warranty period is 24 months for hardware defects.","metadata":{"source":"warranty.txt"}}}' \
  '{"id":5,"method":"commit","params":{}}' \
  '{"id":6,"method":"search","params":{"query":"invoice payment","k":3}}' \
  '{"id":7,"method":"close","params":{}}' \
  | ./target/debug/memvidd
```

Verified run (release build, 2026-09-11, model present -> `embeds=internal`):

```
[memvidd] semantic search ready (bge-small-en-v1.5, 384d)
[memvidd] ready (v2.0.133, embeds=internal)
{"id":1,"ok":true,"result":{"embeds":"internal","pong":true,"version":"2.0.133"}}
```

`content` has `memvid-core`'s metadata footer stripped (see `clean_snippet`).

Re-verified (debug build, 2026-09-11, after adding `encryption` support) — `open`'s result
shape for a `--features vec` build (no `passphrase`) is unchanged, byte for byte:

```
{"id":1,"ok":true,"result":{"embeds":"internal","pong":true,"version":"2.0.133"}}
{"id":2,"ok":true,"result":{"created":true,"documents":0,"path":"/tmp/smoke.mv2"}}
{"id":3,"ok":true,"result":{"chunks":1,"seq":2}}
{"id":4,"ok":true,"result":{"chunks":1,"seq":3}}
{"id":5,"ok":true,"result":{"documents":2}}
{"id":6,"ok":true,"result":{"elapsed_ms":79,"hits":[{"content":"Payment terms are net 30 days from invoice date.","metadata":{"frame_id":0,"matches":6,"rank":1,"source":"contract.txt","uri":"mv2://doc/contract.txt"},"score":1.0},{"content":"The warranty period is 24 months for hardware defects.","metadata":{"frame_id":1,"matches":0,"rank":2,"source":"warranty.txt","uri":"mv2://doc/warranty.txt"},"score":0.5}],"total_hits":2}}
{"id":7,"ok":true,"result":{}}
```

No `encrypted` key on `open`'s result — that field only appears in `encryption`-feature
builds (see **Encryption**).

## Encryption

Optional password-based encryption for the durable store, built on `memvid-core`'s
`encryption` feature (`src/encryption/capsule.rs`: `lock_file` / `unlock_file`). It encrypts
the **whole `.mv2` file as one capsule** (`.mv2e`, Argon2id-derived key, AES-256-GCM), not
individual frames — see `tests/encryption_capsule.rs` for the underlying capsule contract.

Compiled in only with `--features "vec encryption"` (see **Build**). A `--features vec` build
does not link the crypto dependencies at all; it rejects any `open` call that includes
`passphrase` with `ENCRYPTION_NOT_BUILT` instead of silently ignoring it.

### Protocol addition

`open` gains an optional `params.passphrase: string`. Its presence is what makes a store
"encrypted mode" for that session — there is no separate flag:

- `create:true`, file absent: create+commit a plaintext `.mv2` (in a temp scratch path), then
  immediately `lock_file` it to the target `path` — so the durable `.mv2e` exists on disk right
  after `open`, matching the plaintext path's existing behavior of leaving a durable `.mv2` on
  disk right after create (don't wait for an explicit `commit`).
- file present (`create` true or false — same as the plaintext path, existence wins):
  `unlock_file` it with the given passphrase into a temp plaintext `.mv2`, then `Memvid::open`
  that temp file. `memvid-core` cannot open a `.mv2e` capsule directly — `Memvid::open` on one
  returns `MemvidError::EncryptedFile` by design (fast-path magic-byte check, independent of
  the `encryption` feature) — the sidecar always unlocks first.
- Wrong passphrase -> `EncryptionError::Decryption` -> mapped to `BAD_PASSPHRASE` (not
  `IO_ERROR`). The error message ("Decryption failed - invalid password or corrupted file")
  never includes the passphrase itself — nor does any other `EncryptionError` variant's
  message, all of which fall back to `IO_ERROR`.
- No `passphrase` in `open` -> today's plaintext `.mv2` path, **completely unchanged**,
  including opening a `.mv2e` file by mistake: that still fails with the pre-existing
  `IO_ERROR` / `MemvidError::EncryptedFile` message ("This file is encrypted: ...") — clearly,
  not a crash, but not given its own error code, since this path is intentionally untouched.
- `open`'s result gains `"encrypted": bool` (only in `encryption`-feature builds — see above)
  so the host knows which mode a store ended up in.

### Lifecycle: plaintext scratch file

`memvid-core` only ever reads/writes a plaintext `.mv2`. In encrypted mode `memvidd` keeps one
alive at a unique path under the OS temp dir (`std::env::temp_dir()`, not next to the `.mv2e`)
for the life of the session:

1. `open` (existing file): `unlock_file(target, scratch, passphrase)`, then `Memvid::open(scratch)`.
2. `commit`: the normal `mem.commit()` flushes to `scratch`, then — encrypted mode only —
   `lock_file(scratch, target, passphrase)` re-encrypts it back over the durable `.mv2e`
   **immediately**, not deferred to `close`. So the durable file is never more than one commit
   stale, even if the process is killed right after.
3. `close` (and the "release previous store" step `open` runs before opening anything new):
   best-effort `std::fs::remove_file(scratch)` — a plain file delete, **not a secure wipe**.
   Plaintext content may still be recoverable from disk (SSD wear-leveling, filesystem
   snapshots, swap) after this. A failure to delete is logged as a warning to stderr, not a
   hard error — `close`/`open` still succeed.
4. The passphrase itself is held in memory only for the session (`EncryptionSession`,
   zeroized via `zeroize` on drop — at `close` or when `open` releases the previous store) and
   is never written to stderr/stdout.

### Error codes

| code | meaning |
|---|---|
| `BAD_PASSPHRASE` | `open` with a `passphrase` that doesn't decrypt the target `.mv2e` (wrong password or corrupted file — AES-GCM can't tell which) |
| `ENCRYPTION_NOT_BUILT` | `open` was called with a `passphrase` but this binary was built without `--features encryption` |

### Naming

The sidecar does not infer `.mv2` vs `.mv2e` from the file extension — it only looks at whether
`passphrase` was given. The host is expected to use a `.mv2e` extension for encrypted stores by
convention (that's what `lock_file`'s own default output extension is, when no explicit output
path is given — not exercised here since `memvidd` always passes an explicit `path`).

### Smoke test

```bash
printf '%s\n' \
  '{"id":1,"method":"ping","params":{}}' \
  '{"id":2,"method":"open","params":{"path":"/tmp/smoke_enc.mv2e","create":true,"passphrase":"correct-horse-battery-staple"}}' \
  '{"id":3,"method":"add_document","params":{"content":"Payment terms are net 30 days from invoice date.","metadata":{"source":"contract.txt"}}}' \
  '{"id":4,"method":"commit","params":{}}' \
  '{"id":5,"method":"close","params":{}}' \
  | ./target/debug/memvidd
```

Verified (debug build, 2026-09-11):

```
{"id":1,"ok":true,"result":{"embeds":"internal","pong":true,"version":"2.0.133"}}
{"id":2,"ok":true,"result":{"created":true,"documents":0,"encrypted":true,"path":"/tmp/smoke_enc.mv2e"}}
{"id":3,"ok":true,"result":{"chunks":1,"seq":2}}
{"id":4,"ok":true,"result":{"documents":1}}
{"id":5,"ok":true,"result":{}}
```

On-disk file starts with the `MV2E` magic, not `MV2\0`, and `file /tmp/smoke_enc.mv2e` reports
`data` (not recognizable/text content) — confirmed not plaintext. No leftover scratch file
under `$TMPDIR` after `close`.

Reopen with the **correct** passphrase — finds the document:

```
{"id":1,"method":"open","params":{"path":"/tmp/smoke_enc.mv2e","passphrase":"correct-horse-battery-staple"}}
{"id":1,"ok":true,"result":{"created":false,"documents":1,"encrypted":true,"path":"/tmp/smoke_enc.mv2e"}}
{"id":3,"method":"search","params":{"query":"invoice payment","k":3}}
{"id":3,"ok":true,"result":{"elapsed_ms":75,"hits":[{"content":"Payment terms are net 30 days from invoice date.", ...}],"total_hits":1}}
```

Reopen with the **wrong** passphrase — fails cleanly, does not crash, does not silently
succeed:

```
{"id":2,"method":"open","params":{"path":"/tmp/smoke_enc.mv2e","passphrase":"totally-wrong-password"}}
{"id":2,"ok":false,"error":{"code":"BAD_PASSPHRASE","message":"Decryption failed - invalid password or corrupted file"}}
```

Reopen with **no** passphrase at all on the encrypted file — existing (unchanged)
`MemvidError::EncryptedFile` path, fails clearly, not a crash:

```
{"id":2,"method":"open","params":{"path":"/tmp/smoke_enc.mv2e"}}
{"id":2,"ok":false,"error":{"code":"IO_ERROR","message":"open failed: This file is encrypted: /tmp/smoke_enc.mv2e\nRun: memvid unlock /tmp/smoke_enc.mv2e"}}
```

`passphrase` on a `--features vec` (no `encryption`) build — rejected, not ignored:

```
{"id":2,"method":"open","params":{"path":"/tmp/smoke_enc.mv2e","passphrase":"whatever"}}
{"id":2,"ok":false,"error":{"code":"ENCRYPTION_NOT_BUILT","message":"this memvidd build was compiled without encryption support; rebuild with --features \"vec encryption\" to open a passphrase-protected store"}}
```

A second `open`+`add_document`+`commit` cycle against the same `.mv2e` (`create:true`, file now
already exists -> `created:false`, unlocks+opens the existing capsule instead of overwriting
it) confirmed `commit`'s re-lock correctly **overwrites** an already-existing target path:
`documents` went from 1 to 2, and a final reopen after that showed `documents:2` durably on
disk.

### memvid-core API surprises for the Node integration

- `Memvid::open` on a `.mv2e` file's bytes always returns `MemvidError::EncryptedFile` — this
  check is a fast-path magic-byte read, unconditional on the `encryption` feature. There's no
  way to get a different (e.g. generic decode) error by opening an encrypted file directly;
  always unlock first.
- `lock_file`'s and `unlock_file`'s `output` path **does not need to not-exist** — both go
  through `write_atomic` (`atomic-write-file` crate: write to a same-directory temp file, then
  atomic rename on success), so re-locking over an already-existing `.mv2e` (every `commit`
  after the first) overwrites it cleanly. Verified above (second commit cycle).
- The **parent directory of `output`** does need to exist — `write_atomic` does not create it.
  `memvidd` already `create_dir_all`s the target's parent on `create:true`, matching the
  plaintext path; the temp plaintext scratch path is always directly under
  `std::env::temp_dir()`, which the OS guarantees exists.
- On a failed `unlock_file` (wrong passphrase or corrupted capsule), **no output file is left
  behind** — the AEAD tag is verified before any bytes are committed to the plaintext output
  (oneshot legacy format: before `write_atomic` is even called; streaming format: inside
  `write_atomic`'s closure, which only calls `.commit()` on success). Safe to retry the
  password without manual cleanup.
- `lock_file` reads the *entire* plaintext input as its `original_size` up front (oneshot
  format) or streams it in 1 MiB chunks (streaming format, auto-selected — see
  `tests/encryption_capsule.rs`); either way it needs the plaintext `.mv2` fully committed and
  closed (no open file handle / WAL still pending) before locking — which is exactly the order
  `memvidd` uses (`mem.commit()` then `lock_file`).

## memvid-core API used

Lexical: `Memvid::create` / `Memvid::open` / `commit` / `stats` / `search(SearchRequest)` — the
same surface as `examples/basic_usage.rs`. Semantic: `preview_chunks` /
`put_with_embedding_and_options` / `put_with_chunk_embeddings` / `search_vec` /
`frame_by_id` / `frame_text_by_id`, plus `set_vec_model` / `effective_vec_index_dimension`
for the model binding. The embedding model itself runs in memvidd (`embed.rs`), not through
memvid-core's `text_embed`.

## Semantic search

`memvid-core`'s lexical engine is fast but exact-term-only (see **Search strategy**). memvidd
adds semantic (meaning-based) retrieval with a local multilingual embedding model:
`intfloat/multilingual-e5-base` (768 dims, about 100 languages including Czech), as the int8
ONNX conversion published as `Xenova/multilingual-e5-base`.

It runs in memvidd's own `embed.rs` (`E5Embedder`) on ONNX Runtime and `tokenizers` directly.
memvid-core's `LocalTextEmbedder` can't host it: it only knows a fixed list of English BERT
models (an unknown name silently falls back to bge-small) and pools the CLS token, while e5
needs mean pooling over the attention mask and a prefix on every input. Until 2026-10-30
memvidd used `LocalTextEmbedder` with `bge-small-en-v1.5` (384 dims, English only).

**The model is a runtime concern**, resolved once at startup by `init_embedder()`: load the
model and run one real embedding as a readiness probe. If that works, `ping` reports
`"embeds":"internal"` and `"embed_model":"multilingual-e5-base"`. If it fails for any reason
(model file missing, broken ONNX session, ...), memvidd logs why and **falls back to
lexical-only** (`"embeds":"none"`).

The model is never downloaded automatically. Fetch it once (296 MB; the revision is pinned and
the hashes are the ones arxcore-v2's Dockerfile checks):

```bash
D=~/Library/Caches/memvid/text-models   # macOS; ~/.cache/memvid/text-models on Linux
REV=1ec9243030a27d1a115d5c340572074c125b58b2
mkdir -p "$D"
curl -fL "https://huggingface.co/Xenova/multilingual-e5-base/resolve/$REV/onnx/model_quantized.onnx" \
  -o "$D/multilingual-e5-base.onnx"
curl -fL "https://huggingface.co/Xenova/multilingual-e5-base/resolve/$REV/tokenizer.json" \
  -o "$D/multilingual-e5-base_tokenizer.json"
shasum -a 256 "$D"/multilingual-e5-base*
# df7a9a29309e3ad491e1783adf8baee710262cc06079c7cbab63c630277fac94  multilingual-e5-base.onnx
# 62c24cdc13d4c9952d63718d6c9fa4c287974249e16b7ade6d5a85e7bbb75626  multilingual-e5-base_tokenizer.json
```

Inputs are truncated to 512 tokens. Passages are embedded as `"passage: <text>"` and queries as
`"query: <text>"`, as the e5 model card requires; the resulting vectors are L2-normalized.

### Ingestion

`put_with_best_effort_embedding()`: if memvid-core is going to chunk the document
(`Memvid::preview_chunks`, ~2400-char threshold), embed the parent text plus every chunk
(batches of 16) and store them with `put_with_chunk_embeddings`, so each chunk is
independently searchable by vector. Otherwise embed the single text and use
`put_with_embedding_and_options`. Any embedding failure degrades that one document to a plain
lexical put rather than failing the request.

memvidd turns off memvid-core's time-budgeted extraction for every put (`untimed()`). With the
budget on, memvid-core stored text-like files, judged by the extension in the uri (`.txt`,
`.md`, `.json`, `.rtf`, `.odt`, ...), as one "skim" frame and left chunking to an enrichment
worker memvidd never runs. Those documents had a single embedding of their first 512 tokens:
a 32,000-character story uploaded as `.txt` became 1 frame instead of 23.

### Query

The query is embedded with the `query: ` prefix. `search_vec` only returns
`{frame_id, distance}`, so each hit's text/title/uri is fetched with `frame_by_id` +
`frame_text_by_id`.

### Model binding and re-embedding (2026-10-30)

Vectors from two different models must never meet in one index: their similarities are
meaningless across models. On `open`, `bind_vector_model()` decides per store:

- no vectors yet: bind the store's vector index to `multilingual-e5-base` (memvid-core's
  `set_vec_model`, persisted in the vector index manifest);
- 768-d vectors: `set_vec_model` too, and a `ModelMismatch` error marks the store stale;
- any other dimension: stale. Every store written before 2026-10-30 holds 384-d bge-small
  vectors and no binding at all.

A stale store answers `open`/`stats` with `needs_reembed: true`, is searched lexical-only
(including the lexical degrade path), and refuses `add_document` with `NEEDS_REEMBED`, so it
never ends up half re-indexed.

Stale stores are told apart by dimension, not by binding: memvid-core has no public getter for
the bound model, and `set_vec_model` binds an unbound index (which `Memvid`'s `Drop` then
commits, even for `open_read_only`, which opens the file writable). Calling it on a legacy
store would persist a wrong binding.

`memvidd --reembed <file.mv2>...` rebuilds stale stores and exits:

```bash
target/release/memvidd --reembed data/memories/*.mv2
# data/memories/budget.mv2: 1 documents (56 frames) re-embedded with multilingual-e5-base in 5.5s
```

Per file it reads the active documents (text, uri, title, tags, metadata, timestamp), writes
them with fresh embeddings into `<file>.reembed-tmp`, verifies the document count and the
dimension, and renames the result over the original. The original stays locked for the whole
rebuild (a running ragtime can't write to it meanwhile; memvid-core gives up after ~10 s
with "file is in use by another process") and is left untouched on any failure. Files already
bound to the current model are skipped. Superseded and deleted frames are not carried over.
Encrypted `.mv2e` stores are refused.

The rebuild commits after every document. One commit over a 3-document store with its copied
metadata failed reproducibly with `Embedded WAL is corrupted ... wal record checksum mismatch`
(memvid-core has open upstream WAL fixes, PR #231 and #249). Normal ingestion, many documents
then one commit, was checked against the same real files and was not affected.

### Hybrid ranking

`op_search` runs lexical (`lexical_hits`, single pass, no degrade) and semantic (`vector_hits`,
if the embedder is ready) in parallel passes over an over-fetched candidate pool
(`pool_k = (k*6).max(20)`, not the final `k` — see **Source diversity** below for why), then
merges with **Reciprocal Rank Fusion** (`rrf_merge`: score = `1/(60+rank)` summed per frame
across both lists — the standard RRF constant, chosen because lexical BM25-ish scores and
vector L2 distances are not on comparable scales, but rank position is). If both lists come
back empty *and* there is no embedder, the old three-tier lexical degrade (full query ->
keywords -> per-term union) runs as a last resort — kept as the safety net for lexical-only
builds/deployments.

Verified (`bitcoin.pdf`, hybrid vs. lexical-only-fallback on a zero-shared-vocabulary query
*"What makes it computationally expensive to rewrite transaction history?"* — no "hash",
"proof-of-work" or "chain" in the question): hybrid's #3 hit was the CPU-power/honest-nodes
passage (page 9); lexical-only's #2/#3 were a probability table and a proof-of-work aside —
plausible, but less directly on-topic. Both returned *something* relevant; hybrid ranked the
most relevant chunk higher.

### Cross-memory comparable similarity (2026-10-30)

`score` is `1 / rank` of the fused, diversified list, so it says nothing about how relevant a hit is
compared with a hit from another memory. Every hit therefore also carries `metadata.similarity`: the
cosine between the query and the hit's text. All memories are bound to the same embedding model and
its vectors are L2-normalised, so the cosine is comparable across memories.

- Vector hits: `search_vec` returns an L2 distance `d`; for unit vectors `cos = 1 - d^2 / 2`
  (`cosine_from_distance`).
- Lexical-only hits (found by keyword, no vector neighbour): their text is embedded once with the
  `passage: ` prefix (batched) and compared with the query embedding (`fill_missing_similarity`).
- A hit found by both sides keeps the similarity the vector side measured.

Measured on the state-budget PDF: a relevant question gives 0.84-0.85 for its top hits, an irrelevant one
(a recipe) at best 0.79, so the band is narrow (e5 similarities are compressed) but the order is
meaningful. An exact-term query (an ISBN) found by the lexical side has a *lower* similarity (0.74)
than semantic neighbours (0.75), which is why a merge across memories must not sort by cosine alone.
ragtime fuses the global cosine order with each memory's own rank order (`src/merge.ts`).

Raw RRF-by-score alone lets one source dominate every slot: found on real user data (a memory
mixing one 194-page terms-and-conditions PDF with three one-page e-tickets — every top-5 hit
for a travel question came from the terms doc, zero from any ticket) and even *within* one
class of source (three near-identical travellers' tickets, where two people's low-value
chunks outranked the third person's only relevant chunk, so their trip was never mentioned).

Fix in `rrf_merge`: after RRF-scoring, group candidates by document (`uri` with any `#page-N`
suffix stripped — all chunks of one `add_document` call share the un-suffixed prefix), then
round-robin across groups in best-hit order — every source with *any* relevant hit gets a slot
before any source gets a second. This only has candidates to promote because of the `pool_k`
over-fetch above; grouping+round-robining a pool already pre-truncated to `k` (the original
bug - `lexical_hits`/`vector_hits` were called with `k`, not a larger pool, so nothing beyond
the top `k` ever reached `rrf_merge` to diversify with) is a no-op.

Verified on the real mixed memory: top-5 for the same query went from 5/5 terms-doc chunks to
4/5 real ticket content. On the three-tickets memory, `k:5` still only fit each traveller's
single best chunk plus one repeat before running out of slots; `arxcore-v2`'s `/chat` bumped
its request from `k:5` to `k:8` to give the round-robin enough room for every traveller to get
a second chunk too (`docs/SIDECAR_PROTOCOL.md` unchanged — `k` was already a client-supplied
param).

**Known remaining gap**: a document's own identity/summary content (here, the parent frame
holding the traveller's name and full itinerary) can still rank below that same document's
less-central chunks (page 2's connection details, page 5's insurance boilerplate) for a broad
query, landing outside even an enlarged `k`. Diversity fixes *which documents* get a slot, not
*which chunk of a document* wins that slot. Not addressed here - would need either a bigger
`k` (diminishing returns, noisier context) or a ranking boost for a document's first/parent
chunk specifically.

**What the round-robin actually guarantees** (debugging session, 2026-09-15, see
`arxcore-v2/tests/manual/rag-quality.sh`): one slot per *document group* within the first N
ranks, where N = number of distinct groups in the pool. It does **not** guarantee a small
document outranks a big one inside that first round — group order within round 1 still
follows each group's own best relevance score. On a synthetic fixture (3 short profiles + 1
large handbook, 4 groups total) one profile legitimately scored below the handbook's best
chunk for a broad query and landed 4th, not inside the top 3 — correct behavior, since all 4
groups (all 3 profiles + the handbook) got exactly one slot each in the first 4 ranks as
designed. A test that asserted "all 3 profiles must be in the top 3" was checking a stronger
guarantee than the fix provides; corrected to top-4 (one slot per group). Anyone writing a
new retrieval-quality check against this fixer should assert **"present within the first N
slots"** (N = number of distinct sources in the pool), never a tighter window than that.

## Search strategy

`memvid-core` lexical search is conjunctive — every query term must be present, and the
term-sketch pre-filter enforces the same — so a natural-language question matches nothing.
`search` degrades in three steps and returns the first non-empty result:

1. the query as given (`no_sketch: true`)
2. content words only — stop words and tokens shorter than 3 chars dropped
3. per-term union — each keyword searched alone, hits merged by frame keeping the best score,
   re-ranked, truncated to `k`

This degrade path now only runs as the fallback when there is no working embedder (see
**Hybrid ranking**); with semantic search available, `rrf_merge` compensates for conjunctive
lexical misses directly.

The query reaches memvid-core as written, so its query language stays available: `"phrases"`,
`AND`/`OR`/`NOT` (lower case too) with parentheses, and `tag:`, `label:`, `uri:`, `scope:`,
`track:` and `date:[start TO end]` filters. Two failure modes are handled in `run_query`
(2026-10-30; before that both failed the whole `search`):

- a query that isn't valid in that language (an unbalanced quote or parenthesis, a lone
  `* * *`) returns `InvalidQuery`; it is retried as its plain words;
- when Tantivy finds nothing, or gets a wildcard, memvid-core falls back to a legacy lexical
  index these stores don't have and returns "Lexical index is not enabled"; that is treated
  as "no lexical match".

## Native document extraction

`add_document`'s `content_base64` param routes raw file bytes through `memvid-core`'s built-in
reader registry (`ReaderRegistry::default()`: `PdfReader`, `DocxReader`, `XlsxReader`,
`XlsReader`, `PptxReader`, plus a passthrough) instead of expecting the host to have already
extracted plain text. All of these readers are unconditionally compiled (`calamine`,
`pdf-extract` etc. are regular, non-optional dependencies) — no extra cargo feature needed,
unlike `vec`/`encryption`.

`resolve_content` (in `op_add_document`) picks the extractor from `metadata.source`'s file
extension via `formats::format_from_extension` - **not** from magic-byte sniffing, since the
host already knows the true filename. The extracted text becomes `content` for the rest of
the pipeline (chunking, embedding, `put_*`) exactly as if the host had sent plain text.

| Extensions | Extracted by |
|---|---|
| `.pdf` | memvid-core `PdfReader` |
| `.docx` `.docm` `.dotx` `.dotm` | memvid-core `DocxReader` (macro and template variants share the format) |
| `.xlsx` `.xlsm` `.xltx` `.xltm` | memvid-core `XlsxReader` |
| `.xls` | memvid-core `XlsReader` |
| `.pptx` `.pptm` `.ppsx` `.ppsm` `.potx` `.potm` | memvid-core `PptxReader` |
| `.ods` `.xlsb` | calamine, rendered like `XlsReader` (`Sheet: <name>`, tab-separated rows) |
| `.odt` `.odp` | `content.xml` of the archive: one line per paragraph/heading, `Slide N:` per presentation page |
| `.html` `.htm` `.xhtml` | tag stripper: script/style skipped, blocks on their own lines, table cells tab-separated, HTML5 entities decoded; charset from BOM, `<meta charset>` or XML declaration, else UTF-8, else windows-1250 |
| `.epub` | `container.xml` -> OPF -> spine order, each chapter through the HTML extractor |
| `.rtf` | control-word parser: skipped destinations (font/color tables, info, pictures, field instructions), `\'hh` in the `\ansicpgN` code page, `\uN` |

Everything except the memvid-core readers is in `formats.rs`, built only on crates memvid-core
already depends on (calamine, zip, quick-xml with its `escape-html` feature, encoding_rs), with
a 64 MiB cap on any single archive entry. Verified on synthetic fixtures (unit tests) and on a
real 32,000-character Czech story converted to RTF, ODT and HTML by macOS `textutil`: the same
23-24 frames as the DOCX original and no markup left in search hits.

A reader that finds no text at all (scanned/image-only PDF, a genuinely empty file, or bytes
that don't decode to anything) fails the whole `add_document` call with `BAD_PARAMS`:
`no extractable text in "<source>" (scanned/image-only, or genuinely empty)` — the host's
create-memory-equivalent flow is expected to catch this per-file and use the message as a skip
reason, not to retry or crash.

Adopted 2026-09-14 same day as the upstream rebase (see **Rebase notes**); replaced
arxcore-v2's own Node-side `mammoth` (.docx) / `pdf-parse` (.pdf) / `exceljs` (.xlsx)
extraction, which is now deleted from that repo entirely. Verified: real `.docx`, `.xlsx`, and
`bitcoin.pdf` fixtures round-tripped through `content_base64` → search → live RAG chat with
correct, cited answers; a truly unparseable binary payload with a `.pdf` extension failed
cleanly with `BAD_PARAMS`, no crash.

Not adopted: `XlsxReader`'s more advanced structured API (`chunk_workbook` /
`XlsxChunkingOptions`, header-value pairing, table detection) — `DocumentReader::extract()`
(what `add_document` calls) uses the simpler flat-text path (`generate_flat_text`), which is
already a strict improvement over the old Node-side `exceljs` dump but not the fanciest option
available. Revisit if XLSX chunk quality ever becomes a real problem.

## Chunking

`memvid-core` chunks automatically — `add_document` does **not** need to split text itself.
Any document >= ~2400 normalized characters is split by `plan_document_chunks`
(`src/memvid/chunks.rs`) into ~1200-char pieces (structure-aware: table rows and code blocks
are kept whole), each becoming its own searchable frame with a `#page-N` URI suffix, alongside
one unchunked "parent" frame at the plain `mv2://doc/<source>` URI. Smaller documents stay a
single frame. This is why `stats().documents` (`frame_count`) can jump by more than 1 per
`add_document` call — e.g. a 22.8k-char PDF became 19 frames — and why `commit` and `stats`
report frame counts only *after* `commit` (a pending `add_document` does not bump the count).
The same chunk boundaries are reused for embeddings (`preview_chunks`), so lexical and semantic
search hit the same ~1200-char units.

## Known limitations (PoC)

- The embedding model is the int8 quantization of multilingual-e5-base, slightly below the
  fp32 original in quality; swapping the download URL for `onnx/model.onnx` (1.1 GB) is the
  only change needed to use fp32.
- Wildcards (`galaxie*`) never match lexically: memvid-core only implements them in its legacy
  lexical index, which these stores don't have. The vector side still answers.
- `memvid-core` can return the exact same frame twice for one lexical query (observed on a
  chunked PDF); `run_query` over-fetches (`top_k * 2`) and dedupes by `frame_id`.
- Ingestion cost grows with `vec` on: the 55-page state-budget PDF takes ~5.8s with
  multilingual-e5-base on Apple Silicon CPU (4.9s with the old bge-small). Fine for a PoC;
  would want a queue for bulk imports.
- One request in flight at a time (the host serialises); no streaming.
- `.mv2e` encryption is optional and whole-capsule (see **Encryption**) — not per-frame, and
  the plaintext scratch file it operates on is only `remove_file`-deleted on `close`, not
  securely wiped. No auth.
