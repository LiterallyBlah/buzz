//! Audio-worker seam tests.
//!
//! Two groups:
//!
//! * Pure seams (resampling, PCM decoding, engine pre-flight) — always run.
//! * Fixture-driven tests through the **real** sherpa-onnx keyword spotter.
//!   Those need the ~18 MB model, which is downloaded at runtime and is not in
//!   the repository, so they are `#[ignore]`d and read the model directory from
//!   `BUZZ_AMBIENT_KWS_MODEL_DIR`. Run them with:
//!
//!   ```text
//!   BUZZ_AMBIENT_KWS_MODEL_DIR=/path/to/sherpa-onnx-kws-zipformer-… \
//!     cargo test --lib ambient_voice::session -- --ignored --nocapture
//!   ```
//!
//!   They are the only place the tokenizer → engine contract is exercised
//!   against the real library. Everything else about that contract is
//!   unobservable from Rust: bad input kills the process rather than erroring.

use super::*;
use crate::ambient_voice::wake_word::WakeWordTokenizer;

const MODEL_DIR_ENV: &str = "BUZZ_AMBIENT_KWS_MODEL_DIR";

fn f32_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[test]
fn pcm_bytes_decode_as_little_endian_f32() {
    let samples = [0.0f32, 1.0, -1.0, 0.25];
    assert_eq!(bytes_to_f32(&f32_bytes(&samples)), samples);
    // A trailing partial sample is ignored rather than misaligning the stream.
    let mut bytes = f32_bytes(&samples);
    bytes.push(0x7f);
    assert_eq!(bytes_to_f32(&bytes).len(), 4);
}

#[test]
fn sixteen_kilohertz_input_passes_straight_through() {
    // The pass-through exists so a 16 kHz WAV fixture reaches the engines
    // without the resampler standing in between.
    let mut resampler = Resampler::new(16_000).expect("resampler");
    let input: Vec<f32> = (0..VAD_FRAME_SAMPLES * 3).map(|i| i as f32).collect();
    let chunks = resampler.push(&input);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks.concat(), input);
}

#[test]
fn forty_eight_kilohertz_input_is_resampled_to_a_third_the_samples() {
    let mut resampler = Resampler::new(48_000).expect("resampler");
    // One second of a 440 Hz tone at 48 kHz.
    let input: Vec<f32> = (0..48_000)
        .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin())
        .collect();
    let produced: usize = resampler.push(&input).iter().map(Vec::len).sum();
    // 3:1 decimation, minus whatever the final partial chunk holds back.
    assert!(
        (15_000..=16_000).contains(&produced),
        "expected ~16000 output samples, got {produced}"
    );
}

#[test]
fn the_resampler_holds_partial_chunks_instead_of_emitting_short_ones() {
    let mut resampler = Resampler::new(16_000).expect("resampler");
    assert!(resampler.push(&vec![0.0; VAD_FRAME_SAMPLES - 1]).is_empty());
    let chunks = resampler.push(&[0.0]);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), VAD_FRAME_SAMPLES);
}

#[test]
fn an_incomplete_model_directory_is_refused_before_the_engine_is_touched() {
    // sherpa-onnx does not report a missing file — it fails deep inside ONNX
    // or, worse, succeeds with a half-built spotter. Pre-flight in app code.
    let dir = tempfile::tempdir().expect("temp dir");
    let Err(error) = create_keyword_spotter(dir.path(), "\u{2581}HE Y\n") else {
        panic!("an incomplete model directory must be refused");
    };
    assert!(error.contains("incomplete"), "{error}");
}

#[test]
fn an_empty_keywords_payload_is_refused() {
    // "" is not the same as "\n": the empty string means "the caller forgot",
    // while a single newline is the proven-safe representation of no keywords.
    let dir = tempfile::tempdir().expect("temp dir");
    let Err(error) = create_keyword_spotter(dir.path(), "") else {
        panic!("an empty keywords payload must be refused");
    };
    assert!(error.contains("empty payload"), "{error}");
}

#[test]
fn the_beam_and_trailing_blank_settings_match_the_spike_findings() {
    // Regression guard on two constants the M0 spike measured, both of which
    // are silent failures if a later change reverts them to sherpa's defaults:
    // beam 4 drops true detections with >4 armed keywords, and 1 trailing
    // blank makes the spotter fire on partial phrases.
    assert!(
        (8..=16).contains(&MAX_ACTIVE_PATHS),
        "max_active_paths must stay in the 8–16 band, got {MAX_ACTIVE_PATHS}"
    );
    assert!(
        (2..=4).contains(&NUM_TRAILING_BLANKS),
        "num_trailing_blanks must stay in the 2–4 band, got {NUM_TRAILING_BLANKS}"
    );
}

// ── Fixture-driven tests through the real engine ─────────────────────────────

