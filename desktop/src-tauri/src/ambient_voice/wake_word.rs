//! Wake-word normalisation, tokenisation and **strict** validation.
//!
//! # Why this module exists
//!
//! sherpa-onnx's keyword-spotting C API does not accept raw text and ships no
//! tokenizer for KWS (`modeling_unit`/`bpe_vocab` are ignored by the KWS
//! implementation). Keywords must arrive pre-tokenised into the model's own
//! vocabulary pieces, space-separated, one keyword per line.
//!
//! Feeding text the model cannot encode is **fatal, not an error**:
//!
//! * `keywords_file` / `keywords_buf` → the C library calls `_Exit(-1)`; the
//!   whole app dies with no Rust-visible error.
//! * `create_stream_with_keywords` → the C wrapper returns a null stream that
//!   the Rust binding hands back as a plain `OnlineStream` (not
//!   `Option`/`Result`), which then **SIGSEGVs on first use**.
//!
//! Both were reproduced empirically (M0 spike S1, findings 2–3). There is no
//! way to detect either condition from the Rust API after the fact, so every
//! keyword MUST be validated here, in app code, before anything reaches the
//! engine. [`encode_for_engine`] is the only supported way to build a keywords
//! payload, and it fails loudly rather than passing unvalidated bytes down.
//!
//! # The tokenisation recipe
//!
//! Validated 12/12 against the model's own `keywords_raw.txt` →
//! `keywords.txt` pair. Greedy longest-match — the obvious alternative — was
//! measured **wrong at 7/9**, so it is not an acceptable substitute.
//!
//! 1. Parse the model's `bpe.model`. Despite the name it is a SentencePiece
//!    **Unigram** `ModelProto` (not BPE merges): 500 pieces whose order is
//!    byte-identical to `tokens.txt`, each carrying a log probability.
//! 2. Normalise the phrase: uppercase, collapse whitespace, join words with
//!    U+2581 (`▁`) and prepend one U+2581 (SentencePiece `add_dummy_prefix`).
//! 3. Viterbi-segment to maximise the summed piece score. Characters no piece
//!    covers take a single-character `<unk>` arc scored
//!    `min_piece_score - 10.0` (SentencePiece's own unk penalty) so the
//!    segmentation still completes and the offending characters can be named
//!    in the error.
//! 4. Emit the pieces space-separated.
//!
//! The model vocabulary is 500 **uppercase ASCII** pieces, so accented Latin
//! and non-Latin scripts inevitably produce `<unk>` arcs and are rejected.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

/// SentencePiece word-boundary marker.
const WORD_BOUNDARY: char = '\u{2581}';

/// SentencePiece's own penalty for an unknown single-character arc, relative
/// to the lowest-scoring real piece.
const UNK_SCORE_PENALTY: f32 = 10.0;

/// Minimum letters (spaces excluded) in a normalised wake word.
///
/// M0 finding 7: short, common words ("THE", "HERE") fire constantly on
/// unrelated speech, and an acoustically overlapping near-miss can *preempt*
/// the true detection because the beam resets. A floor is the cheapest
/// mitigation that does not need per-user tuning.
pub const MIN_WAKE_WORD_LETTERS: usize = 6;

/// A single-word wake phrase needs to be longer than a multi-word one: two
/// short words still give the decoder two boundaries to agree on, one short
/// word gives it none.
pub const MIN_SINGLE_WORD_LETTERS: usize = 8;

/// Upper bound on a stored wake phrase, so a paste accident cannot produce a
/// keyword line long enough to matter to the decoder.
pub const MAX_WAKE_WORD_CHARS: usize = 64;

/// Why a wake word cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeWordError {
    Empty,
    TooShort {
        letters: usize,
        required: usize,
        single_word: bool,
    },
    /// Characters the model's vocabulary cannot represent at all.
    Unsupported {
        characters: Vec<char>,
    },
    /// A piece the tokenizer produced is absent from `tokens.txt`. This means
    /// the tokenizer and the engine disagree — the exact condition that would
    /// crash the C library — so it is always fatal for that keyword.
    UnknownPiece {
        piece: String,
    },
}

