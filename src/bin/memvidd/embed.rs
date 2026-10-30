//! Multilingual text embeddings for memvidd.
//!
//! `intfloat/multilingual-e5-base` (the int8 ONNX conversion published as
//! `Xenova/multilingual-e5-base`) run through ONNX Runtime directly. memvid-core's
//! `LocalTextEmbedder` can't be used for it: it only knows a fixed list of English BERT
//! models (unknown names silently fall back to bge-small) and pools the CLS token, while
//! e5 needs mean pooling plus a `query: ` / `passage: ` prefix on every input.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ort::session::{Session, SessionInputValue, builder::GraphOptimizationLevel};
use ort::value::Tensor;
use tokenizers::tokenizer::{Tokenizer, TruncationParams};
use tokenizers::{
    PaddingDirection, PaddingParams, PaddingStrategy, TruncationDirection, TruncationStrategy,
};

/// Model name recorded as the vector-index binding of every store this build writes.
pub const MODEL_NAME: &str = "multilingual-e5-base";
pub const DIMENSIONS: usize = 768;
const MAX_TOKENS: usize = 512;
/// `<pad>` in the XLM-RoBERTa vocabulary.
const PAD_ID: u32 = 1;
const BATCH_SIZE: usize = 16;
const QUERY_PREFIX: &str = "query: ";
const PASSAGE_PREFIX: &str = "passage: ";

pub struct E5Embedder {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    input_names: Vec<String>,
    output_name: String,
}

impl E5Embedder {
    /// Load the model from memvid's model cache (see [`model_dir`]).
    pub fn load() -> Result<Self, String> {
        let dir = model_dir();
        Self::load_from(
            &dir.join(format!("{MODEL_NAME}.onnx")),
            &dir.join(format!("{MODEL_NAME}_tokenizer.json")),
        )
    }

    pub fn load_from(model_path: &Path, tokenizer_path: &Path) -> Result<Self, String> {
        for path in [model_path, tokenizer_path] {
            if !path.exists() {
                return Err(format!(
                    "{} is missing - see docs/memvidd.md (Semantic search) for the one-time download",
                    path.display()
                ));
            }
        }

        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| format!("cannot load tokenizer: {e}"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id: PAD_ID,
            pad_type_id: 0,
            pad_token: "<pad>".to_string(),
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                strategy: TruncationStrategy::LongestFirst,
                stride: 0,
                direction: TruncationDirection::Right,
            }))
            .map_err(|e| format!("cannot configure truncation: {e}"))?;

        let session = Session::builder()
            .map_err(|e| format!("cannot create ONNX session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| format!("cannot set optimization level: {e}"))?
            .with_intra_threads(4)
            .map_err(|e| format!("cannot set intra threads: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| format!("cannot load {}: {e}", model_path.display()))?;

        let input_names = session
            .inputs
            .iter()
            .map(|input| input.name.clone())
            .collect();
        let output_name = session
            .outputs
            .iter()
            .find(|output| output.name == "last_hidden_state")
            .or_else(|| session.outputs.first())
            .map(|output| output.name.clone())
            .ok_or_else(|| "the embedding model declares no outputs".to_string())?;

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            input_names,
            output_name,
        })
    }

    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>, String> {
        self.embed(QUERY_PREFIX, &[query])?
            .pop()
            .ok_or_else(|| "the model returned no embedding".to_string())
    }

    pub fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        self.embed(PASSAGE_PREFIX, texts)
    }

    fn embed(&self, prefix: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH_SIZE) {
            out.extend(self.run_batch(prefixed(prefix, batch))?);
        }
        Ok(out)
    }

    fn run_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        let encodings = self
            .tokenizer
            .encode_batch(texts, true)
            .map_err(|e| format!("tokenization failed: {e}"))?;
        let rows = encodings.len();
        let seq = encodings
            .first()
            .map_or(0, |encoding| encoding.get_ids().len());

        let mut ids = Vec::with_capacity(rows * seq);
        let mut mask = Vec::with_capacity(rows * seq);
        for encoding in &encodings {
            ids.extend(encoding.get_ids().iter().map(|&id| i64::from(id)));
            mask.extend(encoding.get_attention_mask().iter().map(|&m| i64::from(m)));
        }

        let mut feed: Vec<(Cow<'static, str>, SessionInputValue<'static>)> = Vec::new();
        for name in &self.input_names {
            let data = match name.as_str() {
                "input_ids" => ids.clone(),
                "attention_mask" => mask.clone(),
                "token_type_ids" => vec![0; rows * seq],
                other => return Err(format!("unexpected model input `{other}`")),
            };
            let tensor = Tensor::from_array((vec![rows as i64, seq as i64], data))
                .map_err(|e| format!("cannot build `{name}` tensor: {e}"))?;
            feed.push((Cow::Owned(name.clone()), SessionInputValue::from(tensor)));
        }

        let mut session = self
            .session
            .lock()
            .map_err(|_| "the embedding session lock is poisoned".to_string())?;
        let outputs = session
            .run(feed)
            .map_err(|e| format!("inference failed: {e}"))?;
        let output = outputs
            .get(self.output_name.as_str())
            .ok_or_else(|| format!("the model returned no `{}` output", self.output_name))?;
        let (shape, hidden) = output
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("cannot read embeddings: {e}"))?;
        if shape.len() != 3 || shape[0] as usize != rows || shape[1] as usize != seq {
            return Err(format!("unexpected output shape {:?}", &shape[..]));
        }
        let dims = shape[2] as usize;
        if dims != DIMENSIONS {
            return Err(format!("expected {DIMENSIONS}-d embeddings, got {dims}-d"));
        }

        Ok(mean_pool(hidden, &mask, rows, seq, dims)
            .into_iter()
            .map(l2_normalize)
            .collect())
    }
}

