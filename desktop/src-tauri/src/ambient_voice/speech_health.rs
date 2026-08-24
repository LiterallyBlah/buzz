//! Whether the speech servers the user configured are actually answering.
//!
//! Both server-backed roles fail softly on purpose, and that is the right
//! behaviour: a failed transcription falls back to the on-device recogniser so
//! the sentence still reaches the agent, and a failed synthesis costs the
//! spoken half of one reply rather than the session. What was wrong is that it
//! failed *silently* — the pill went on saying "Listening for the wake word",
//! the settings section went on showing the address as though it were in use,
//! and the only evidence a server was down at all was a line on stderr nobody
//! reads. Someone whose server had stopped could not tell that from a wake word
//! that was not firing.
//!
//! So this records what each role's server last did, and the status report
//! carries it. **The fallback is untouched**: nothing here decides anything,
//! it only makes the decision that was already being made visible.
//!
//! ## Shape
//!
//! One counter and one message per role, written from the worker threads
//! (`transcriber` on the audio thread, `http_tts` on the speech thread) and
//! read by `build_report` on a command thread. A success clears both, so this
//! answers "is it failing now", not "has it ever failed" — a server that came
//! back must stop being complained about, and the next reply or utterance is
//! what proves it did.
//!
//! `configured` is set when a session starts and cleared when one stops, so a
//! role that runs on this computer never reports a server problem and a stale
//! failure cannot outlive the session it happened in.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, Mutex,
};

use serde::{Deserialize, Serialize};

/// How much of a failure's own words are kept. The settings section shows this
/// on one line under the status.
const MAX_ERROR_CHARS: usize = 160;

/// The health of both speech roles, shared by the session's workers.
#[derive(Debug, Default)]
pub struct SpeechHealth {
    /// Speech to text — written by the audio worker's transcriber.
    pub stt: Arc<RoleHealth>,
    /// Text to speech — written by the reply-speaking worker.
    pub tts: Arc<RoleHealth>,
}

/// One role's server, as of its last attempt.
#[derive(Debug, Default)]
pub struct RoleHealth {
    /// A server is configured for this role, and a session is running.
    configured: AtomicBool,
    /// Attempts since the last success. Zero while the server is answering.
    failures: AtomicU32,
    /// The last failure's own words, clipped. Cleared by a success.
    last_error: Mutex<Option<String>>,
}

impl RoleHealth {
    /// This role's server answered.
    pub fn succeeded(&self) {
        // Ordered so a reader between the two sees "no failures" with a stale
        // message rather than "failing" with none: the count is what `failing`
        // is derived from, and a message with nothing to explain is harmless.
        self.failures.store(0, Ordering::Release);
        self.set_error(None);
    }

    /// This role's server did not, and this is what it said.
    pub fn failed(&self, error: &str) {
        self.set_error(Some(clip(error)));
        self.failures.fetch_add(1, Ordering::Release);
    }

    /// Record `outcome` in one call, for a caller that has a `Result` in hand.
    pub fn record<T, E: std::fmt::Display>(&self, outcome: &Result<T, E>) {
        match outcome {
            Ok(_) => self.succeeded(),
            Err(error) => self.failed(&error.to_string()),
        }
    }

    /// A session started or stopped with this role pointed at a server.
    ///
    /// Always clears: a session that is starting has not failed yet, and one
    /// that has stopped has nothing left to fail. A failure from a previous
    /// session shown against a new one would be a second kind of lie.
    pub(super) fn configure(&self, on_a_server: bool) {
        self.configured.store(on_a_server, Ordering::Release);
        self.failures.store(0, Ordering::Release);
        self.set_error(None);
    }

    fn set_error(&self, error: Option<String>) {
        if let Ok(mut held) = self.last_error.lock() {
            *held = error;
        }
    }

    /// This role's snapshot on its own, for tests that hold one handle rather
    /// than the pair — the same value `report()` puts on the status report.
    #[cfg(test)]
    pub(super) fn snapshot_for_test(&self) -> SpeechRoleHealth {
        self.snapshot()
    }

    fn snapshot(&self) -> SpeechRoleHealth {
        let configured = self.configured.load(Ordering::Acquire);
        let failures = self.failures.load(Ordering::Acquire);
        SpeechRoleHealth {
            configured,
            // Derived here rather than in the frontend so one rule has one
            // home: a role running on this computer has no server to fail.
            failing: configured && failures > 0,
            consecutive_failures: failures,
            last_error: self
                .last_error
                .lock()
                .ok()
                .and_then(|error| error.clone())
                .filter(|_| configured),
        }
    }
}

impl SpeechHealth {
    /// Record which roles a starting session put on a server, clearing both.
    pub fn configure(&self, stt_on_a_server: bool, tts_on_a_server: bool) {
        self.stt.configure(stt_on_a_server);
        self.tts.configure(tts_on_a_server);
    }

    pub fn report(&self) -> SpeechBackendHealthReport {
        SpeechBackendHealthReport {
            stt: self.stt.snapshot(),
            tts: self.tts.snapshot(),
        }
    }
}

/// Both roles' servers, as carried on the status report.
///
/// Pinned from the producing side by
/// `the_speech_backend_health_serialises_in_the_shape_the_frontend_parses` in
/// `mod_tests.rs`; that test, `AmbientSpeechHealth` in `ambientVoiceApi.ts` and
/// the fixtures in `ambientVoiceTestDom.mjs` change together.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechBackendHealthReport {
    pub stt: SpeechRoleHealth,
    pub tts: SpeechRoleHealth,
}

/// One role's server, as carried on the status report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechRoleHealth {
    /// This role is pointed at a server, and a session is running.
    pub configured: bool,
    /// That server's last attempt failed and it has not answered since.
    pub failing: bool,
    /// Attempts since the last success — how long it has been failing, in the
    /// only unit this path has.
    pub consecutive_failures: u32,
    /// The last failure's own words. `None` when there is nothing to explain.
    pub last_error: Option<String>,
}

fn clip(error: &str) -> String {
    let trimmed = error.trim();
    if trimmed.chars().count() <= MAX_ERROR_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(MAX_ERROR_CHARS).collect()
}

#[cfg(test)]
#[path = "speech_health_tests.rs"]
mod speech_health_tests;
