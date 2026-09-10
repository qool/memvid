//! memvidd — NDJSON stdio sidecar around `memvid-core` for the arxcore-v2 PoC.
//!
//! Protocol: one JSON object per line on stdin (request), one JSON object per line
//! on stdout (response). All logs go to stderr. See
//! `arxcore-v2/docs/SIDECAR_PROTOCOL.md` for the full contract.
//!
//! Build: `cargo build --features vec --bin memvidd` for hybrid lexical + semantic
//! search (needs a downloaded embedding model, see docs/memvidd.md - Semantic
//! search). Without `--features vec`, or if the model file is missing, this falls
//! back to lexical-only search and `ping` reports `"embeds": "none"`.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use memvid_core::text_embed::{LocalTextEmbedder, TextEmbedConfig};
use memvid_core::types::embedding::EmbeddingProvider;
use memvid_core::{DocumentFormat, Memvid, PutOptions, ReaderHint, ReaderRegistry, SearchHit, SearchRequest};
use serde_json::{Value, json};

/// BGE models want an instruction prefix on the *query* side only (not on
/// passages/documents) - see the model card for bge-small-en-v1.5.
const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// Active store, the path it was opened from, and the (optional) embedder.
struct State {
    mem: Option<Memvid>,
    path: Option<PathBuf>,
    embedder: Option<LocalTextEmbedder>,
}

impl State {
    fn new(embedder: Option<LocalTextEmbedder>) -> Self {
        State { mem: None, path: None, embedder }
    }
}

/// One search result normalised across the lexical and vector code paths, so
/// both can be merged before the final JSON response is built.
struct RankedHit {
    frame_id: u64,
    uri: String,
    title: Option<String>,
    content: String,
    matches: usize,
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let embedder = init_embedder();
    let embeds_label = if embedder.is_some() { "internal" } else { "none" };
    let mut state = State::new(embedder);

    eprintln!(
        "[memvidd] ready (v{}, embeds={embeds_label})",
        env!("CARGO_PKG_VERSION")
    );

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[memvidd] stdin read error: {e}");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = handle_line(trimmed, &mut state);
        if writeln!(stdout, "{response}").is_err() {
            break;
        }
        let _ = stdout.flush();
    }

    eprintln!("[memvidd] stdin closed, exiting");
}

/// Build the local ONNX embedder and probe it with one real embed call, so a
/// missing model file (or a broken ONNX Runtime) degrades to lexical-only
/// instead of failing every later request. Cheap when the `vec` feature is
/// off: `LocalTextEmbedder` still builds, but embedding is never attempted.
fn init_embedder() -> Option<LocalTextEmbedder> {
    if !cfg!(feature = "vec") {
        return None;
    }
    let config = TextEmbedConfig::default();
    let embedder = match LocalTextEmbedder::new(config) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[memvidd] embedder init failed, falling back to lexical-only: {err}");
            return None;
        }
    };
    match embedder.embed_text("ready check") {
        Ok(_) => {
            eprintln!(
                "[memvidd] semantic search ready ({}, {}d)",
                embedder.model_info().name,
                embedder.dimension()
            );
            Some(embedder)
        }
        Err(err) => {
            eprintln!(
                "[memvidd] embedding model not usable, falling back to lexical-only: {err}"
            );
            None
        }
    }
}

/// Parse one request line and produce one response line.
fn handle_line(line: &str, state: &mut State) -> String {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return err_line(&Value::Null, &format!("invalid JSON: {e}"), "BAD_PARAMS"),
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    let result: Result<Value, (String, &'static str)> = match method {
        "ping" => Ok(json!({
            "pong": true,
            "version": env!("CARGO_PKG_VERSION"),
            "embeds": if state.embedder.is_some() { "internal" } else { "none" }
        })),
        "open" => op_open(state, &params),
        "add_document" => op_add_document(state, &params),
        "commit" => op_commit(state),
        "search" => op_search(state, &params),
        "stats" => op_stats(state),
        "inspect" => op_inspect(&params),
        "close" => {
            state.mem = None;
            state.path = None;
            Ok(json!({}))
        }
        "" => Err(("missing `method`".to_string(), "BAD_PARAMS")),
        other => Err((format!("unknown method `{other}`"), "BAD_PARAMS")),
    };

    match result {
        Ok(value) => json!({ "id": id, "ok": true, "result": value }).to_string(),
        Err((message, code)) => {
            eprintln!("[memvidd] {method} failed: {message}");
            json!({ "id": id, "ok": false, "error": { "message": message, "code": code } }).to_string()
        }
    }
}

