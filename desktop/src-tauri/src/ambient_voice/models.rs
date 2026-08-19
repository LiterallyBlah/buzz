//! Wake-word model access, as a facade over the shared download manager.
//!
//! The download machinery, integrity hashes and licence sidecar live with the
//! other models in `huddle::models`; this module is the single seam the
//! ambient feature reads them through, so the rest of `ambient_voice` never
//! reaches into the huddle module directly.

use std::path::PathBuf;

pub(crate) use crate::huddle::models::kws::{
    KWS_DECODER, KWS_ENCODER, KWS_JOINER, KWS_REQUIRED_FILES, KWS_TOKENS,
};
use crate::huddle::models::{global_model_manager, ModelStatus};

/// Where the wake-word model lives once installed, or `None` if it is not
/// ready yet.
pub fn kws_model_dir() -> Option<PathBuf> {
    crate::huddle::models::kws::kws_model_dir()
}

/// `true` once every wake-word file is installed.
pub fn is_kws_ready() -> bool {
    crate::huddle::models::kws::is_kws_ready()
}

/// One-shot edge: `true` exactly once after the wake-word download completes.
pub fn take_kws_ready() -> bool {
    global_model_manager()
        .map(|manager| manager.take_kws_ready())
        .unwrap_or(false)
}

/// Where the speech-to-text model lives. Shared with huddles: one Parakeet
/// install serves both, and the ambient feature never triggers its download
/// (it is already fetched at launch).
pub fn stt_model_dir() -> Option<PathBuf> {
    crate::huddle::models::stt_model_dir()
}

/// Where the text-to-speech model lives. Shared with huddles.
pub fn tts_model_dir() -> Option<PathBuf> {
    crate::huddle::models::tts_model_dir()
}

/// Start the wake-word download if it is not already installed.
///
/// Called when the feature is switched on and at boot when the persisted
/// settings say the session should run — never unconditionally, because it is
/// ~18 MB of preview-only capability.
pub fn ensure_kws_download(http_client: reqwest::Client) {
    if let Some(manager) = global_model_manager() {
        manager.start_kws_download(http_client);
    }
}

/// Download status for every model the ambient session needs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbientModelStatus {
    /// Wake-word spotter — downloaded on demand for this feature.
    pub kws: ModelStatus,
    /// Speech-to-text — shared with huddles.
    pub stt: ModelStatus,
    /// Text-to-speech — shared with huddles.
    pub tts: ModelStatus,
}

pub fn ambient_model_status() -> Result<AmbientModelStatus, String> {
    let manager = global_model_manager()
        .ok_or("model manager unavailable (home directory could not be resolved)")?;
    Ok(AmbientModelStatus {
        kws: manager.kws_status(),
        stt: manager.stt_status(),
        tts: manager.tts_status(),
    })
}