impl fmt::Display for WakeWordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "Enter a wake word"),
            Self::TooShort {
                letters,
                required,
                single_word,
            } => {
                if *single_word {
                    write!(
                        f,
                        "A one-word wake phrase needs at least {required} letters (this one has {letters}). \
                         Short words fire constantly — try two words, like \"hey buzz\"."
                    )
                } else {
                    write!(
                        f,
                        "A wake phrase needs at least {required} letters (this one has {letters}). \
                         Short phrases fire constantly on unrelated speech."
                    )
                }
            }
            Self::Unsupported { characters } => {
                let list: String = characters
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "The wake-word model only understands unaccented English letters. \
                     It cannot hear: {list}"
                )
            }
            Self::UnknownPiece { piece } => write!(
                f,
                "This wake phrase produced a token the model does not know ({piece}). Try different words."
            ),
        }
    }
}

impl std::error::Error for WakeWordError {}

/// Normalise a user-typed phrase to the model's uppercase, `▁`-joined form.
pub fn normalize(phrase: &str) -> String {
    let mut normalized = String::from(WORD_BOUNDARY);
    normalized.push_str(
        &phrase
            .to_uppercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(&WORD_BOUNDARY.to_string()),
    );
    normalized
}

fn letter_count(phrase: &str) -> usize {
    phrase.chars().filter(|c| !c.is_whitespace()).count()
}

fn word_count(phrase: &str) -> usize {
    phrase.split_whitespace().count()
}

/// Model-independent checks: non-empty and long enough to be discriminative.
///
/// Deliberately separate from [`encode_for_engine`] so the settings UI can
/// reject an obviously bad phrase before the KWS model has been downloaded.
/// Passing this is necessary but **not** sufficient — the vocabulary check in
/// [`encode_for_engine`] still runs before any engine call.
pub fn validate_wake_word(phrase: &str) -> Result<(), WakeWordError> {
    let trimmed = phrase.trim();
    if trimmed.is_empty() {
        return Err(WakeWordError::Empty);
    }
    let letters = letter_count(trimmed);
    let single_word = word_count(trimmed) < 2;
    let required = if single_word {
        MIN_SINGLE_WORD_LETTERS
    } else {
        MIN_WAKE_WORD_LETTERS
    };
    if letters < required {
        return Err(WakeWordError::TooShort {
            letters,
            required,
            single_word,
        });
    }
    Ok(())
}

// ── SentencePiece Unigram model ──────────────────────────────────────────────

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Yield `(field_number, payload)` for every length-delimited protobuf field,
/// skipping every other wire type. Enough of a parser for `ModelProto`.
fn length_delimited_fields(bytes: &[u8]) -> Vec<(u64, &[u8])> {
    let mut fields = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some(key) = read_varint(bytes, &mut cursor) else {
            break;
        };
        match key & 7 {
            0 => {
                if read_varint(bytes, &mut cursor).is_none() {
                    break;
                }
            }
            1 => cursor += 8,
            2 => {
                let Some(len) = read_varint(bytes, &mut cursor).map(|len| len as usize) else {
                    break;
                };
                if cursor + len > bytes.len() {
                    break;
                }
                fields.push((key >> 3, &bytes[cursor..cursor + len]));
                cursor += len;
            }
            5 => cursor += 4,
            _ => break,
        }
    }
    fields
}

/// SentencePiece `piece_type` for a normal (non-control, non-unk) piece.
const PIECE_TYPE_NORMAL: i32 = 1;

/// The subset of a SentencePiece `ModelProto` needed to Viterbi-segment.
pub struct WakeWordTokenizer {
    /// Normal pieces, indexed by piece text → log probability.
    scores: HashMap<String, f32>,
    max_piece_chars: usize,
    unk_score: f32,
    /// Engine-authoritative vocabulary read from `tokens.txt`. A piece absent
    /// here must never reach sherpa-onnx.
    engine_tokens: HashMap<String, i32>,
}

impl WakeWordTokenizer {
    /// Load from a model directory containing `bpe.model` and `tokens.txt`.
    pub fn load(model_dir: &Path) -> Result<Self, String> {
        let sp = std::fs::read(model_dir.join("bpe.model"))
            .map_err(|error| format!("read wake-word bpe.model: {error}"))?;
        let tokens = std::fs::read_to_string(model_dir.join("tokens.txt"))
            .map_err(|error| format!("read wake-word tokens.txt: {error}"))?;
        Self::from_parts(&sp, &tokens)
    }