fn op_open(state: &mut State, params: &Value) -> Result<Value, (String, &'static str)> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ("`params.path` is required".to_string(), "BAD_PARAMS"))?;
    let want_create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let path_buf = PathBuf::from(path);

    // Release any previously-open store first, otherwise re-opening the same path
    // (or any path) races our own still-held exclusive file lock.
    state.mem = None;
    state.path = None;

    let exists = path_buf.exists();

    let (mem, created) = if want_create && !exists {
        if let Some(parent) = path_buf.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| (format!("cannot create parent dir: {e}"), "IO_ERROR"))?;
        }
        let mut m =
            Memvid::create(&path_buf).map_err(|e| (format!("create failed: {e}"), "IO_ERROR"))?;
        m.commit().map_err(|e| (format!("initial commit failed: {e}"), "IO_ERROR"))?;
        (m, true)
    } else {
        let m =
            Memvid::open(&path_buf).map_err(|e| (format!("open failed: {e}"), "IO_ERROR"))?;
        (m, false)
    };

    let documents = mem.stats().map(|s| s.frame_count).unwrap_or(0);
    state.mem = Some(mem);
    state.path = Some(path_buf);

    Ok(json!({ "path": path, "documents": documents, "created": created }))
}

fn op_add_document(state: &mut State, params: &Value) -> Result<Value, (String, &'static str)> {
    let metadata = params.get("metadata").cloned().unwrap_or_else(|| json!({}));
    let source = metadata
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("document")
        .to_string();

    // Two ways in: plain UTF-8 text (`content`, unchanged since before native
    // extraction existed), or raw file bytes (`content_base64`) for a binary
    // format memvid-core's reader registry can extract natively (PDF, DOCX,
    // XLSX, XLS, PPTX) - see `resolve_content` and docs/memvidd.md "Native
    // document extraction".
    let content = resolve_content(params, &source)?;

    let mut builder = PutOptions::builder()
        .title(source.clone())
        .uri(format!("mv2://doc/{source}"));
    if let Some(obj) = metadata.as_object() {
        for (key, value) in obj {
            let v = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            builder = builder.tag(key.clone(), v);
        }
    }

    let embedder = state.embedder.as_ref();
    let mem = state
        .mem
        .as_mut()
        .ok_or_else(|| ("no memory file is open; call `open` first".to_string(), "MV2_NOT_OPEN"))?;

    let seq = put_with_best_effort_embedding(mem, embedder, &content, builder.build())?;

    // One `add_document` call per document; memvid-core may still split it into
    // several searchable frames internally (see docs/memvidd.md - Chunking).
    Ok(json!({ "chunks": 1, "seq": seq }))
}

/// Resolve `add_document`'s text content, from either `params.content` (plain
/// UTF-8, the original protocol - unchanged) or `params.content_base64` (raw
/// file bytes, decoded and run through the matching native reader for
/// `source`'s extension).
fn resolve_content(params: &Value, source: &str) -> Result<String, (String, &'static str)> {
    if let Some(text) = params.get("content").and_then(Value::as_str) {
        return Ok(text.to_string());
    }
    let encoded = params
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ("either `params.content` or `params.content_base64` is required".to_string(), "BAD_PARAMS")
        })?;
    let bytes = BASE64
        .decode(encoded)
        .map_err(|e| (format!("`content_base64` is not valid base64: {e}"), "BAD_PARAMS"))?;
    let format = document_format_from_extension(source).ok_or_else(|| {
        (
            format!("no native reader for the extension on \"{source}\" - send `content` (plain text) instead"),
            "BAD_PARAMS",
        )
    })?;
    extract_native(&bytes, format, source)
}