/// Where memvid keeps text-embedding models: `<cache dir>/memvid/text-models`, the same
/// directory memvid-core's `LocalTextEmbedder` uses (`~/Library/Caches/...` on macOS,
/// `~/.cache/...` on Linux).
pub fn model_dir() -> PathBuf {
    dirs_next::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("memvid")
        .join("text-models")
}

fn prefixed(prefix: &str, texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| format!("{prefix}{text}")).collect()
}

/// Per row, the mean of the token vectors whose attention-mask entry is 1, so padding
/// never dilutes a shorter text in the same batch.
fn mean_pool(hidden: &[f32], mask: &[i64], rows: usize, seq: usize, dims: usize) -> Vec<Vec<f32>> {
    (0..rows)
        .map(|row| {
            let mut sum = vec![0.0f32; dims];
            let mut count = 0.0f32;
            for token in 0..seq {
                if mask[row * seq + token] == 0 {
                    continue;
                }
                let start = (row * seq + token) * dims;
                for (acc, value) in sum.iter_mut().zip(&hidden[start..start + dims]) {
                    *acc += value;
                }
                count += 1.0;
            }
            let count = count.max(1.0);
            sum.iter_mut().for_each(|value| *value /= count);
            sum
        })
        .collect()
}

fn l2_normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|value| *value /= norm);
    }
    vector
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_pool_ignores_padding_tokens() {
        // two rows, three tokens, two dims; the second row's last token is padding
        let hidden = [
            1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 10.0, 20.0, 30.0, 40.0, 99.0, 99.0,
        ];
        let mask = [1, 1, 1, 1, 1, 0];

        let pooled = mean_pool(&hidden, &mask, 2, 3, 2);

        assert_eq!(pooled, vec![vec![3.0, 4.0], vec![20.0, 30.0]]);
    }

    #[test]
    fn l2_normalize_returns_unit_vectors_and_keeps_zero_vectors() {
        assert_eq!(l2_normalize(vec![3.0, 4.0]), vec![0.6, 0.8]);
        assert_eq!(l2_normalize(vec![0.0, 0.0]), vec![0.0, 0.0]);
    }

    #[test]
    fn inputs_get_the_e5_prefix() {
        assert_eq!(
            prefixed(PASSAGE_PREFIX, &["a", "b"]),
            vec!["passage: a".to_string(), "passage: b".to_string()]
        );
    }

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    /// Runs only where the model has been downloaded (see docs/memvidd.md).
    #[test]
    fn czech_question_lands_closer_to_its_english_answer_than_to_unrelated_text() {
        let Ok(embedder) = E5Embedder::load() else {
            eprintln!("skipping: {MODEL_NAME} is not downloaded");
            return;
        };

        let query = embedder
            .embed_query("Kolik stojí jízdenka do Prahy?")
            .unwrap();
        let passages = embedder
            .embed_passages(&[
                "A train ticket to Prague costs 250 crowns.",
                "Photosynthesis converts light into chemical energy in plants.",
            ])
            .unwrap();

        assert_eq!(query.len(), DIMENSIONS);
        assert!(dot(&query, &passages[0]) > dot(&query, &passages[1]));
    }
}
