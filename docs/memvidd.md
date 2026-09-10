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
already picked up for free by the rebase itself.

## Build

```bash
cargo build --features vec --bin memvidd            # debug -> target/debug/memvidd
cargo build --release --features vec --bin memvidd  # -> target/release/memvidd
```

The `[[bin]]` requires the `vec` feature to compile at all (it links
`memvid_core::text_embed`, which is itself feature-gated) — see **Semantic search** below for
why that doesn't mean an embedding model is mandatory at runtime.

## Methods

| method | params | result |
|---|---|---|
| `ping` | `{}` | `{pong, version, embeds:"internal"\|"none"}` |
| `open` | `{path, create, passphrase?}` | `{path, documents, created, encrypted?}` — closes any previously-open store first (so re-opening the same path is safe), then sets the one active store; `create:true` makes+commits an empty `.mv2` when the file is absent. `passphrase` treats `path` as an encrypted `.mv2e` capsule — see **Encryption** below; `encrypted` is only present in `encryption`-feature builds |
| `add_document` | `{content?, content_base64?, metadata:{source,...}}` | `{chunks:1, seq}` — exactly one of `content` (plain UTF-8) / `content_base64` (raw file bytes, natively extracted — see **Native document extraction**) is required. `chunks` is always 1 here (one ingest call per document); `memvid-core` may still split the document into several search-indexed frames internally — see **Chunking** below. Every metadata key is stored as a frame tag |
| `commit` | `{}` | `{documents}` — in encrypted mode, also re-locks the durable `.mv2e` (see **Encryption**) |
| `search` | `{query, k?}` | `{hits:[{score, content, metadata:{source,uri,frame_id,rank,matches}}], total_hits, elapsed_ms}` |
| `stats` | `{}` | `{path, documents}` — about the *active* store |
| `inspect` | `{path}` | `{path, documents: int\|null}` — reads any `.mv2`'s document count via `Memvid::open_read_only` (a shared, not exclusive, lock) without disturbing the active store; `null` (not an error) for anything unreadable |
| `close` | `{}` | `{}` — drops the active store, process stays up; in encrypted mode, best-effort deletes the plaintext scratch file (see **Encryption**) |

Errors: `{"id":N,"ok":false,"error":{"message","code"}}` with `code` in
`BAD_PARAMS | MV2_NOT_OPEN | IO_ERROR | INTERNAL`.

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

## memvid-core API used

Lexical: `Memvid::create` / `Memvid::open` / `commit` / `stats` / `search(SearchRequest)` — the
same surface as `examples/basic_usage.rs`. Semantic: `preview_chunks` /
`put_with_embedding_and_options` / `put_with_chunk_embeddings` / `search_vec` /
`frame_by_id` / `frame_text_by_id`, plus `text_embed::{LocalTextEmbedder, TextEmbedConfig}` and
the `EmbeddingProvider` trait (`embed_text`, `embed_batch`) — the same surface as
`examples/text_embedding.rs`.

## Semantic search

`memvid-core`'s lexical engine is fast but exact-term-only (see **Search strategy**). Building
with `--features vec` links ONNX Runtime (`ort`) and a local text-embedding provider
(`LocalTextEmbedder`), giving true semantic (meaning-based) retrieval on top.

**The feature must be compiled in, but the model is a runtime concern**, resolved once at
startup by `init_embedder()`:

1. Construct `LocalTextEmbedder` with `TextEmbedConfig::default()` (model `bge-small-en-v1.5`,
   384 dims, cache dir `dirs_next::cache_dir()/memvid/text-models` — `~/Library/Caches/...` on
   macOS).
2. Run one real `embed_text()` call as a readiness probe.
3. If that succeeds: `ping` reports `"embeds":"internal"` and every `add_document`/`search`
   uses embeddings. If it fails for any reason (model file missing, corrupt ONNX session,
   etc.): log the reason to stderr and **fall back to lexical-only**, exactly like a non-`vec`
   build — `ping` reports `"embeds":"none"`.

`LocalTextEmbedder` does **not** auto-download the model — `ensure_model_file()` just errors
with instructions if it's missing. Fetch it once:

```bash
mkdir -p ~/Library/Caches/memvid/text-models   # macOS; ~/.cache/memvid/text-models on Linux
curl -L 'https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/onnx/model.onnx' \
  -o ~/Library/Caches/memvid/text-models/bge-small-en-v1.5.onnx
curl -L 'https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/tokenizer.json' \
  -o ~/Library/Caches/memvid/text-models/bge-small-en-v1.5_tokenizer.json
```

(~134 MB total.) Without these files the binary still runs fine, just lexical-only.

### Ingestion

`put_with_best_effort_embedding()`: if the document is large enough to chunk
(`Memvid::preview_chunks`, same ~2400-char threshold as lexical chunking), embed the parent
text plus every chunk (`embed_batch`) and store them with `put_with_chunk_embeddings` — the
API `memvid-core` recommends for chunked documents, so each chunk is independently searchable
by vector, not just the whole document. Otherwise embed the single text and use
`put_with_embedding_and_options`. Any embedding failure degrades that one document to a plain
lexical `put_bytes_with_options` rather than failing the request.

### Query

BGE models want an instruction prefix on the **query** side only (not on stored passages):
`"Represent this sentence for searching relevant passages: "` (see `BGE_QUERY_PREFIX`).
`search_vec` only returns `{frame_id, distance}`, so each hit's text/title/uri is fetched with
`frame_by_id` + `frame_text_by_id`.

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

### Source diversity (2026-09-15)

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

## Native document extraction

`add_document`'s `content_base64` param routes raw file bytes through `memvid-core`'s built-in
reader registry (`ReaderRegistry::default()`: `PdfReader`, `DocxReader`, `XlsxReader`,
`XlsReader`, `PptxReader`, plus a passthrough) instead of expecting the host to have already
extracted plain text. All of these readers are unconditionally compiled (`calamine`,
`pdf-extract` etc. are regular, non-optional dependencies) — no extra cargo feature needed,
unlike `vec`/`encryption`.

`resolve_content` (in `op_add_document`) picks the reader from `metadata.source`'s file
extension via `document_format_from_extension` (`.pdf`/`.docx`/`.xlsx`/`.xls`/`.pptx` →
`DocumentFormat::{Pdf,Docx,Xlsx,Xls,Pptx}`) — **not** from magic-byte sniffing, since the host
already knows the true filename. `extract_native` then builds a `ReaderHint` with that format,
finds the matching reader, and calls `.extract()`. The extracted `ReaderOutput.document.text`
becomes `content` for the rest of the pipeline (chunking, embedding, `put_*`) exactly as if the
host had sent plain text directly — chunking/embedding code has no idea the text came from a
reader instead of `params.content`.

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

- Default embedding model (`bge-small-en-v1.5`) is English-tuned; multilingual corpora will
  see weaker semantic recall than lexical.
- `memvid-core` can return the exact same frame twice for one lexical query (observed on a
  chunked PDF); `run_query` over-fetches (`top_k * 2`) and dedupes by `frame_id`.
- Ingestion cost grows with `vec` on: embedding ~19 chunks of a 22.8k-char PDF took ~2s on
  Apple Silicon CPU (batched). Fine for a PoC; would want a queue for bulk imports.
- One request in flight at a time (the host serialises); no streaming.
- No `.mv2e` encryption, no auth.