/// Map a filename's extension to the native reader format it needs.
fn document_format_from_extension(name: &str) -> Option<DocumentFormat> {
    let ext = name.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Some(DocumentFormat::Pdf),
        "docx" => Some(DocumentFormat::Docx),
        "xlsx" => Some(DocumentFormat::Xlsx),
        "xls" => Some(DocumentFormat::Xls),
        "pptx" => Some(DocumentFormat::Pptx),
        _ => None,
    }
}

/// The built-in reader set (PDF, DOCX, XLSX, XLS, PPTX, plus a plain-text
/// fallback) - unconditionally compiled, no extra feature needed (`calamine`,
/// `pdf-extract` etc. are all regular, non-optional dependencies already).
fn native_readers() -> &'static ReaderRegistry {
    static REGISTRY: std::sync::OnceLock<ReaderRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(ReaderRegistry::default)
}

/// Extract plain text from raw file bytes via memvid-core's native reader for
/// `format`. Returns a `BAD_PARAMS`-coded error (not a crash) when the reader
/// finds no text at all - e.g. a scanned/image-only PDF, matching the
/// create-memory-side skip-reason convention on the Node side.
fn extract_native(bytes: &[u8], format: DocumentFormat, source: &str) -> Result<String, (String, &'static str)> {
    let hint = ReaderHint::new(None, Some(format));
    let reader = native_readers()
        .find_reader(&hint)
        .ok_or_else(|| (format!("no native reader registered for {source}"), "INTERNAL"))?;
    let output = reader
        .extract(bytes, &hint)
        .map_err(|e| (format!("native extraction of \"{source}\" failed: {e}"), "INTERNAL"))?;
    match output.document.text {
        Some(text) if !text.trim().is_empty() => Ok(text),
        _ => Err((
            format!("no extractable text in \"{source}\" (scanned/image-only, or genuinely empty)"),
            "BAD_PARAMS",
        )),
    }
}

/// Ingest one document, embedding it (and its auto-generated chunks) when an
/// embedder is available. Falls back to a plain lexical put if embedding
/// fails for any reason - ingestion should not hard-fail just because the
/// embedding model hiccuped.
fn put_with_best_effort_embedding(
    mem: &mut Memvid,
    embedder: Option<&LocalTextEmbedder>,
    content: &str,
    options: PutOptions,
) -> Result<u64, (String, &'static str)> {
    let Some(embedder) = embedder else {
        return mem
            .put_bytes_with_options(content.as_bytes(), options)
            .map_err(|e| (format!("put failed: {e}"), "INTERNAL"));
    };

    let chunk_plan = mem.preview_chunks(content.as_bytes());
    let embedded = match chunk_plan {
        Some(chunk_texts) => {
            let refs: Vec<&str> = chunk_texts.iter().map(String::as_str).collect();
            match (embedder.embed_text(content), embedder.embed_batch(&refs)) {
                (Ok(parent), Ok(chunks)) => Some((Some(parent), chunks)),
                (parent_result, chunks_result) => {
                    let reason = parent_result
                        .err()
                        .or_else(|| chunks_result.err())
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "unknown error".to_string());
                    eprintln!(
                        "[memvidd] chunk embedding failed ({reason}), falling back to lexical-only for this document"
                    );
                    None
                }
            }
        }
        None => match embedder.embed_text(content) {
            Ok(vector) => Some((Some(vector), Vec::new())),
            Err(err) => {
                eprintln!("[memvidd] document embedding failed, falling back to lexical-only: {err}");
                None
            }
        },
    };

    match embedded {
        Some((parent, chunks)) if !chunks.is_empty() => mem
            .put_with_chunk_embeddings(content.as_bytes(), parent, chunks, options)
            .map_err(|e| (format!("put failed: {e}"), "INTERNAL")),
        Some((Some(parent), _)) => mem
            .put_with_embedding_and_options(content.as_bytes(), parent, options)
            .map_err(|e| (format!("put failed: {e}"), "INTERNAL")),
        _ => mem
            .put_bytes_with_options(content.as_bytes(), options)
            .map_err(|e| (format!("put failed: {e}"), "INTERNAL")),
    }
}