fn model_dir_from_env() -> PathBuf {
    let dir = std::env::var(MODEL_DIR_ENV).unwrap_or_else(|_| {
        panic!("set {MODEL_DIR_ENV} to a sherpa-onnx KWS model directory to run this test")
    });
    let dir = PathBuf::from(dir);
    assert!(dir.is_dir(), "{MODEL_DIR_ENV} is not a directory: {dir:?}");
    dir
}

/// Feed a WAV through the production spotting loop and collect what fired.
fn detections_for(model_dir: &Path, phrases: &[&str], wav: &Path) -> Vec<String> {
    let tokenizer = WakeWordTokenizer::load(model_dir).expect("load tokenizer");
    let owned: Vec<String> = phrases.iter().map(|p| (*p).to_string()).collect();
    let keywords_buf = tokenizer
        .keywords_buf(&owned)
        .unwrap_or_else(|(phrase, error)| panic!("{phrase}: {error}"));

    let spotter = create_keyword_spotter(model_dir, &keywords_buf).expect("create spotter");
    let wave = sherpa_onnx::Wave::read(&wav.to_string_lossy()).expect("read wav fixture");
    assert_eq!(wave.sample_rate(), 16_000, "fixture must be 16 kHz");

    let stream = spotter.create_stream();
    let mut fired = Vec::new();
    // Chunk exactly as the worker does, so the test exercises the streaming
    // path rather than a single bulk accept_waveform.
    for chunk in wave.samples().chunks(VAD_FRAME_SAMPLES) {
        stream.accept_waveform(16_000, chunk);
        fired.extend(drain_detections(&spotter, &stream));
    }
    fired
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn a_user_typed_wake_word_fires_on_the_matching_fixture() {
    let model_dir = model_dir_from_env();
    let wav = model_dir.join("test_wavs/1.wav");

    // "lovely child" is spoken in 1.wav and not in 0.wav. It is typed here in
    // lower case exactly as a user would type it — normalisation, Viterbi
    // segmentation and vocabulary validation all run before the engine.
    let fired = detections_for(&model_dir, &["lovely child"], &wav);
    assert!(
        fired.iter().any(|k| k.eq_ignore_ascii_case("LOVELY CHILD")),
        "expected LOVELY CHILD in {fired:?}"
    );
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn a_wake_word_that_is_not_spoken_does_not_fire() {
    let model_dir = model_dir_from_env();
    // The same keyword against the other fixture: the control that makes the
    // positive test meaningful.
    let fired = detections_for(
        &model_dir,
        &["lovely child"],
        &model_dir.join("test_wavs/0.wav"),
    );
    assert!(fired.is_empty(), "unexpected detections: {fired:?}");
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn no_wake_words_arms_nothing() {
    // M0 finding 5: runtime keywords MERGE with the configured set, so "no
    // wake words" has to be representable. A single newline is the proven-safe
    // payload — this asserts it arms nothing rather than everything.
    let model_dir = model_dir_from_env();
    let spotter = create_keyword_spotter(&model_dir, "\n").expect("create spotter");
    let wave = sherpa_onnx::Wave::read(&model_dir.join("test_wavs/1.wav").to_string_lossy())
        .expect("read wav fixture");
    let stream = spotter.create_stream();
    let mut fired = Vec::new();
    for chunk in wave.samples().chunks(VAD_FRAME_SAMPLES) {
        stream.accept_waveform(16_000, chunk);
        fired.extend(drain_detections(&spotter, &stream));
    }
    assert!(fired.is_empty(), "an empty keyword set fired: {fired:?}");
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn forty_eight_kilohertz_worklet_audio_reaches_the_engine_intact() {
    // The worklet pushes 48 kHz. Upsample the 16 kHz fixture to 48 kHz, push it
    // through the production Resampler, and require the same detection — this
    // is the only check that the capture-rate path does not silently destroy
    // the wake word.
    let model_dir = model_dir_from_env();
    let wave = sherpa_onnx::Wave::read(&model_dir.join("test_wavs/1.wav").to_string_lossy())
        .expect("read wav fixture");
    let upsampled: Vec<f32> = wave
        .samples()
        .iter()
        .flat_map(|s| std::iter::repeat_n(*s, 3))
        .collect();

    let tokenizer = WakeWordTokenizer::load(&model_dir).expect("load tokenizer");
    let keywords_buf = tokenizer
        .keywords_buf(&["lovely child".to_string()])
        .expect("keywords buf");
    let spotter = create_keyword_spotter(&model_dir, &keywords_buf).expect("create spotter");
    let stream = spotter.create_stream();

    let mut resampler = Resampler::new(48_000).expect("resampler");
    let mut fired = Vec::new();
    for batch in upsampled.chunks(4_800) {
        for chunk in resampler.push(batch) {
            stream.accept_waveform(16_000, &chunk);
            fired.extend(drain_detections(&spotter, &stream));
        }
    }
    assert!(
        fired.iter().any(|k| k.eq_ignore_ascii_case("LOVELY CHILD")),
        "48 kHz path lost the wake word: {fired:?}"
    );
}
