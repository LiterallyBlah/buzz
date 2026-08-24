//! The mute fence: a capture's authority to exist, and every hand that checks it.
//!
//! A capture is bound to the mute epoch in force when it started, and that
//! binding is its authority: the close, the transcript queue and the egress
//! boundary all compare it against the live counter and drop what they are
//! holding the moment the two disagree. A mute is therefore a death sentence
//! pronounced at the event rather than a flag read later — the unmute that may
//! follow opens the microphone for the next capture and never revives the last
//! one.
//!
//! The primitives that say that and enforce it live here rather than among the
//! worker's audio plumbing in [`super::session`], so the whole fence can be read
//! — and reviewed — in one file. `session` re-exports them, so every call site
//! outside this module still names them through it.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Mutex,
};

use tokio::sync::mpsc as tokio_mpsc;

use super::rolling::RollingCapture;
use super::session::{AudioFlow, StatusSink};
use super::status::AmbientStatus;
use super::utterance::UtteranceMachine;

/// What the close must know about mute, and it is three things.
///
/// The flag says whether a mute is in force *now*; the counter says whether one
/// has *happened*; and `armed_under` says which counter value this capture was
/// born under. They travel as one because any two of them without the third is
/// a hole: the flag alone is erased by an unmute, and the counter alone is
/// meaningless without the value to compare it against — a close that read the
/// counter for itself would read whatever the mute had already moved it to.
pub(crate) struct MuteSignal<'a> {
    pub(crate) muted: &'a AtomicBool,
    pub(crate) epochs: &'a AtomicU64,
    /// The epoch the capture was armed under ([`CaptureEpoch`]), or `None` when
    /// nothing was armed — which is no authority at all, and publishes nothing.
    pub(crate) armed_under: Option<u64>,
}

/// The mute epoch the capture in progress was armed under.
///
/// The worker reads mute once per audio batch, so a mute and the unmute after
/// it can both complete without the worker ever seeing the flag raised: between
/// two batches, or inside one before the frame that ends the utterance. What
/// survives either ordering is the counter, and what makes it usable is binding
/// it at the moment the capture *starts* rather than at the moment some later
/// stage happens to look. Bound when the machine leaves `Idle` — a wake word,
/// including the one that restarts a capture on barge-in — and released when it
/// returns there, this is the capture's authority to exist, and it is what the
/// close, the queue and the egress boundary all end up comparing against the
/// live counter.
#[derive(Default)]
pub(crate) struct CaptureEpoch(Option<u64>);

impl CaptureEpoch {
    /// A capture starts now: bind it to the epoch in force.
    pub(crate) fn arm(&mut self, mute_epochs: &AtomicU64) {
        self.0 = Some(mute_epochs.load(Ordering::Acquire));
    }

    /// The capture is over, however it ended. Hands its epoch to the close.
    pub(crate) fn disarm(&mut self) -> Option<u64> {
        self.0.take()
    }

    /// Keep the binding in step with the machine.
    ///
    /// A capture exists exactly while the machine is not `Idle`, so that is
    /// exactly how long the epoch stays bound. It is not the same as "clear on
    /// every `Drop`": playback gating drops what was collected and deliberately
    /// holds the arm window open, and that window is still the same capture —
    /// clearing there would leave the utterance the user goes on to speak with
    /// no authority and drop it silently at the close.
    pub(crate) fn clear_if_idle(&mut self, machine: &UtteranceMachine) {
        if machine.phase() == super::utterance::UtterancePhase::Idle {
            self.0 = None;
        }
    }

    /// Whether a mute has happened since this capture was armed.
    ///
    /// The unmute that may have followed does not enter into it: the capture
    /// died at the mute, and this is the worker's own copy of the comparison
    /// every later hand makes ([`transcript_still_wanted`]).
    pub(crate) fn revoked(&self, mute_epochs: &AtomicU64) -> bool {
        self.0
            .is_some_and(|armed_under| !transcript_still_wanted(mute_epochs, armed_under))
    }
}