fn op_commit(state: &mut State) -> Result<Value, (String, &'static str)> {
    let mem = state
        .mem
        .as_mut()
        .ok_or_else(|| ("no memory file is open; call `open` first".to_string(), "MV2_NOT_OPEN"))?;
    mem.commit().map_err(|e| (format!("commit failed: {e}"), "IO_ERROR"))?;
    let documents = mem.stats().map(|s| s.frame_count).unwrap_or(0);
    Ok(json!({ "documents": documents }))
}

fn op_search(state: &mut State, params: &Value) -> Result<Value, (String, &'static str)> {
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ("`params.query` is required".to_string(), "BAD_PARAMS"))?
        .to_string();
    let k = params.get("k").and_then(Value::as_u64).unwrap_or(5).max(1) as usize;

    let embedder = state.embedder.as_ref();
    let mem = state
        .mem
        .as_mut()
        .ok_or_else(|| ("no memory file is open; call `open` first".to_string(), "MV2_NOT_OPEN"))?;

    let started = std::time::Instant::now();

    // Over-fetch a candidate pool larger than the final k: rrf_merge's
    // per-source diversification needs other sources' hits still in the pool
    // to promote them over a dominant source's extra chunks. Fetching exactly
    // k from each side left nothing else to draw from, which made the
    // diversification a no-op in practice - see docs/memvidd.md "Hybrid
    // ranking" for the two real cases (a huge terms doc drowning out one-page
    // e-tickets; three people's tickets where two low-value chunks from the
    // same two people crowded out the third person's only relevant chunk).
    let pool_k = (k * 6).max(20);
    let lexical = lexical_hits(mem, &query, pool_k)?;
    let vector = embedder
        .map(|embedder| vector_hits(mem, embedder, &query, pool_k))
        .transpose()?
        .unwrap_or_default();

    let mut merged = rrf_merge(lexical, vector, k);

    // Both ranked lists can legitimately come back empty only when the corpus
    // itself has nothing close to the query; the lexical degrade tiers already
    // cover conjunctive-search misses, so nothing further to retry here.
    if merged.is_empty() && embedder.is_none() {
        merged = degrade_lexical(mem, &query, k)?
            .into_iter()
            .map(ranked_hit_from_search_hit)
            .collect();
    }

    let total_hits = merged.len();
    let out: Vec<Value> = merged
        .into_iter()
        .enumerate()
        .map(|(i, hit)| {
            json!({
                "score": 1.0 / (i as f64 + 1.0),
                "content": hit.content,
                "metadata": {
                    "source": hit.title.unwrap_or_else(|| hit.uri.clone()),
                    "uri": hit.uri,
                    "frame_id": hit.frame_id,
                    "rank": i + 1,
                    "matches": hit.matches,
                },
            })
        })
        .collect();

    Ok(json!({ "hits": out, "total_hits": total_hits, "elapsed_ms": started.elapsed().as_millis() }))
}

fn ranked_hit_from_search_hit(hit: SearchHit) -> RankedHit {
    let raw = hit.chunk_text.clone().unwrap_or_else(|| hit.text.clone());
    RankedHit {
        frame_id: hit.frame_id,
        uri: hit.uri,
        title: hit.title,
        content: clean_snippet(&raw),
        matches: hit.matches,
    }
}

/// Lexical search only, no degradation - used as the "A" side of the hybrid
/// merge. Returns `[]` on a conjunctive miss rather than erroring; callers
/// decide whether to degrade further.
fn lexical_hits(mem: &mut Memvid, query: &str, k: usize) -> Result<Vec<RankedHit>, (String, &'static str)> {
    Ok(run_query(mem, query, k)?.into_iter().map(ranked_hit_from_search_hit).collect())
}

