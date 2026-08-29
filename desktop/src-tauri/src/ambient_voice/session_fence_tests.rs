//! The mute-fence regressions.
//!
//! Every one of these is an ordering: where the mute landed relative to the
//! capture it kills — while the capture was still collecting, while the close
//! was blocked on the transcriber, or between two audio batches with the unmute
//! already behind it. They are driven through the production `finish_capture`,
//! `abandon_capture` and `apply_mute` rather than through hand-rolled copies,
//! because a fence a test re-implements is a fence nothing checks.
//!
//! Split out of `session_tests` for size alone; both files are children of
//! `session` and see the worker through the same `use super::*`.

use super::session_test_support::{recorder, scripted_capture, Announced};
use super::*;

#[test]
fn a_mute_landing_during_the_final_close_is_not_talked_over() {
    // The close blocks the worker for as long as transcription takes — against
    // a slow server, minutes — and the mute button works the whole time:
    // `set_muted` stores the flag and writes `Muted` to the pill from the
    // command thread. The worker only reads the flag between audio batches, so
    // without a fence the close queued the muted words for publication anyway
    // and then wrote `Listening` straight over the `Muted` the user was shown.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let mut rolling = RollingCapture::spawn(move |_| {
        let _ = release_rx.recv();
        Ok("said just before the mute".to_string())
    })
    .expect("rolling");

    let muted = Arc::new(AtomicBool::new(false));
    let mute_epochs = Arc::new(AtomicU64::new(0));
    let mute_authority = Mutex::new(());
    let status_cell = Arc::new(Mutex::new(AmbientStatus::Capturing));
    let announced: Announced = Arc::new(Mutex::new(Vec::new()));
    let status = StatusSink::new(Arc::clone(&status_cell), Some(recorder(&announced)));
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(4);

    let worker = {
        let muted = Arc::clone(&muted);
        let mute_epochs = Arc::clone(&mute_epochs);
        thread::spawn(move || {
            let flow = AudioFlow::new();
            finish_capture(
                &mut rolling,
                &mut vec![0.05_f32; 16_000],
                &transcript_tx,
                &status,
                None,
                &flow,
                MuteSignal {
                    muted: &muted,
                    epochs: &mute_epochs,
                    // Armed before the mute this test lands mid-close.
                    armed_under: Some(mute_epochs.load(Ordering::Acquire)),
                },
            );
        })
    };

    // Wait until the close is genuinely inside its blocking wait…
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Transcribing
    ) {
        assert!(Instant::now() < deadline, "the close never started");
        thread::sleep(Duration::from_millis(1));
    }
    // …then mute through the exact writes the `set_muted` command performs…
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
    // …and only then let the transcription finish.
    drop(release_tx);
    worker.join().expect("the close panicked");

    assert!(
        transcripts.try_recv().is_err(),
        "words muted mid-close were queued for publication anyway"
    );
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Muted,
        "the close talked over the mute the user was shown"
    );
    assert_eq!(
        announced.lock().expect("announced").clone(),
        vec![AmbientStatus::Transcribing],
        "nothing after the mute may announce a status"
    );
}

