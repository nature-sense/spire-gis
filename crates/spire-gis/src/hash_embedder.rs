// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! Key-free embedder for SeleneDB vector search.
//!
//! Implements `spire_core::models::embedding::Embedder` with a deterministic
//! hashed token/character-ngram bag-of-words vector (L2-normalised). It works
//! offline with no model download, so the whole semantic-search pipeline runs
//! in the GIS app today. Swapping in a neural embedder (e.g. CandleEmbedder)
//! later is a one-line change at `InitializeEmbedder`.

use async_trait::async_trait;
use spire_core::models::embedding::{Embedder, Embedding};

/// Deterministic hashing embedder (cosine-comparable, fixed dimension).
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new() -> Self {
        Self { dim: 384 }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Lowercase alphanumeric tokens: words + overlapping 2/3-gram windows of the
/// whole string so substrings ("expressway" vs "expressways") still overlap.
fn tokens(text: &str) -> Vec<String> {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let words: Vec<String> = cleaned.split_whitespace().map(String::from).collect();
    let mut out = words.clone();
    // word bigrams
    for w in words.windows(2) {
        out.push(format!("{} {}", w[0], w[1]));
    }
    // character n-grams over the raw cleaned text
    let compact: Vec<char> = cleaned.chars().filter(|c| *c != ' ').collect();
    for n in [2usize, 3] {
        if compact.len() >= n {
            for w in compact.windows(n) {
                out.push(w.iter().collect());
            }
        }
    }
    out
}

impl HashEmbedder {
    fn vectorize(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for t in tokens(text) {
            let idx = (fnv1a(&t) % self.dim as u64) as usize;
            v[idx] += 1.0;
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-8 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        v
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    async fn embed(&self, text: &str) -> anyhow::Result<Embedding> {
        Ok(Embedding::new(self.vectorize(text), text, "spire-gis-hash-v1"))
    }

    async fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Embedding>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(Embedding::new(self.vectorize(t), t, "spire-gis-hash-v1"));
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dim
    }
}