/// memvid-core's conjunctive-search degrade path (full query -> content words
/// -> per-term union). Used as the lexical-only fallback when there is no
/// embedder to compensate for a natural-language query.
fn degrade_lexical(mem: &mut Memvid, query: &str, k: usize) -> Result<Vec<SearchHit>, (String, &'static str)> {
    let mut hits = run_query(mem, query, k)?;

    if hits.is_empty() {
        let reduced = keywords_only(query);
        if !reduced.is_empty() && reduced != query.to_ascii_lowercase() {
            hits = run_query(mem, &reduced, k)?;
        }
    }

    if hits.is_empty() {
        let mut merged: std::collections::HashMap<u64, SearchHit> = std::collections::HashMap::new();
        for term in keywords_only(query).split_whitespace() {
            for hit in run_query(mem, term, k)? {
                let key = hit.frame_id;
                let better = merged
                    .get(&key)
                    .map(|prev| prev.score.unwrap_or(0.0) < hit.score.unwrap_or(0.0))
                    .unwrap_or(true);
                if better {
                    merged.insert(key, hit);
                }
            }
        }
        hits = merged.into_values().collect();
        hits.sort_by(|a, b| {
            b.score
                .unwrap_or(0.0)
                .partial_cmp(&a.score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(k);
    }

    Ok(hits)
}

/// Semantic search: embed the query (with the BGE query instruction) and rank
/// frames by vector distance, fetching each hit's text/title/uri by id since
/// `search_vec` only returns `{frame_id, distance}`.
fn vector_hits(
    mem: &mut Memvid,
    embedder: &LocalTextEmbedder,
    query: &str,
    k: usize,
) -> Result<Vec<RankedHit>, (String, &'static str)> {
    let embedding = match embedder.embed_text(&format!("{BGE_QUERY_PREFIX}{query}")) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("[memvidd] query embedding failed, skipping vector search: {err}");
            return Ok(Vec::new());
        }
    };

    let hits = match mem.search_vec(&embedding, k * 2) {
        Ok(h) => h,
        Err(err) => {
            eprintln!("[memvidd] vector search failed, skipping: {err}");
            return Ok(Vec::new());
        }
    };

    let mut ranked = Vec::with_capacity(hits.len());
    for hit in hits {
        let Ok(frame) = mem.frame_by_id(hit.frame_id) else {
            continue;
        };
        let Ok(text) = mem.frame_text_by_id(hit.frame_id) else {
            continue;
        };
        ranked.push(RankedHit {
            frame_id: hit.frame_id,
            uri: frame.uri.clone().unwrap_or_default(),
            title: frame.title.clone(),
            content: clean_snippet(&text),
            matches: 0,
        });
    }
    ranked.truncate(k);
    Ok(ranked)
}

/// Reciprocal Rank Fusion: combine two already-ranked hit lists by position
/// (not raw score, which isn't comparable between BM25-ish lexical scores and
/// vector distances) using the standard `1 / (60 + rank)` weighting.
fn rrf_merge(lexical: Vec<RankedHit>, vector: Vec<RankedHit>, k: usize) -> Vec<RankedHit> {
    const RRF_K: f64 = 60.0;
    let mut score: std::collections::HashMap<u64, f64> = std::collections::HashMap::new();
    let mut hit_by_id: std::collections::HashMap<u64, RankedHit> = std::collections::HashMap::new();

    for (rank, hit) in lexical.into_iter().enumerate() {
        *score.entry(hit.frame_id).or_insert(0.0) += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit_by_id.entry(hit.frame_id).or_insert(hit);
    }
    for (rank, hit) in vector.into_iter().enumerate() {
        *score.entry(hit.frame_id).or_insert(0.0) += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit_by_id.entry(hit.frame_id).or_insert(hit);
    }

    let mut ids: Vec<u64> = hit_by_id.keys().copied().collect();
    ids.sort_by(|a, b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Diversify by source document before truncating to k: a large source (many
    // chunks) would otherwise fill every slot on raw score alone, crowding out
    // smaller-but-relevant sources entirely - observed on a mixed memory (a
    // 194-page terms doc drowned out three one-page e-tickets) and even within
    // one source class (two people's low-value chunks outranking a third
    // person's only relevant chunk). `#page-N` is stripped from the URI so all
    // chunks of one uploaded document group together; round-robin across
    // groups in best-hit order so every source with *any* relevant hit gets at
    // least one slot before any source gets a second.
    let mut groups: Vec<Vec<u64>> = Vec::new();
    let mut group_index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for id in &ids {
        let doc_key = hit_by_id[id].uri.split('#').next().unwrap_or("");
        let idx = *group_index.entry(doc_key).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[idx].push(*id);
    }

    let mut selected: Vec<u64> = Vec::with_capacity(k);
    let mut cursors = vec![0usize; groups.len()];
    while selected.len() < k {
        let mut progressed = false;
        for (group, cursor) in groups.iter().zip(cursors.iter_mut()) {
            if selected.len() >= k {
                break;
            }
            if let Some(id) = group.get(*cursor) {
                selected.push(*id);
                *cursor += 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    selected.into_iter().filter_map(|id| hit_by_id.remove(&id)).collect()
}

/// One lexical search pass with the sketch pre-filter disabled.
///
/// `memvid-core` can return the same frame twice for one query (observed on a
/// chunked PDF: identical frame_id/content pair back to back), so this
/// over-fetches and dedupes by `frame_id`, keeping the first (best-ranked)
/// occurrence, before truncating to the requested `k`.
fn run_query(mem: &mut Memvid, query: &str, k: usize) -> Result<Vec<SearchHit>, (String, &'static str)> {
    let request = SearchRequest {
        query: query.to_string(),
        top_k: k * 2,
        snippet_chars: 400,
        uri: None,
        scope: None,
        cursor: None,
        as_of_frame: None,
        as_of_ts: None,
        no_sketch: true,
        // No ACL model in this PoC - no caller context, and Audit mode (the
        // Default) never blocks a result even if a frame somehow had a policy.
        acl_context: None,
        acl_enforcement_mode: Default::default(),
    };
    let hits = mem
        .search(request)
        .map(|r| r.hits)
        .map_err(|e| (format!("search failed: {e}"), "INTERNAL"))?;

    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<SearchHit> = hits
        .into_iter()
        .filter(|hit| seen.insert(hit.frame_id))
        .collect();
    deduped.truncate(k);
    Ok(deduped)
}

fn op_stats(state: &State) -> Result<Value, (String, &'static str)> {
    let path = match &state.path {
        Some(p) => Value::String(p.to_string_lossy().into_owned()),
        None => Value::Null,
    };
    let documents = match &state.mem {
        Some(mem) => mem.stats().map(|s| s.frame_count).unwrap_or(0),
        None => 0,
    };
    Ok(json!({ "path": path, "documents": documents }))
}

/// Read a `.mv2`'s frame count without touching the active store - unlike
/// `open`, this never becomes the session's active store and never conflicts
/// with a lock the active store already holds (`Memvid::open_read_only` takes
/// a shared, not exclusive, lock). Used for listing memories with per-file
/// document counts. Encrypted `.mv2e` files (no passphrase given here) and
/// anything else unreadable simply report `documents: null` rather than
/// failing the whole request - callers are expected to be listing many files
/// and should not let one bad file break the rest.
fn op_inspect(params: &Value) -> Result<Value, (String, &'static str)> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ("`params.path` is required".to_string(), "BAD_PARAMS"))?;
    let documents = Memvid::open_read_only(path)
        .ok()
        .and_then(|mem| mem.stats().ok())
        .map(|s| s.frame_count);
    Ok(json!({ "path": path, "documents": documents }))
}

/// Very small stop-word list — enough to keep a question like
/// "what time do the backups run" from over-constraining lexical search.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "at", "for", "is", "are", "was",
    "were", "be", "do", "does", "did", "how", "what", "when", "where", "why", "who", "which",
    "that", "this", "it", "its", "they", "them", "their", "you", "your", "we", "our", "i",
    "with", "as", "by", "from", "about", "into", "long", "much", "many", "can", "will",
];

/// Reduce a query to lower-cased content words (length >= 3, not a stop word).
fn keywords_only(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `memvid-core` appends a metadata footer (`\ntitle: ...`, `\nuri: ...`, `\ntags: ...`)
/// to frame text. Strip it so the host gets just the document body for RAG context.
fn clean_snippet(raw: &str) -> String {
    for marker in ["\ntitle: ", "\nuri: mv2://", "\ntags: ", "\nlabels: "] {
        if let Some(idx) = raw.find(marker) {
            return raw[..idx].trim_end().to_string();
        }
    }
    raw.trim_end().to_string()
}

fn err_line(id: &Value, message: &str, code: &str) -> String {
    json!({ "id": id, "ok": false, "error": { "message": message, "code": code } }).to_string()
}
