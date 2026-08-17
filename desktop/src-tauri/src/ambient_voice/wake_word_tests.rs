//! Wake-word tokenizer and validation tests.
//!
//! The conformance test below is the load-bearing one: it segments the KWS
//! model's own `keywords_raw.txt` and compares against the model's own
//! `keywords.txt`. Every line must match exactly. A mismatch means the app
//! would hand sherpa-onnx tokens it does not recognise, which kills the
//! process (`_Exit(-1)`) or segfaults on first stream use — neither of which
//! is observable from Rust, so this test is the only place the defect can be
//! caught.

use super::*;

/// The model's SentencePiece Unigram vocabulary. See the sibling `NOTICE.md`.
const BPE_MODEL: &[u8] = include_bytes!("../../resources/ambient-voice-test-vocab/bpe.model");
const TOKENS_TXT: &str = include_str!("../../resources/ambient-voice-test-vocab/tokens.txt");
const KEYWORDS_RAW: &str =
    include_str!("../../resources/ambient-voice-test-vocab/keywords_raw.txt");
const KEYWORDS_TOKENIZED: &str =
    include_str!("../../resources/ambient-voice-test-vocab/keywords.txt");

fn tokenizer() -> WakeWordTokenizer {
    WakeWordTokenizer::from_parts(BPE_MODEL, TOKENS_TXT).expect("load wake-word tokenizer")
}

#[test]
fn segmentation_matches_the_models_own_tokenised_keywords() {
    let sp = tokenizer();
    let expected: Vec<&str> = KEYWORDS_TOKENIZED
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let raw: Vec<&str> = KEYWORDS_RAW
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(raw.len(), expected.len(), "fixture pair is misaligned");
    assert!(raw.len() >= 9, "expected the model's full keyword set");

    for (phrase, want) in raw.iter().zip(expected.iter()) {
        // The model's own keyword list contains short phrases ("ALEXA",
        // "GO HOME") that Buzz would refuse as wake words. Segmentation is
        // tested directly so the length policy does not hide the oracle.
        let (pieces, unsupported) = sp.viterbi(&normalize(phrase));
        assert!(
            unsupported.is_empty(),
            "{phrase}: unexpected unsupported characters {unsupported:?}"
        );
        assert_eq!(
            pieces.join(" "),
            *want,
            "segmentation mismatch for {phrase}"
        );
    }
}

#[test]
fn every_piece_the_tokenizer_emits_exists_in_the_engine_vocabulary() {
    let sp = tokenizer();
    assert_eq!(sp.engine_token_count(), 500);
    for phrase in [
        "hey hermes",
        "hey buzz there",
        "computer wake up",
        "good morning buzz",
    ] {
        let pieces = sp.tokenize(phrase).expect("tokenize");
        assert!(!pieces.is_empty());
        for piece in pieces {
            assert!(
                sp.engine_tokens.contains_key(&piece),
                "{phrase}: piece {piece} is absent from tokens.txt"
            );
        }
    }
}

#[test]
fn greedy_longest_match_is_not_the_recipe() {
    // Guards against a future "simplification" back to greedy matching, which
    // the M0 spike measured wrong at 7/9 against this same oracle.
    let sp = tokenizer();
    let greedy = |phrase: &str| -> Vec<String> {
        let chars: Vec<char> = normalize(phrase).chars().collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < chars.len() {
            let mut taken = 0usize;
            for j in (i + 1..=chars.len()).rev() {
                let candidate: String = chars[i..j].iter().collect();
                if sp.scores.contains_key(&candidate) {
                    out.push(candidate);
                    taken = j - i;
                    break;
                }
            }
            if taken == 0 {
                out.push(chars[i].to_string());
                taken = 1;
            }
            i += taken;
        }
        out
    };

    let raw: Vec<&str> = KEYWORDS_RAW
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let expected: Vec<&str> = KEYWORDS_TOKENIZED
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let disagreements = raw
        .iter()
        .zip(expected.iter())
        .filter(|(phrase, want)| greedy(phrase).join(" ") != **want)
        .count();
    assert!(
        disagreements > 0,
        "greedy matching now agrees everywhere — the oracle has changed, re-derive the recipe"
    );
}

