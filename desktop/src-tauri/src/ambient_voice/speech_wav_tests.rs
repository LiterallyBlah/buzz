//! WAV coding tests.
//!
//! Both directions are exercised against bytes rather than against each other
//! alone: an encoder and a decoder that agree on a wrong header would pass a
//! round trip and fail against every real server, so the header is asserted
//! field by field and the decoder is fed shapes a server can actually send.

use super::*;

/// The 16 kHz utterance rate the worker resamples to.
const UTTERANCE_RATE: u32 = 16_000;

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[test]
fn the_uploaded_header_is_the_one_a_server_expects() {
    // Every field a receiving decoder reads. A wrong byte rate or block align
    // is the classic silent fault: the file opens, and the transcription is of
    // audio at the wrong speed.
    let wav = encode_pcm16_mono(&[0.0; 8], UTTERANCE_RATE);
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(&wav[12..16], b"fmt ");
    assert_eq!(u32_at(&wav, 16), 16, "fmt chunk size");
    assert_eq!(u16_at(&wav, 20), 1, "PCM");
    assert_eq!(u16_at(&wav, 22), 1, "mono");
    assert_eq!(u32_at(&wav, 24), UTTERANCE_RATE);
    assert_eq!(u32_at(&wav, 28), UTTERANCE_RATE * 2, "byte rate");
    assert_eq!(u16_at(&wav, 32), 2, "block align");
    assert_eq!(u16_at(&wav, 34), 16, "bits per sample");
    assert_eq!(&wav[36..40], b"data");
    assert_eq!(u32_at(&wav, 40), 16, "8 samples of 16-bit audio");
    // RIFF size describes everything after the field itself.
    assert_eq!(u32_at(&wav, 4) as usize, wav.len() - 8);
}

#[test]
fn samples_survive_the_round_trip_and_full_scale_does_not_wrap() {
    let samples = [0.0, 0.5, -0.5, 1.0, -1.0];
    let decoded = decode_pcm16(&encode_pcm16_mono(&samples, UTTERANCE_RATE)).expect("decode");
    assert_eq!(decoded.sample_rate, UTTERANCE_RATE);
    assert_eq!(decoded.channels, 1);
    for (sent, back) in samples.iter().zip(decoded.samples.iter()) {
        assert!((sent - back).abs() < 1e-3, "{sent} came back as {back}");
    }
    // Out-of-range input is clamped, not wrapped: a +1.2 that wrapped to
    // negative full scale is an audible click the server would transcribe.
    let clipped = decode_pcm16(&encode_pcm16_mono(&[1.5, -1.5], UTTERANCE_RATE)).expect("decode");
    assert!(clipped.samples[0] > 0.99, "{:?}", clipped.samples);
    assert!(clipped.samples[1] < -0.99, "{:?}", clipped.samples);
}

#[test]
fn a_reply_with_chunks_before_the_audio_still_decodes() {
    // Encoders put `LIST` or `fact` before `data`. Assuming the canonical
    // 44-byte header would play those bytes as noise.
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&0u32.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&24_000u32.to_le_bytes());
    wav.extend_from_slice(&48_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    // An odd-sized chunk, so the pad byte is exercised too.
    wav.extend_from_slice(b"LIST");
    wav.extend_from_slice(&3u32.to_le_bytes());
    wav.extend_from_slice(b"abc\0");
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&4u32.to_le_bytes());
    wav.extend_from_slice(&16_384i16.to_le_bytes());
    wav.extend_from_slice(&(-16_384i16).to_le_bytes());

    let decoded = decode_pcm16(&wav).expect("decode");
    assert_eq!(decoded.sample_rate, 24_000);
    assert_eq!(decoded.samples.len(), 2);
    assert!((decoded.samples[0] - 0.5).abs() < 1e-3, "{decoded:?}");
}

#[test]
fn a_reply_this_build_cannot_play_is_an_error_rather_than_noise() {
    // Each of these is a real answer some server gives. None of them may reach
    // the audio device as samples, and none may panic the TTS thread.
    let truncated = encode_pcm16_mono(&[0.1; 4], UTTERANCE_RATE);
    for (bytes, expected) in [
        (b"not a wav at all".to_vec(), "not a WAV"),
        // A response cut short mid-header: the format chunk claims more bytes
        // than arrived, and the range check is what refuses it.
        (truncated[..30].to_vec(), "past the end"),
        (Vec::new(), "not a WAV"),
    ] {
        let error = decode_pcm16(&bytes).expect_err("must not decode");
        assert!(error.contains(expected), "{error}");
    }

    // A lying `data` size must not index past the buffer.
    let mut overlong = encode_pcm16_mono(&[0.1; 4], UTTERANCE_RATE);
    let len = overlong.len();
    overlong[40..44].copy_from_slice(&(u32::MAX / 2).to_le_bytes());
    assert_eq!(overlong.len(), len);
    let error = decode_pcm16(&overlong).expect_err("must not decode");
    assert!(error.contains("past the end"), "{error}");

    // Formats the player has no code for are named, not guessed at.
    let mut compressed = encode_pcm16_mono(&[0.1; 4], UTTERANCE_RATE);
    compressed[20..22].copy_from_slice(&3u16.to_le_bytes());
    assert!(decode_pcm16(&compressed)
        .expect_err("float PCM")
        .contains("uncompressed PCM only"));

    let mut deep = encode_pcm16_mono(&[0.1; 4], UTTERANCE_RATE);
    deep[34..36].copy_from_slice(&24u16.to_le_bytes());
    assert!(decode_pcm16(&deep)
        .expect_err("24-bit")
        .contains("16-bit only"));
}

#[test]
fn a_wav_with_a_header_and_no_audio_says_so() {
    // A server that answers 200 with an empty body is a failure to speak, not
    // silence to play.
    let header_only = encode_pcm16_mono(&[], UTTERANCE_RATE);
    let decoded = decode_pcm16(&header_only).expect("header-only wav decodes");
    assert!(decoded.samples.is_empty());

    let mut no_data = header_only.clone();
    no_data.truncate(36);
    assert!(decode_pcm16(&no_data)
        .expect_err("no data chunk")
        .contains("no audio data"));
}