/// Kill the capture in progress: nothing it collected is transcribed, and the
/// chunks already handed off go with it.
///
/// One function for the two ways a capture is killed between batches — the flag
/// is up, or the epoch it was armed under has moved — so the second cannot
/// drift into being a weaker version of the first.
pub(super) fn abandon_capture(
    machine: &mut UtteranceMachine,
    speech_buf: &mut Vec<f32>,
    rolling: &mut RollingCapture,
    capture_epoch: &mut CaptureEpoch,
) {
    machine.reset();
    speech_buf.clear();
    // Chunks already handed off are part of the same half-captured sentence,
    // and are abandoned with it.
    rolling.abort();
    capture_epoch.disarm();
}

/// One finished utterance on its way to publication, bound to the mute epoch
/// it was captured under.
///
/// The fence in [`finish_capture`] is necessary and not sufficient: after it
/// passes, the transcript crosses a queue and a publisher that awaits other
/// work before the POST, and a mute landing anywhere in that pipeline must
/// still kill it. The epoch is the transcript's authority to exist — every
/// consumer on the way to the wire compares it against the live counter
/// ([`transcript_still_wanted`]) and drops the transcript the moment they
/// disagree.
pub(crate) struct Transcript {
    pub(crate) text: String,
    /// [`super::session::AmbientSessionConfig::mute_epochs`] as of the moment
    /// the capture was armed — the capture this text belongs to died if it
    /// moved.
    pub(crate) mute_epoch: u64,
}

/// Whether a transcript captured under `mute_epoch` may still be published.
///
/// One function so the publisher task, the egress boundary and the regression
/// all run the same comparison. `false` means a mute happened after the
/// capture's close began; the unmute that may have followed opens the
/// microphone for the next capture, never revives this one.
pub(crate) fn transcript_still_wanted(mute_epochs: &AtomicU64, mute_epoch: u64) -> bool {
    mute_epochs.load(Ordering::Acquire) == mute_epoch
}

/// What the indicator shows once an utterance has been decoded.
///
/// A failure stays there until the next transition rather than flashing past.
/// The user has just spoken and heard nothing back, and going straight back to
/// "listening for the wake word" would be the same class of lie the audio
/// watchdog was built to end: a pill claiming to work while the thing it
/// describes is broken. The next wake word replaces it, so nothing has to
/// clear it.
pub(super) fn status_after_decode(outcome: Result<(), String>) -> AmbientStatus {
    match outcome {
        Ok(()) => AmbientStatus::Listening,
        Err(error) => AmbientStatus::Error(error),
    }
}

/// Close the capture, publish what it said, and leave the buffer empty — the
/// tail every close shares.
///
/// The buffer is the utterance's **last** chunk, and it is usually the only
/// one. Anything the ceiling closed earlier is already being transcribed;
/// [`RollingCapture::finish`] waits for all of it and hands back one transcript.
/// The samples are moved out rather than copied: the chunk belongs to the
/// transcription thread from here on.
///
/// `trim` is the stop phrase when one ended this capture, so the phrase the
/// user said to stop talking is not itself sent to the agent.
pub(super) fn finish_capture(
    rolling: &mut RollingCapture,
    speech_buf: &mut Vec<f32>,
    transcript_tx: &tokio_mpsc::Sender<Transcript>,
    status: &StatusSink,
    trim: Option<&str>,
    flow: &AudioFlow,
    mute: MuteSignal<'_>,
) {
    // Bound when the capture started, not read here: a mute is a death sentence
    // for the capture in progress, pronounced the moment the button is pressed,
    // and an unmute must not commute it. A close that latched the epoch for
    // itself would latch whatever a mute during the capture had already moved
    // it to — the flag says nothing by then either, because the unmute put it
    // back — and would publish the capture the user muted.
    let Some(epoch_armed_under) = mute.armed_under else {
        speech_buf.clear();
        rolling.abort();
        return;
    };
    if mute_landed(&mute, epoch_armed_under) {
        // Already dead before the wait began. Abandoned rather than finished:
        // there is nothing to wait for, nobody to hand the words to, and no
        // status to announce — the pill stays exactly where the last mute or
        // unmute wrote it.
        speech_buf.clear();
        rolling.abort();
        return;
    }
    status.set(AmbientStatus::Transcribing);
    // The worker cannot drain its audio queue while this blocks, and against a
    // speech server it blocks for a network round trip. Marked for the length
    // of the wait so the staleness window measures a starved worker rather than
    // a busy one — see [`AudioFlow`]. This is the only wait that is marked: a
    // chunk handed off mid-capture is decoded on the transcription thread while
    // the worker goes on taking audio off its queue.
    let outcome = {
        let _busy = flow.transcribing();
        rolling.finish(std::mem::take(speech_buf), trim)
    };
    // The wait is long — against a slow server, minutes — and the mute button
    // works during it. The worker only reads mute between batches, so this is
    // where a mute that landed mid-close is honoured: the words were muted
    // before they were sent, so they are not sent, whatever the flag says by
    // now, and the pill is left exactly as the last mute or unmute wrote it.
    // Without this fence the transcript would be queued anyway and the close
    // would overwrite the user's state — a mute the app took and talked over.
    if mute_landed(&mute, epoch_armed_under) {
        return;
    }
    let outcome = outcome.map(|text| publish(text, epoch_armed_under, transcript_tx));
    status.set(status_after_decode(outcome));
}

