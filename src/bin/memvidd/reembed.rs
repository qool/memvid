//! `memvidd --reembed <file.mv2>...`: rebuild stores whose vectors come from another
//! embedding model - every store written before the multilingual model holds 384-d
//! bge-small vectors - so they get semantic search again.
//!
//! Each file is rebuilt into a sibling temp file from its active documents (original
//! text, uri, title, tags, metadata and timestamp) with fresh embeddings, then renamed
//! over the original. The original stays locked for the whole rebuild, so a running
//! ragtime can't write to it in the meantime, and any failure leaves it untouched.
//! Superseded and deleted frames are not carried over.

use std::path::Path;
use std::time::Instant;

use memvid_core::types::{FrameRole, FrameStatus};
use memvid_core::{Memvid, PutOptions};

use crate::embed::{self, E5Embedder};

pub fn run(paths: &[String], embedder: Option<&E5Embedder>) -> i32 {
    let Some(embedder) = embedder else {
        eprintln!(
            "[memvidd] --reembed needs the embedding model; see docs/memvidd.md (Semantic search)"
        );
        return 1;
    };
    if paths.is_empty() {
        eprintln!("usage: memvidd --reembed <file.mv2>...");
        return 2;
    }

    let mut failed = 0;
    for path in paths {
        match reembed_file(Path::new(path), embedder) {
            Ok(Outcome::AlreadyCurrent) => println!("{path}: already uses {}", embed::MODEL_NAME),
            Ok(Outcome::Rebuilt {
                documents,
                frames,
                seconds,
            }) => println!(
                "{path}: {documents} documents ({frames} frames) re-embedded with {} in {seconds:.1}s",
                embed::MODEL_NAME
            ),
            Err(err) => {
                failed += 1;
                eprintln!("{path}: FAILED, original left untouched: {err}");
            }
        }
    }
    i32::from(failed > 0)
}

enum Outcome {
    AlreadyCurrent,
    Rebuilt {
        documents: usize,
        frames: u64,
        seconds: f64,
    },
}

/// One active document to carry over: its original text plus the put options that
/// reproduce its metadata.
struct Document {
    content: String,
    options: PutOptions,
}