#[test]
fn an_unmute_before_the_close_returns_does_not_revive_the_muted_capture() {
    // The sharper race: mute lands while the close is waiting, and the user
    // unmutes before the decoder returns. A fence that reads the flag's
    // present value finds it false again — the unmute erased the evidence —
    // and the capture the user muted is queued anyway. The mute is a death
    // sentence for the capture in progress, pronounced at the event; an
    // unmute may open the microphone for the next capture, never revive the
    // last one. `apply_mute` therefore latches an epoch on every mute-on, and
    // the close compares epochs around its wait instead of trusting the flag.
    let next = AtomicU64::new(0);
    let (tokens_tx, tokens_rx) = mpsc::channel::<()>();
    let mut rolling = RollingCapture::spawn(move |_| {
        let _ = tokens_rx.recv();
        match next.fetch_add(1, Ordering::AcqRel) {
            0 => Ok("said before the mute".to_string()),
            1 => Ok("said after the unmute".to_string()),
            _ => Err("Speech server failed: HTTP 502".to_string()),
        }
    })
    .expect("rolling");

    let muted = Arc::new(AtomicBool::new(false));
    let mute_epochs = Arc::new(AtomicU64::new(0));
    let mute_authority = Mutex::new(());
    let status_cell = Arc::new(Mutex::new(AmbientStatus::Capturing));
    let announced: Announced = Arc::new(Mutex::new(Vec::new()));
    let status = StatusSink::new(Arc::clone(&status_cell), Some(recorder(&announced)));
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(8);
    let flow = AudioFlow::new();

    let wait_for_transcribing = |cell: &Arc<Mutex<AmbientStatus>>| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(*cell.lock().expect("status"), AmbientStatus::Transcribing) {
            assert!(Instant::now() < deadline, "the close never started");
            thread::sleep(Duration::from_millis(1));
        }
    };
    let close_while = |rolling: &mut RollingCapture, mid_close: &dyn Fn()| {
        // The capture being closed was armed under the epoch in force when the
        // close begins: every mute in this test lands mid-close, after it.
        let armed_under = Some(mute_epochs.load(Ordering::Acquire));
        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                finish_capture(
                    rolling,
                    &mut vec![0.05_f32; 16_000],
                    &transcript_tx,
                    &status,
                    None,
                    &flow,
                    MuteSignal {
                        muted: &muted,
                        epochs: &mute_epochs,
                        armed_under,
                    },
                );
            });
            mid_close();
            tokens_tx.send(()).expect("release the decoder");
            worker.join().expect("the close panicked");
        });
    };

    // A mute and an unmute both land while the close is waiting: the words
    // muted mid-flight are not sent, and the pill reads what the unmute wrote.
    close_while(&mut rolling, &|| {
        wait_for_transcribing(&status_cell);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, false);
    });
    assert!(
        transcripts.try_recv().is_err(),
        "an unmute revived the capture the mute had killed"
    );
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Listening
    );

    // The unmute opened the microphone for what comes next: a genuinely new
    // capture on the same machinery decodes, stitches and publishes.
    close_while(&mut rolling, &|| {});
    assert_eq!(
        transcripts.try_recv().ok().map(|t| t.text).as_deref(),
        Some("said after the unmute"),
        "the fence must kill the muted capture, not the session"
    );

    // And a stale close cannot repaint the state the mute cycle established:
    // this decode *fails* after the mute→unmute, and the failure belongs to a
    // dead capture — the pill keeps reading `Listening`, not `Error`.
    announced.lock().expect("announced").clear();
    close_while(&mut rolling, &|| {
        wait_for_transcribing(&status_cell);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, false);
    });
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Listening,
        "a dead capture's failure repainted the pill"
    );
    assert_eq!(
        announced.lock().expect("announced").clone(),
        vec![AmbientStatus::Transcribing],
        "a dead capture announced something after the mute cycle"
    );
}

/// The worker's capture stage, driven frame by frame without the engines.
///
/// The wake word and the VAD's verdict are the two answers the ONNX models
/// supply, and both are inputs here; everything the worker does with them —
/// arming, buffering, closing — is the production machine. That is what lets
/// the mute fence be driven from where a capture *starts*, which is where the
/// epoch is now bound and therefore where the orderings below have to begin.
/// One instant serves throughout: nothing in this fence depends on time
/// passing, and a test that slept would be proving something else.
struct CaptureRun {
    machine: UtteranceMachine,
    speech_buf: Vec<f32>,
    now: Instant,
}

impl CaptureRun {
    fn new() -> Self {
        Self {
            machine: UtteranceMachine::new(UtteranceTiming::from_silence_hold_ms(
                crate::ambient_voice::utterance::DEFAULT_SILENCE_HOLD_MS,
            )),
            speech_buf: Vec::new(),
            now: Instant::now(),
        }
    }

    /// A wake word fired: the capture begins here.
    fn wake(&mut self) {
        self.machine.on_wake(self.now);
    }

    /// `frames` frames the VAD called speech, buffered as the worker buffers.
    fn speak(&mut self, frames: usize) {
        for _ in 0..frames {
            match self.machine.on_frame(true, false, self.now) {
                FrameOutcome::Buffer => self.buffer_frame(),
                other => panic!("a speech frame produced {other:?}"),
            }
        }
    }