#[test]
fn accented_and_non_latin_phrases_are_rejected_before_the_engine() {
    let sp = tokenizer();
    for phrase in ["héllo thère", "привет ассистент", "こんにちは ブザー"]
    {
        match sp.tokenize(phrase) {
            Err(WakeWordError::Unsupported { characters }) => {
                assert!(!characters.is_empty(), "{phrase}: no characters named");
            }
            other => panic!("{phrase}: expected Unsupported, got {other:?}"),
        }
    }
}

#[test]
fn short_and_empty_phrases_are_rejected() {
    assert_eq!(validate_wake_word("   "), Err(WakeWordError::Empty));
    // "the" and "here" fire freely — M0 finding 7.
    assert!(matches!(
        validate_wake_word("the"),
        Err(WakeWordError::TooShort { .. })
    ));
    assert!(matches!(
        validate_wake_word("here"),
        Err(WakeWordError::TooShort { .. })
    ));
    // A single word must clear the higher bar.
    assert!(matches!(
        validate_wake_word("alexa"),
        Err(WakeWordError::TooShort {
            single_word: true,
            required: MIN_SINGLE_WORD_LETTERS,
            ..
        })
    ));
    assert!(validate_wake_word("computer").is_ok());
    // Two words clear the lower bar.
    assert!(validate_wake_word("hey buzz").is_ok());
    assert!(matches!(
        validate_wake_word("hi yo"),
        Err(WakeWordError::TooShort {
            single_word: false,
            required: MIN_WAKE_WORD_LETTERS,
            ..
        })
    ));
}

#[test]
fn an_empty_binding_set_arms_nothing() {
    // M0 finding 5: runtime keywords MERGE with the configured set, and the
    // proven-safe representation of "no wake words" is a single newline.
    let sp = tokenizer();
    assert_eq!(sp.keywords_buf(&[]).expect("empty buf"), "\n");
}

#[test]
fn keywords_buf_is_line_delimited_and_newline_terminated() {
    let sp = tokenizer();
    let buf = sp
        .keywords_buf(&["hey hermes".to_string(), "good morning buzz".to_string()])
        .expect("keywords buf");
    assert!(buf.ends_with('\n'));
    let lines: Vec<&str> = buf.trim_end_matches('\n').split('\n').collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        assert!(!line.is_empty());
        for piece in line.split(' ') {
            assert!(
                sp.engine_tokens.contains_key(piece),
                "piece {piece} is absent from tokens.txt"
            );
        }
    }
}

#[test]
fn keywords_buf_fails_loudly_and_names_the_offending_phrase() {
    let sp = tokenizer();
    let (phrase, error) = sp
        .keywords_buf(&["hey hermes".to_string(), "привет ассистент".to_string()])
        .expect_err("un-encodable phrase must fail");
    assert_eq!(phrase, "привет ассистент");
    assert!(matches!(error, WakeWordError::Unsupported { .. }));
}

#[test]
fn normalisation_uppercases_and_marks_word_boundaries() {
    assert_eq!(normalize("hey hermes"), "\u{2581}HEY\u{2581}HERMES");
    assert_eq!(normalize("  hey   hermes  "), "\u{2581}HEY\u{2581}HERMES");
}

#[test]
fn a_truncated_model_is_an_error_not_a_panic() {
    // A truncated `bpe.model` parses cleanly right up to the cut — it just
    // yields a smaller vocabulary, and a tokenizer built on it would emit
    // *different* pieces that still exist in tokens.txt and so would sail
    // through validation into the engine. The count check is what catches it.
    let Err(error) = WakeWordTokenizer::from_parts(&BPE_MODEL[..64], TOKENS_TXT) else {
        panic!("a truncated bpe.model must be refused");
    };
    assert!(error.contains("disagree"), "{error}");
    assert!(WakeWordTokenizer::from_parts(BPE_MODEL, "").is_err());
    // Half a tokens.txt is the mirror-image corruption.
    let half: String = TOKENS_TXT.lines().take(250).collect::<Vec<_>>().join("\n");
    assert!(WakeWordTokenizer::from_parts(BPE_MODEL, &half).is_err());
}

#[test]
fn error_messages_name_what_the_user_must_change() {
    let sp = tokenizer();
    let message = sp
        .tokenize("héllo thère")
        .expect_err("unsupported")
        .to_string();
    assert!(message.contains("cannot hear"), "{message}");
    let message = validate_wake_word("the")
        .expect_err("too short")
        .to_string();
    assert!(message.contains("at least"), "{message}");
}