fn reembed_file(path: &Path, embedder: &E5Embedder) -> Result<Outcome, String> {
    if path.extension().is_some_and(|ext| ext == "mv2e") {
        return Err("encrypted .mv2e stores are not supported".to_string());
    }
    let started = Instant::now();
    let mut source = Memvid::open(path).map_err(|e| format!("cannot open: {e}"))?;
    if is_current(&mut source)? {
        return Ok(Outcome::AlreadyCurrent);
    }
    let documents = read_documents(&mut source)?;

    let temp = path.with_extension("mv2.reembed-tmp");
    let _ = std::fs::remove_file(&temp);
    let frames = match write_store(&temp, &documents, embedder) {
        Ok(frames) => frames,
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
    };
    std::fs::rename(&temp, path).map_err(|e| format!("cannot replace the original: {e}"))?;
    drop(source);

    Ok(Outcome::Rebuilt {
        documents: documents.len(),
        frames,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// True when the store's vectors already come from this build's model. Only calls
/// `set_vec_model` once the dimension matches, for the reason given on
/// `bind_vector_model` in main.rs.
fn is_current(mem: &mut Memvid) -> Result<bool, String> {
    match mem.effective_vec_index_dimension() {
        Ok(Some(dims)) if dims as usize == embed::DIMENSIONS => {
            Ok(mem.set_vec_model(embed::MODEL_NAME).is_ok())
        }
        Ok(_) => Ok(false),
        Err(e) => Err(format!("cannot read the vector dimension: {e}")),
    }
}

fn read_documents(mem: &mut Memvid) -> Result<Vec<Document>, String> {
    let mut documents = Vec::new();
    for id in 0..mem.frame_count() as u64 {
        let frame = mem
            .frame_by_id(id)
            .map_err(|e| format!("cannot read frame {id}: {e}"))?;
        if frame.role != FrameRole::Document || frame.status != FrameStatus::Active {
            continue;
        }
        let payload = mem
            .frame_canonical_payload(id)
            .map_err(|e| format!("cannot read the text of frame {id}: {e}"))?;
        let content =
            String::from_utf8(payload).map_err(|_| format!("frame {id} is not UTF-8 text"))?;
        // Tags are copied as stored, auto-generated ones included, so auto-tagging is off.
        let options = PutOptions {
            timestamp: Some(frame.timestamp),
            track: frame.track,
            kind: frame.kind,
            uri: frame.uri,
            title: frame.title,
            metadata: frame.metadata,
            tags: frame.tags,
            labels: frame.labels,
            extra_metadata: frame.extra_metadata,
            auto_tag: false,
            ..crate::untimed(PutOptions::default())
        };
        documents.push(Document { content, options });
    }
    Ok(documents)
}

/// Write `documents` into a new store at `path`, every one embedded (an embedding
/// failure fails the whole rebuild rather than leaving documents without vectors),
/// and check the result before the caller swaps it in. Returns the frame count.
fn write_store(path: &Path, documents: &[Document], embedder: &E5Embedder) -> Result<u64, String> {
    let mut mem =
        Memvid::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    mem.set_vec_model(embed::MODEL_NAME)
        .map_err(|e| e.to_string())?;
    for (index, document) in documents.iter().enumerate() {
        let embeddings = crate::embed_document(&mem, embedder, &document.content)
            .map_err(|e| format!("embedding document {} failed: {e}", index + 1))?;
        crate::put_document(
            &mut mem,
            &document.content,
            Some(embeddings),
            document.options.clone(),
        )
        .map_err(|(message, _)| message)?;
        // Commit per document: one commit over several re-put documents with their
        // copied metadata failed with "Embedded WAL is corrupted ... checksum mismatch"
        // (a 3-document store, reproducible; memvid-core has open WAL fixes upstream).
        mem.commit()
            .map_err(|e| format!("commit after document {} failed: {e}", index + 1))?;
    }
    mem.commit().map_err(|e| format!("commit failed: {e}"))?;

    let mut active = 0;
    for id in 0..mem.frame_count() as u64 {
        let frame = mem
            .frame_by_id(id)
            .map_err(|e| format!("cannot verify frame {id}: {e}"))?;
        if frame.role == FrameRole::Document && frame.status == FrameStatus::Active {
            active += 1;
        }
    }
    if active != documents.len() {
        return Err(format!(
            "rebuilt store has {active} documents, expected {}",
            documents.len()
        ));
    }
    let dims = mem
        .effective_vec_index_dimension()
        .map_err(|e| format!("cannot verify the vector dimension: {e}"))?;
    if !documents.is_empty() && dims != Some(embed::DIMENSIONS as u32) {
        return Err(format!(
            "rebuilt store has {dims:?}-d vectors, expected {}",
            embed::DIMENSIONS
        ));
    }
    mem.stats()
        .map(|s| s.frame_count)
        .map_err(|e| format!("cannot read stats: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only where the model has been downloaded (see docs/memvidd.md).
    #[test]
    fn rebuilds_a_legacy_store_keeping_documents_and_metadata() {
        let Ok(embedder) = E5Embedder::load() else {
            eprintln!("skipping: {} is not downloaded", embed::MODEL_NAME);
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.mv2");
        let long = "Vlak do Kjóta odjíždí v 9:15 z nástupiště 3. ".repeat(80);
        let documents = [
            ("ticket.pdf", long.as_str()),
            ("note.txt", "Krátká poznámka o hotelu."),
        ];
        {
            let mut mem = Memvid::create(&path).unwrap();
            for (name, text) in documents {
                let options = PutOptions::builder()
                    .title(name)
                    .uri(format!("mv2://doc/{name}"))
                    .tag("source", name)
                    .build();
                // 384-d, like every store written with the old bge-small model
                mem.put_with_embedding_and_options(text.as_bytes(), vec![0.1; 384], options)
                    .unwrap();
            }
            mem.commit().unwrap();
        }

        let outcome = reembed_file(&path, &embedder).unwrap();

        assert!(matches!(outcome, Outcome::Rebuilt { documents: 2, .. }));
        let mut mem = Memvid::open(&path).unwrap();
        assert_eq!(
            mem.effective_vec_index_dimension().unwrap(),
            Some(embed::DIMENSIONS as u32)
        );
        let rebuilt = read_documents(&mut mem).unwrap();
        drop(mem);
        for (document, (name, text)) in rebuilt.iter().zip(documents) {
            assert_eq!(document.content, text.trim_end());
            assert_eq!(document.options.title.as_deref(), Some(name));
            assert_eq!(
                document.options.uri.as_deref(),
                Some(format!("mv2://doc/{name}").as_str())
            );
            assert_eq!(
                document
                    .options
                    .extra_metadata
                    .get("source")
                    .map(String::as_str),
                Some(name)
            );
        }
        assert!(matches!(
            reembed_file(&path, &embedder).unwrap(),
            Outcome::AlreadyCurrent
        ));
    }
}
