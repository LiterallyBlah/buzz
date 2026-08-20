//! PCM16 WAV coding for the HTTP speech backends.
//!
//! Both directions of the wire carry a WAV: an utterance is uploaded as one
//! (`multipart` `file`), and a spoken reply comes back as one. Written here
//! rather than pulled from a crate because it is a header and a cast in each
//! direction, and because a hand-written pair is exactly testable — every
//! branch below is a real answer some server could give us.
//!
//! Nothing here allocates from a length field it has not checked: a truncated
//! or hostile response must be an error a caller can log, never a panic on the
//! audio thread.

/// One decoded WAV, ready for the audio device.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecodedAudio {
    /// Interleaved samples in `[-1.0, 1.0]`.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Uncompressed PCM, the only `fmt ` encoding either direction uses.
const WAVE_FORMAT_PCM: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;
const HEADER_BYTES: usize = 44;

/// Encode mono `samples` as a PCM16 WAV.
///
/// Samples are clamped before the cast: the utterance buffer comes from a
/// microphone through a resampler, and a value outside full scale would wrap
/// to the opposite polarity as an audible click in whatever the server
/// transcribes.
pub(crate) fn encode_pcm16_mono(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_bytes = samples.len() * 2;
    let mut out = Vec::with_capacity(HEADER_BYTES + data_bytes);
    let byte_rate = sample_rate * u32::from(BITS_PER_SAMPLE) / 8;

    out.extend_from_slice(b"RIFF");
    // Everything after this field. Saturating so a buffer larger than a u32
    // could describe writes a wrong length rather than panicking; nothing in
    // this app produces one (an utterance is seconds long, capped upstream).
    out.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for sample in samples {
        let scaled = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        out.extend_from_slice(&scaled.to_le_bytes());
    }
    out
}

/// Decode a PCM16 WAV.
///
/// Chunk-walking rather than a fixed 44-byte header: encoders in the wild put
/// `LIST`/`fact` chunks before `data`, and assuming the canonical layout would
/// play those bytes as noise.
pub(crate) fn decode_pcm16(bytes: &[u8]) -> Result<DecodedAudio, String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("speech response is not a WAV file".to_string());
    }
    let mut format: Option<(u16, u32, u16)> = None;
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let body_start = offset + 8;
        let body = bytes
            .get(body_start..body_start.saturating_add(size))
            .ok_or_else(|| format!("WAV chunk runs past the end of the response ({size} bytes)"))?;

        if id == b"fmt " {
            format = Some(read_format(body)?);
        } else if id == b"data" {
            let (channels, sample_rate, _) =
                format.ok_or("WAV data arrived before its format chunk")?;
            return Ok(DecodedAudio {
                samples: body
                    .chunks_exact(2)
                    .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32_768.0)
                    .collect(),
                sample_rate,
                channels,
            });
        }
        // Chunks are word-aligned: an odd size carries a pad byte that is not
        // counted in the size field.
        offset = body_start + size + (size % 2);
    }
    Err("WAV response carried no audio data".to_string())
}

/// `(channels, sample_rate, bits_per_sample)` from a `fmt ` chunk body.
fn read_format(body: &[u8]) -> Result<(u16, u32, u16), String> {
    if body.len() < 16 {
        return Err("WAV format chunk is truncated".to_string());
    }
    let audio_format = u16::from_le_bytes([body[0], body[1]]);
    if audio_format != WAVE_FORMAT_PCM {
        return Err(format!(
            "speech response is WAV format {audio_format}; this build plays uncompressed PCM only"
        ));
    }
    let channels = u16::from_le_bytes([body[2], body[3]]);
    if channels == 0 {
        return Err("WAV response claims zero channels".to_string());
    }
    let sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    if sample_rate == 0 {
        return Err("WAV response claims a zero sample rate".to_string());
    }
    let bits = u16::from_le_bytes([body[14], body[15]]);
    if bits != BITS_PER_SAMPLE {
        return Err(format!(
            "speech response is {bits}-bit WAV; this build plays 16-bit only"
        ));
    }
    Ok((channels, sample_rate, bits))
}

#[cfg(test)]
#[path = "speech_wav_tests.rs"]
mod speech_wav_tests;