/// Whether mute has taken this capture, by either half of the signal.
///
/// The flag catches a mute still in force; the counter catches one that has
/// been and gone. Asked twice by [`finish_capture`] — once before the wait and
/// once after — because the wait is where the button is most likely pressed and
/// the answer before it says nothing about the answer after.
fn mute_landed(mute: &MuteSignal<'_>, epoch_armed_under: u64) -> bool {
    mute.muted.load(Ordering::Acquire) || !transcript_still_wanted(mute.epochs, epoch_armed_under)
}

/// Send one finished utterance on to the publisher task, stamped with the
/// mute epoch it was captured under.
///
/// The fence above is the last check *this* thread can make, and the queue and
/// the publisher's own awaits lie beyond it; the stamp is what lets every
/// later hand — the queue's consumer, the egress boundary — make their own
/// ([`transcript_still_wanted`]). `None` is an utterance that carried no
/// words, which is an ordinary outcome and not a fault to report — including
/// one which was only the stop phrase, and is therefore empty once trimmed.
fn publish(text: Option<String>, mute_epoch: u64, transcript_tx: &tokio_mpsc::Sender<Transcript>) {
    let Some(text) = text else {
        return;
    };
    if let Err(error) = transcript_tx.blocking_send(Transcript { text, mute_epoch }) {
        eprintln!("buzz-desktop: ambient transcript channel closed: {error}");
    }
}

/// What a mute is, mechanically: the flag the worker honours, the latched
/// epoch, and the pill.
///
/// One function rather than lines in
/// [`super::session::AmbientSession::set_muted`] so the mute-fence regressions
/// drive the exact writes the command path performs — a test that hand-rolled
/// its own version would bless nothing.
///
/// The epoch advances on mute **on** only, and never goes back: it records
/// that a mute happened, where the boolean records whether one is in force.
/// The capture in progress dies at the event, so the event is what the close
/// checks — an unmute restores the flag but must never restore the capture.
///
/// A mute-on makes both of its writes while holding `authority`, the same lock
/// a transcript takes to decide whether it may be dispatched
/// ([`super::publish::DispatchGate`]). That is what turns "the epoch moved
/// first" from a race into an ordering: a mute and a send that happen at the
/// same instant are serialised, and exactly one of them wins. The lock is held
/// for two stores and released — a mute never waits on the network, which is
/// the whole reason the *send* is started under it rather than awaited under
/// it. An unmute takes nothing: it revives no capture and invalidates nothing.
pub(crate) fn apply_mute(
    muted: &AtomicBool,
    mute_epochs: &AtomicU64,
    authority: &Mutex<()>,
    status: &Mutex<AmbientStatus>,
    on: bool,
) {
    if on {
        let _authority = authority.lock().unwrap_or_else(|e| e.into_inner());
        mute_epochs.fetch_add(1, Ordering::AcqRel);
        muted.store(true, Ordering::Release);
    } else {
        muted.store(false, Ordering::Release);
    }
    let mut status = status.lock().unwrap_or_else(|e| e.into_inner());
    *status = if on {
        AmbientStatus::Muted
    } else {
        AmbientStatus::Listening
    };
}