    /// Build from raw bytes — the seam tests use with in-repo fixtures.
    pub fn from_parts(sentencepiece_model: &[u8], tokens_txt: &str) -> Result<Self, String> {
        let mut scores: HashMap<String, f32> = HashMap::new();
        let mut max_piece_chars = 1usize;
        let mut min_score = f32::MAX;
        let mut pieces_seen = 0usize;

        for (field_no, payload) in length_delimited_fields(sentencepiece_model) {
            // ModelProto.pieces is field 1.
            if field_no != 1 {
                continue;
            }
            let Some((piece, score, piece_type)) = decode_sentence_piece(payload) else {
                continue;
            };
            pieces_seen += 1;
            if piece_type != PIECE_TYPE_NORMAL {
                continue;
            }
            max_piece_chars = max_piece_chars.max(piece.chars().count());
            min_score = min_score.min(score);
            scores.insert(piece, score);
        }
        if scores.is_empty() {
            return Err("wake-word bpe.model contained no usable pieces".to_string());
        }

        let mut engine_tokens = HashMap::new();
        for line in tokens_txt.lines() {
            // "<piece> <id>" — split from the right so pieces containing a
            // space (there are none today, but the format permits it) survive.
            let mut parts = line.rsplitn(2, ' ');
            let Some(id) = parts.next().and_then(|id| id.parse::<i32>().ok()) else {
                continue;
            };
            let Some(piece) = parts.next() else {
                continue;
            };
            engine_tokens.insert(piece.to_string(), id);
        }
        if engine_tokens.is_empty() {
            return Err("wake-word tokens.txt contained no tokens".to_string());
        }

        // Integrity: the two files describe the same vocabulary, piece for
        // piece and in the same order (M0 spike — `bpe.model`'s piece index IS
        // the `tokens.txt` id). A count mismatch means one of them is
        // truncated or from a different model, and a tokenizer built on half a
        // vocabulary would silently produce different segmentations. Refuse
        // rather than arm the engine from a disagreement.
        if pieces_seen != engine_tokens.len() {
            return Err(format!(
                "wake-word model files disagree: bpe.model has {pieces_seen} pieces, \
                 tokens.txt has {} — the model directory is incomplete or mismatched",
                engine_tokens.len()
            ));
        }

        Ok(Self {
            scores,
            max_piece_chars,
            unk_score: min_score - UNK_SCORE_PENALTY,
            engine_tokens,
        })
    }

    /// Number of pieces the engine knows about. The conformance test asserts
    /// the model's documented 500-piece vocabulary is fully parsed.
    #[cfg(test)]
    pub fn engine_token_count(&self) -> usize {
        self.engine_tokens.len()
    }

    /// Viterbi-segment a normalised string.
    ///
    /// Returns `(pieces, unsupported_characters)`. Unsupported characters are
    /// reported rather than silently dropped so the UI can name them.
    fn viterbi(&self, normalized: &str) -> (Vec<String>, Vec<char>) {
        let chars: Vec<char> = normalized.chars().collect();
        let n = chars.len();
        let mut best = vec![f32::NEG_INFINITY; n + 1];
        // (previous index, piece, is_unknown)
        let mut prev: Vec<Option<(usize, String, bool)>> = vec![None; n + 1];
        best[0] = 0.0;

        for i in 0..n {
            if best[i] == f32::NEG_INFINITY {
                continue;
            }
            for j in (i + 1)..=usize::min(n, i + self.max_piece_chars) {
                let candidate: String = chars[i..j].iter().collect();
                let Some(score) = self.scores.get(&candidate) else {
                    continue;
                };
                let total = best[i] + score;
                if total > best[j] {
                    best[j] = total;
                    prev[j] = Some((i, candidate, false));
                }
            }
            // Single-character unknown fallback keeps the lattice connected.
            let unk_total = best[i] + self.unk_score;
            if unk_total > best[i + 1] {
                best[i + 1] = unk_total;
                prev[i + 1] = Some((i, chars[i].to_string(), true));
            }
        }

        let mut pieces = Vec::new();
        let mut unsupported = Vec::new();
        let mut i = n;
        while i > 0 {
            // Every position is reachable: each step has at least the unknown
            // arc, so the backtrace cannot break. Treat a gap as "unsupported"
            // rather than panicking — this runs on user input.
            let Some((previous, piece, is_unknown)) = prev[i].clone() else {
                unsupported.push(chars[i - 1]);
                i -= 1;
                continue;
            };
            if is_unknown {
                if let Some(c) = piece.chars().next() {
                    unsupported.push(c);
                }
            }
            pieces.push(piece);
            i = previous;
        }
        pieces.reverse();
        unsupported.reverse();
        (pieces, unsupported)
    }

