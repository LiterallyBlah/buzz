//! Ambient session status, as shown by the in-app listening indicator.
//!
//! The indicator is a privacy requirement, not decoration: while the feature
//! is enabled the operating-system microphone indicator is permanently lit, so
//! the app must be able to say at a glance what it is doing with the audio.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum AmbientStatus {
    /// Not running: the flag or the setting is off.
    Off,
    /// Configured, but a huddle owns the microphone.
    Suspended,
    /// Running with the microphone deliberately closed by the user.
    Muted,
    /// Models loading / worker starting.
    Starting,
    /// Armed and listening for the wake word. Nothing is being transcribed.
    Listening,
    /// The wake word fired; waiting for the user to speak.
    Heard,
    /// Capturing an utterance.
    Capturing,
    /// Running speech-to-text on the captured utterance.
    Transcribing,
    /// Reading an agent reply aloud.
    Speaking,
    /// The session could not run. The string is user-facing.
    Error(String),
}

impl AmbientStatus {
    /// Whether audio is currently being processed at all.
    ///
    /// Drives the indicator's "live" affordance; `Muted` and `Suspended` are
    /// deliberately not live even though the session object still exists.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Listening | Self::Heard | Self::Capturing | Self::Transcribing | Self::Speaking
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_as_a_tagged_state_for_the_frontend() {
        assert_eq!(
            serde_json::to_value(AmbientStatus::Listening).expect("json"),
            serde_json::json!({ "state": "listening" })
        );
        assert_eq!(
            serde_json::to_value(AmbientStatus::Error("no model".to_string())).expect("json"),
            serde_json::json!({ "state": "error", "detail": "no model" })
        );
    }

    #[test]
    fn only_processing_states_are_live() {
        for status in [
            AmbientStatus::Off,
            AmbientStatus::Suspended,
            AmbientStatus::Muted,
            AmbientStatus::Starting,
            AmbientStatus::Error("x".to_string()),
        ] {
            assert!(!status.is_live(), "{status:?}");
        }
        for status in [
            AmbientStatus::Listening,
            AmbientStatus::Heard,
            AmbientStatus::Capturing,
            AmbientStatus::Transcribing,
            AmbientStatus::Speaking,
        ] {
            assert!(status.is_live(), "{status:?}");
        }
    }
}
