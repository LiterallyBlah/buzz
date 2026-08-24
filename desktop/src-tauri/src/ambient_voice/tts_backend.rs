//! Which pipeline speaks the agent's replies.
//!
//! One session has exactly one of these, built at start from the user's TTS
//! choice: the local Pocket pipeline, or a speech server. Both are addressed
//! through [`AmbientTts`], so `ambient_speak` and `stop_session` do not care
//! which is running — and both share the `tts_active` / `tts_cancel` flags the
//! audio worker gates capture and barge-in on.
//!
//! Split out of `mod.rs` rather than added to it: choosing between two
//! pipelines is a job of its own, and the lifecycle file is already the
//! longest in the feature.

use std::sync::{atomic::Ordering, Arc};

use crate::app_state::AppState;
use crate::huddle::{tts::TtsPipeline, tts_settings};

use super::http_tts::HttpTtsPipeline;
use super::models;
use super::settings::AmbientVoiceSettings;
use super::speech_health::RoleHealth;
use super::speech_http::SpeechEndpoint;
use super::speech_text::flatten_markdown_for_speech;

/// The speech pipeline a running session holds.
///
/// `Clone` because `ambient_speak` takes a handle out of the runtime lock
/// before it queues anything: speaking must never hold the lock the audio
/// worker's status reports need. Both variants are an `Arc`, so a clone is a
/// refcount.
#[derive(Debug, Clone)]
pub enum AmbientTts {
    /// The on-device voice, synthesised by `huddle::tts`.
    Local(Arc<TtsPipeline>),
    /// A speech server.
    Http(Arc<HttpTtsPipeline>),
}

impl AmbientTts {
    /// Queue a reply to be spoken.
    ///
    /// This is where the reply stops being Markdown. Agent replies are written
    /// as Markdown and the message pane renders them as Markdown, but a voice
    /// reads `**ready**` as "star star ready star star" and a fenced block as
    /// its backticks — so the text is flattened here, at the one door both
    /// pipelines go through, and nowhere on the path that publishes or shows
    /// it. Doing it in each pipeline instead would leave the next backend to
    /// remember; doing it in the caller would leave the flattening one refactor
    /// away from the thing it protects.
    pub fn speak(&self, text: String) -> Result<(), String> {
        let text = flatten_markdown_for_speech(&text);
        if text.is_empty() {
            // A reply that was nothing but marks — a bare rule, an empty code
            // fence. There is nothing to say, and queueing an empty string
            // would make a server synthesise silence for it.
            return Ok(());
        }
        match self {
            Self::Local(pipeline) => pipeline.speak(text),
            Self::Http(pipeline) => pipeline.speak(text),
        }
    }

    /// Tell the pipeline's worker to stop. The handle is dropped separately,
    /// outside every lock, because dropping joins the thread.
    pub fn shutdown(&self) {
        match self {
            Self::Local(pipeline) => pipeline.shutdown(),
            Self::Http(pipeline) => pipeline.shutdown(),
        }
    }
}

/// Build the ambient TTS pipeline.
///
/// Deliberately its own pipeline rather than the huddle's: the huddle's is
/// gated on `HuddlePhase::Active` and is torn down with the huddle, and the
/// two must never contend for the output device.
///
/// **Never fatal.** A missing local model, an unreachable server, an output
/// device that will not open — all of them return `Ok(None)` and leave the
/// session running without speech. The transcript path is what carries the
/// conversation; the replies simply are not read aloud, which the settings
/// section already explains.
pub(super) async fn start_ambient_tts(
    state: &AppState,
    settings: &AmbientVoiceSettings,
) -> Result<Option<AmbientTts>, String> {
    match settings.tts.http_base_url() {
        Some(url) => Ok(start_http_tts(state, settings, url)),
        None => start_local_tts(state, settings).await,
    }
}

/// Build a pipeline that speaks through a server.
///
/// Synchronous, unlike the local one: there is no model to load, and the only
/// blocking step is opening the audio device, which the worker thread does
/// itself before this returns.
fn start_http_tts(
    state: &AppState,
    settings: &AmbientVoiceSettings,
    url: &str,
) -> Option<AmbientTts> {
    let ambient = &state.ambient_voice;
    ambient.tts_cancel.store(false, Ordering::Release);
    build_http_tts(
        url,
        settings.output_device.clone(),
        Arc::clone(&ambient.tts_active),
        Arc::clone(&ambient.tts_cancel),
        Arc::clone(&ambient.speech_health.tts),
    )
}

/// The pipeline itself, without the runtime it is being built for.
///
/// Split out so the failure below can be exercised: an address that does not
/// parse never reaches the audio device, so this whole path runs in a test
/// without a sound card.
pub(super) fn build_http_tts(
    url: &str,
    output_device: Option<String>,
    tts_active: Arc<std::sync::atomic::AtomicBool>,
    tts_cancel: Arc<std::sync::atomic::AtomicBool>,
    health: Arc<RoleHealth>,
) -> Option<AmbientTts> {
    let built = SpeechEndpoint::parse(url).and_then(|endpoint| {
        HttpTtsPipeline::new(
            endpoint,
            output_device,
            tts_active,
            tts_cancel,
            Arc::clone(&health),
        )
    });
    match built {
        Ok(pipeline) => Some(AmbientTts::Http(Arc::new(pipeline))),
        Err(error) => {
            eprintln!("buzz-desktop: ambient speech server unavailable: {error}");
            // A pipeline that was never built asks the server nothing, so
            // `fetch_speech` never records anything and the role would report
            // itself healthy for the whole session — while every reply went
            // unspoken with only a line on stderr to say why.
            health.failed(&error);
            None
        }
    }
}

async fn start_local_tts(
    state: &AppState,
    settings: &AmbientVoiceSettings,
) -> Result<Option<AmbientTts>, String> {
    let Some(model_dir) = models::tts_model_dir() else {
        eprintln!("buzz-desktop: ambient voice started without TTS (model not ready)");
        return Ok(None);
    };
    let app = super::app_handle(state);
    let preferences = state
        .huddle_audio
        .tts
        .lock()
        .map_err(|error| format!("text-to-speech settings lock poisoned: {error}"))
        .map(|tts| tts.voice_preferences.clone())?;
    let voice = match app.as_ref() {
        Some(app) => tts_settings::pocket_voice_reference(app, &preferences)?,
        None => tts_settings::bundled_pocket_voice_reference(&preferences),
    };

    let ambient = &state.ambient_voice;
    let tts_active = Arc::clone(&ambient.tts_active);
    let tts_cancel = Arc::clone(&ambient.tts_cancel);
    tts_cancel.store(false, Ordering::Release);
    let output_device = settings.output_device.clone();

    // Construction loads ONNX sessions (~200 ms) — off the async runtime.
    let built = tokio::task::spawn_blocking(move || {
        TtsPipeline::new_with_voice(
            model_dir,
            tts_active,
            tts_cancel,
            // Ambient voice owns barge-in through `tts_cancel`; it is not part
            // of a Huddle's shared human-floor arbitration. Keep an unbound
            // floor so the common local TTS pipeline authorises its queue.
            Default::default(),
            &voice,
            output_device,
            app,
        )
    })
    .await
    .map_err(|error| format!("ambient TTS startup panicked: {error}"))?;

    match built {
        Ok(pipeline) => Ok(Some(AmbientTts::Local(Arc::new(pipeline)))),
        Err(error) => {
            eprintln!("buzz-desktop: ambient TTS unavailable: {error}");
            Ok(None)
        }
    }
}

#[cfg(test)]
#[path = "tts_backend_tests.rs"]
mod tts_backend_tests;