    /// Tokenise one phrase and prove every piece exists in `tokens.txt`.
    ///
    /// This is the gate the M0 spike showed is mandatory. Nothing else in this
    /// crate may construct a keywords payload.
    pub fn tokenize(&self, phrase: &str) -> Result<Vec<String>, WakeWordError> {
        validate_wake_word(phrase)?;
        let (pieces, unsupported) = self.viterbi(&normalize(phrase));
        if !unsupported.is_empty() {
            let mut characters = unsupported;
            characters.dedup();
            return Err(WakeWordError::Unsupported { characters });
        }
        for piece in &pieces {
            if !self.engine_tokens.contains_key(piece) {
                return Err(WakeWordError::UnknownPiece {
                    piece: piece.clone(),
                });
            }
        }
        if pieces.is_empty() {
            return Err(WakeWordError::Empty);
        }
        Ok(pieces)
    }

    /// Build the exact `keywords_buf` payload for a set of phrases.
    ///
    /// **Empty input arms nothing.** The proven-safe representation of "no
    /// wake words" is a buffer containing a single newline: the C library
    /// accepts it and zero keywords are armed (M0 finding 5 — runtime keywords
    /// MERGE with the configured set, so the configured set must also be
    /// empty, which is what [`super::session`] does).
    ///
    /// Returns `Err` on the first phrase that cannot be encoded, naming it, so
    /// a caller can neither ignore nor partially apply the failure.
    pub fn keywords_buf(&self, phrases: &[String]) -> Result<String, (String, WakeWordError)> {
        if phrases.is_empty() {
            return Ok("\n".to_string());
        }
        let mut lines = Vec::with_capacity(phrases.len());
        for phrase in phrases {
            let pieces = self
                .tokenize(phrase)
                .map_err(|error| (phrase.clone(), error))?;
            lines.push(pieces.join(" "));
        }
        // Trailing newline: the C parser reads line-delimited keywords.
        Ok(format!("{}\n", lines.join("\n")))
    }
}

/// Decode one `SentencePiece` message: `piece` (1, LEN), `score` (2, I32
/// float), `type` (3, VARINT).
fn decode_sentence_piece(bytes: &[u8]) -> Option<(String, f32, i32)> {
    let mut piece = String::new();
    let mut score = 0.0f32;
    let mut piece_type = PIECE_TYPE_NORMAL;
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let key = read_varint(bytes, &mut cursor)?;
        match (key >> 3, key & 7) {
            (1, 2) => {
                let len = read_varint(bytes, &mut cursor)? as usize;
                let end = cursor.checked_add(len)?;
                piece = String::from_utf8_lossy(bytes.get(cursor..end)?).into_owned();
                cursor = end;
            }
            (2, 5) => {
                let end = cursor.checked_add(4)?;
                let raw = bytes.get(cursor..end)?;
                score = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                cursor = end;
            }
            (3, 0) => piece_type = read_varint(bytes, &mut cursor)? as i32,
            (_, 0) => {
                read_varint(bytes, &mut cursor)?;
            }
            (_, 2) => {
                let len = read_varint(bytes, &mut cursor)? as usize;
                cursor = cursor.checked_add(len)?;
            }
            (_, 5) => cursor += 4,
            (_, 1) => cursor += 8,
            _ => return None,
        }
    }
    Some((piece, score, piece_type))
}

#[cfg(test)]
#[path = "wake_word_tests.rs"]
mod wake_word_tests;