    /// Silence until the machine closes the capture, and how it closed.
    fn silence_until_close(&mut self) -> FrameOutcome {
        // Generous: the close comes after `silence_flush_frames`, and anything
        // that does not reach it is a hung capture rather than a slow one.
        for _ in 0..10_000 {
            match self.machine.on_frame(false, false, self.now) {
                FrameOutcome::Buffer => self.buffer_frame(),
                closed => {
                    self.buffer_frame();
                    return closed;
                }
            }
        }
        panic!("the capture never closed")
    }

    fn buffer_frame(&mut self) {
        self.speech_buf
            .extend_from_slice(&[0.05_f32; VAD_FRAME_SAMPLES]);
    }
}

#[test]
fn a_mute_and_unmute_before_the_close_begins_does_not_revive_the_capture() {
    // The ordering the close cannot see from where it stands. The worker reads
    // mute once per batch, so a mute and an unmute can both complete while a
    // capture is collecting — between two batches, or inside one before the
    // frame that ends the utterance. The flag is back to false and the close
    // has not started yet, so a fence that snapshots the epoch when the close
    // *begins* snapshots the epoch the mute already moved to, compares it
    // against itself, and publishes the capture the user muted.
    //
    // The capture's authority is therefore bound when the capture starts and
    // carried for its whole life: the close is handed the epoch its capture
    // was armed under and has nothing of its own to snapshot.
    //
    // Counted rather than scripted: a capture that is dead when its close
    // begins must never reach the transcriber at all, which is a stronger
    // statement than "its words were not published" and the only one that
    // holds against a speech server the close would otherwise wait on.
    let decodes = Arc::new(AtomicU64::new(0));
    let mut rolling = {
        let decodes = Arc::clone(&decodes);
        RollingCapture::spawn(move |_| {
            decodes.fetch_add(1, Ordering::AcqRel);
            Ok("said after the unmute".to_string())
        })
        .expect("rolling")
    };
    let muted = Arc::new(AtomicBool::new(false));
    let mute_epochs = Arc::new(AtomicU64::new(0));
    let mute_authority = Mutex::new(());
    let status_cell = Arc::new(Mutex::new(AmbientStatus::Listening));
    let announced: Announced = Arc::new(Mutex::new(Vec::new()));
    let status = StatusSink::new(Arc::clone(&status_cell), Some(recorder(&announced)));
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(8);
    let flow = AudioFlow::new();

    // A real capture, under the epoch in force when the wake word fired.
    let mut capture_epoch = CaptureEpoch::default();
    let mut capture = CaptureRun::new();
    capture.wake();
    capture_epoch.arm(&mute_epochs);
    let armed_under = mute_epochs.load(Ordering::Acquire);
    capture.speak(crate::ambient_voice::utterance::MIN_VOICED_FRAMES + 2);
    assert!(
        !capture.speech_buf.is_empty(),
        "the capture collected no audio to be muted"
    );

    // The user mutes and unmutes while the capture is still collecting, through
    // the writes the `set_ambient_voice_muted` command performs.
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, false);
    announced.lock().expect("announced").clear();

    // Only now does the capture reach its close.
    assert_eq!(capture.silence_until_close(), FrameOutcome::Decode);
    finish_capture(
        &mut rolling,
        &mut capture.speech_buf,
        &transcript_tx,
        &status,
        None,
        &flow,
        MuteSignal {
            muted: &muted,
            epochs: &mute_epochs,
            armed_under: capture_epoch.disarm(),
        },
    );

    assert!(
        transcripts.try_recv().is_err(),
        "old capture was revived because close snapshots epoch after mute"
    );
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Listening,
        "the dead capture repainted the pill the unmute wrote"
    );
    assert_eq!(
        announced.lock().expect("announced").clone(),
        Vec::new(),
        "the dead capture announced a status of its own"
    );
    assert!(
        !transcript_still_wanted(&mute_epochs, armed_under),
        "the epoch the capture was armed under is still current"
    );
    assert_eq!(
        decodes.load(Ordering::Acquire),
        0,
        "a capture that was already dead was still sent to the transcriber"
    );

    // The unmute opened the microphone for what comes next: a capture armed
    // after it carries the later epoch and publishes normally.
    let mut next = CaptureRun::new();
    next.wake();
    capture_epoch.arm(&mute_epochs);
    let next_armed_under = mute_epochs.load(Ordering::Acquire);
    next.speak(crate::ambient_voice::utterance::MIN_VOICED_FRAMES + 2);
    assert_eq!(next.silence_until_close(), FrameOutcome::Decode);
    finish_capture(
        &mut rolling,
        &mut next.speech_buf,
        &transcript_tx,
        &status,
        None,
        &flow,
        MuteSignal {
            muted: &muted,
            epochs: &mute_epochs,
            armed_under: capture_epoch.disarm(),
        },
    );

    let published = transcripts
        .try_recv()
        .expect("the live capture was dropped");
    assert_eq!(published.text, "said after the unmute");
    assert_eq!(
        published.mute_epoch, next_armed_under,
        "the transcript was stamped with an epoch its capture did not begin under"
    );
    assert!(transcript_still_wanted(&mute_epochs, published.mute_epoch));
    assert_eq!(
        decodes.load(Ordering::Acquire),
        1,
        "the live capture did not reach the transcriber"
    );
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Listening
    );
}

