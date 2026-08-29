//! Where a finished utterance becomes text.
//!
//! This is the only swappable step in the audio path. The wake word, the voice
//! activity detector and the utterance machine always run on this computer:
//! they decide *whether* there is anything to send, and a backend that saw the
//! microphone continuously would be a different feature with a different
//! consent conversation. What can be sent away is one finished utterance —
//! the same audio the user has already decided to say to an agent.
//!
//! Built once per session, on the worker thread, because both halves are
//! thread-bound: the sherpa recogniser is not `Send`-safe across await points
//! and the HTTP client is a blocking one.

use std::{path::Path, sync::Arc};

use super::speech_health::RoleHealth;
use super::speech_http::{self, SpeechEndpoint};

/// Rate of the worker's utterance buffer, after resampling.
pub(crate) const UTTERANCE_SAMPLE_RATE: u32 = 16_000;

/// The transcriber one session runs with.
pub(crate) enum Transcriber {
    /// The on-device recogniser.
    Local(sherpa_onnx::OfflineRecognizer),
    /// A speech server, with the on-device recogniser kept as the
    /// per-utterance fallback whenever its model is installed.
    Http {
        client: reqwest::blocking::Client,
        endpoint: SpeechEndpoint,
        local: Option<sherpa_onnx::OfflineRecognizer>,
        /// Where each attempt against the server is recorded, so a server that
        /// is failing softly is visible on the indicator instead of only on
        /// stderr. It records; it decides nothing.
        health: Arc<RoleHealth>,
    },
}

impl Transcriber {
    /// Build the transcriber for a session.
    ///
    /// The local recogniser is loaded even when a server is configured — that
    /// is what makes the fallback per utterance rather than per session — and
    /// its absence is only fatal when there is no server to fall back *from*.
    ///
    /// A server the settings name but that cannot be used at all (an address
    /// that does not parse, a client that will not build) degrades to the
    /// local recogniser rather than killing the session: the wake word, the
    /// microphone and the transcript path all still work, and the alternative
    /// is a feature that goes silent because of a typo in a URL.
    pub(crate) fn build(
        stt_model_dir: &Path,
        endpoint_url: Option<&str>,
        health: Arc<RoleHealth>,
    ) -> Result<Self, String> {
        let local = create_recognizer(stt_model_dir);
        let Some(raw) = endpoint_url else {
            return local.map(Transcriber::Local);
        };
        match SpeechEndpoint::parse(raw)
            .and_then(|endpoint| speech_http::blocking_client().map(|client| (endpoint, client)))
        {
            Ok((endpoint, client)) => {
                let local = local
                    .inspect_err(|error| {
                        eprintln!(
                            "buzz-desktop: ambient speech runs on the server with no on-device fallback: {error}"
                        );
                    })
                    .ok();
                Ok(Transcriber::Http {
                    client,
                    endpoint,
                    local,
                    health,
                })
            }
            Err(error) => {
                // The one chance to record this. A server that cannot be
                // addressed at all is never asked anything, so the per-request
                // recording below never runs and the report would say
                // "configured, not failing" for the whole session — the pill
                // claiming all is well while every utterance is quietly decoded
                // on this computer, which is the state this was built to end.
                health.failed(&error);
                match local {
                    Ok(recognizer) => {
                        eprintln!(
                            "buzz-desktop: ambient speech server unusable ({error}); using the on-device recogniser"
                        );
                        Ok(Transcriber::Local(recognizer))
                    }
                    Err(local_error) => Err(format!(
                        "{error} — and the on-device speech model is unavailable: {local_error}"
                    )),
                }
            }
        }
    }

    /// Transcribe one utterance.
    ///
    /// An `Ok("")` means the audio carried no words, which is an ordinary
    /// outcome and not a failure. An `Err` is a failure the user has to be
    /// told about: it means nothing at all was heard from this utterance.
    pub(crate) fn transcribe(&self, samples: &[f32]) -> Result<String, String> {
        match self {
            Self::Local(recognizer) => Ok(decode_locally(recognizer, samples)),
            Self::Http {
                client,
                endpoint,
                local,
                health,
            } => {
                let answered =
                    speech_http::transcribe(client, endpoint, samples, UTTERANCE_SAMPLE_RATE);
                // Recorded before the fallback runs, so what is reported is
                // what the *server* did rather than what the session managed
                // to do in spite of it.
                health.record(&answered);
                match answered {
                    Ok(text) => Ok(text),
                    Err(error) => {
                        eprintln!("buzz-desktop: ambient speech server failed: {error}");
                        // Per utterance, and deliberately without switching
                        // the session's backend: the user chose a server, and
                        // a client that quietly stopped using it would leave
                        // them unable to tell a working server from a broken
                        // one. The health line is what makes that tellable.
                        Ok(decode_locally(
                            fallback_after(local.as_ref(), error)?,
                            samples,
                        ))
                    }
                }
            }
        }
    }
}

/// Which recogniser answers after a server failure, if any.
///
/// Generic over the recogniser so both answers are testable without the
/// on-device model installed. What is pinned is the rule, and the rule is the
/// part that has to stay true: fall back for this utterance when a local
/// recogniser exists, and surface the server's own words when it does not.
fn fallback_after<T>(local: Option<T>, server_error: String) -> Result<T, String> {
    local.ok_or_else(|| {
        format!(
            "Speech server failed and this computer has no speech model installed: {server_error}"
        )
    })
}

fn decode_locally(recognizer: &sherpa_onnx::OfflineRecognizer, samples: &[f32]) -> String {
    let stream = recognizer.create_stream();
    stream.accept_waveform(UTTERANCE_SAMPLE_RATE as i32, samples);
    recognizer.decode(&stream);
    stream
        .get_result()
        .map(|result| result.text.trim().to_string())
        .unwrap_or_default()
}

fn create_recognizer(model_dir: &Path) -> Result<sherpa_onnx::OfflineRecognizer, String> {
    use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};

    let tokens_path = model_dir.join("tokens.txt");
    let model_path = model_dir.join("model.int8.onnx");
    if !tokens_path.is_file() || !model_path.is_file() {
        return Err(format!(
            "speech-to-text model not found at {}",
            model_dir.display()
        ));
    }
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.nemo_ctc.model = Some(model_path.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens_path.to_string_lossy().into_owned());
    config.model_config.num_threads = super::session::NUM_THREADS;
    config.model_config.debug = false;
    OfflineRecognizer::create(&config)
        .ok_or_else(|| "could not create the speech recognizer".to_string())
}

#[cfg(test)]
#[path = "transcriber_tests.rs"]
mod transcriber_tests;