#[test]
fn an_epoch_that_moved_between_batches_kills_the_capture_the_worker_never_saw_muted() {
    // The other half of the same ordering, one batch earlier. The worker reads
    // the flag once per batch and it is false at both looks, so the mute is
    // invisible to it — but the counter moved, and the capture in progress was
    // bound to the value before it. What must happen at that observation is
    // exactly what a mute the worker *did* see would do: the machine goes back
    // to idle, the buffer is dropped, and the chunks already handed off are
    // disowned so they cannot be stitched into whatever is said next.
    let mut rolling = scripted_capture(&["a chunk of the muted capture", "said after the unmute"]);
    let muted = Arc::new(AtomicBool::new(false));
    let mute_epochs = Arc::new(AtomicU64::new(0));
    let mute_authority = Mutex::new(());
    let status_cell = Arc::new(Mutex::new(AmbientStatus::Listening));
    let status = StatusSink::new(Arc::clone(&status_cell), None);
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(8);
    let flow = AudioFlow::new();

    // A capture long enough for the ceiling to have taken a chunk off it.
    let mut capture_epoch = CaptureEpoch::default();
    let mut capture = CaptureRun::new();
    capture.wake();
    capture_epoch.arm(&mute_epochs);
    capture.speak(crate::ambient_voice::utterance::MIN_VOICED_FRAMES + 2);
    rolling.hand_off(std::mem::take(&mut capture.speech_buf));

    // The mute and the unmute both land between two batches.
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, false);
    assert!(
        !muted.load(Ordering::Acquire),
        "the flag the worker reads is up, so this is not the ordering under test"
    );

    // The next batch arrives. This is the check the worker makes on it.
    assert!(
        capture_epoch.revoked(&mute_epochs),
        "an epoch change between batches was not seen as one"
    );
    abandon_capture(
        &mut capture.machine,
        &mut capture.speech_buf,
        &mut rolling,
        &mut capture_epoch,
    );
    assert_eq!(
        capture.machine.phase(),
        crate::ambient_voice::utterance::UtterancePhase::Idle,
        "the muted capture is still collecting"
    );
    assert!(capture.speech_buf.is_empty());
    assert!(
        capture_epoch.disarm().is_none(),
        "the killed capture is still holding an epoch"
    );

    // A genuinely new capture on the same machinery: what it sends is its own
    // words, with nothing of the abandoned capture stitched in front of them.
    let mut next = CaptureRun::new();
    next.wake();
    capture_epoch.arm(&mute_epochs);
    next.speak(crate::ambient_voice::utterance::MIN_VOICED_FRAMES + 2);
    assert_eq!(next.silence_until_close(), FrameOutcome::Decode);
    finish_capture(
        &mut rolling,
        &mut next.speech_buf,
        &transcript_tx,
        &status,
        None,
        &flow,
        MuteSignal {
            muted: &muted,
            epochs: &mute_epochs,
            armed_under: capture_epoch.disarm(),
        },
    );

    let published = transcripts
        .try_recv()
        .expect("the live capture was dropped");
    assert_eq!(
        published.text, "said after the unmute",
        "a chunk of the abandoned capture was stitched into the next one"
    );
    assert!(transcripts.try_recv().is_err());
}
