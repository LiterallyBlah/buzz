import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import * as React from "react";

import {
  setupAudioWorklet,
  type AudioWorkletHandle,
} from "@/features/huddle/lib/audioWorklet";
import { useFeatureEnabled } from "@/shared/features";
import {
  ambientCaptureErrorMessage,
  ambientReplyChannel,
  shouldCaptureAmbientAudio,
  AMBIENT_AUDIO_FLOW_INTERVAL_MS,
  AMBIENT_CAPTURE_LOST_MESSAGE,
  AMBIENT_HOTSTART_INTERVAL_MS,
} from "./lib/ambientCapture";
import {
  AMBIENT_STATE_CHANGED_EVENT,
  checkAmbientHotstart,
  getAmbientVoiceStatus,
  reportAmbientAudioFlow,
  reportAmbientCaptureError,
  type AmbientVoiceStatusReport,
} from "./lib/ambientVoiceApi";
import { useAmbientReplySubscription } from "./lib/useAmbientReplySubscription";

/**
 * Headless controller for the `ambientVoice` preview feature.
 *
 * Owns exactly two things the native side cannot do for itself: holding a
 * microphone open in the webview (all mic capture in Buzz is `getUserMedia`),
 * and watching the relay for the bound agent's replies.
 *
 * **Nothing happens here unless the flag is on.** The effects below all key on
 * `featureEnabled`, and when it is false the component subscribes to nothing,
 * polls nothing, and never touches `navigator.mediaDevices`. That is the first
 * acceptance criterion, and `shouldCaptureAmbientAudio` is where it is decided
 * and tested.
 */
export type AmbientVoiceProviderProps = {
  /**
   * Whether this window owns the audio session. The dedicated huddle room
   * window renders the same tree and must not open a second microphone.
   */
  ownsAudioSession: boolean;
  /** Non-null while a huddle is running. The huddle owns the microphone. */
  activeHuddleChannelId: string | null;
};

export function AmbientVoiceProvider({
  ownsAudioSession,
  activeHuddleChannelId,
}: AmbientVoiceProviderProps) {
  const featureEnabled = useFeatureEnabled("ambientVoice");
  const [report, setReport] = React.useState<AmbientVoiceStatusReport | null>(
    null,
  );
  const selfPubkeyRef = React.useRef<string | null>(null);
  const workletRef = React.useRef<AudioWorkletHandle | null>(null);
  const streamRef = React.useRef<MediaStream | null>(null);
  // Whether the pipeline below is actually built. Distinct from `shouldCapture`,
  // which is only what the native report asks for: the gap between the two is
  // where a `getUserMedia` still waiting on a permission prompt lives, and a
  // report that never says `captureReady` is itself the diagnosis.
  const [captureReady, setCaptureReady] = React.useState(false);

  // ── Native state: listen first, then snapshot ────────────────────────────
  //
  // Installed before the snapshot request so a transition that happens while
  // the IPC is in flight is not lost behind a stale answer.
  React.useEffect(() => {
    if (!featureEnabled) {
      setReport(null);
      return;
    }
    let disposed = false;
    let unlisten: UnlistenFn | null = null;

    void listen<AmbientVoiceStatusReport>(
      AMBIENT_STATE_CHANGED_EVENT,
      (event) => {
        if (!disposed) setReport(event.payload);
      },
    )
      .then((dispose) => {
        if (disposed) {
          dispose();
          return;
        }
        unlisten = dispose;
        void getAmbientVoiceStatus()
          .then((snapshot) => {
            // Only seed; a live event that already arrived is newer.
            if (!disposed) setReport((current) => current ?? snapshot);
          })
          .catch((error) => {
            console.warn("[ambient] status snapshot failed:", error);
          });
      })
      .catch((error) => {
        console.warn("[ambient] status listener failed:", error);
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [featureEnabled]);

  // ── Self pubkey, for "never speak my own messages" ───────────────────────
  React.useEffect(() => {
    if (!featureEnabled || selfPubkeyRef.current) return;
    let disposed = false;
    void invoke<{ pubkey: string }>("get_identity")
      .then((identity) => {
        if (!disposed) selfPubkeyRef.current = identity.pubkey;
      })
      .catch(() => {
        /* best-effort; the classifier fails closed on a null self pubkey */
      });
    return () => {
      disposed = true;
    };
  }, [featureEnabled]);

  // ── Hot start ────────────────────────────────────────────────────────────
  //
  // The wake-word model downloads on demand, so switching the feature on
  // usually cannot start a session immediately. This is also what recovers a
  // worker that exited. No-op on the native side unless a session should be
  // running and is not.
  React.useEffect(() => {
    if (!featureEnabled || !ownsAudioSession) return;
    const poll = () => {
      void checkAmbientHotstart()
        .then(setReport)
        .catch((error) => {
          console.debug("[ambient] hotstart poll failed:", error);
        });
    };
    poll();
    const id = window.setInterval(poll, AMBIENT_HOTSTART_INTERVAL_MS);
    return () => window.clearInterval(id);
  }, [featureEnabled, ownsAudioSession]);

  // ── Microphone ───────────────────────────────────────────────────────────
  const shouldCapture = shouldCaptureAmbientAudio({
    featureEnabled,
    ownsAudioSession,
    huddleActive: activeHuddleChannelId !== null,
    report,
  });
  // Re-acquire the device when the persisted choice changes, so picking a
  // microphone in settings takes effect without a restart.
  const inputDeviceId = report?.inputDeviceId ?? null;

  React.useEffect(() => {
    const releaseTracks = (stream: MediaStream | null) => {
      for (const track of stream?.getTracks() ?? []) track.stop();
    };

    if (!shouldCapture) {
      workletRef.current?.stop();
      workletRef.current = null;
      releaseTracks(streamRef.current);
      streamRef.current = null;
      setCaptureReady(false);
      return;
    }

    let disposed = false;
    let localWorklet: AudioWorkletHandle | null = null;
    let localStream: MediaStream | null = null;

    // The native side cannot see any of this: it has no device handle, only a
    // queue that stops filling. Telling it is what turns a dead microphone into
    // the error state the indicator shows, instead of a session that claims to
    // be listening. The report also stops the session, so the existing
    // hot-start poll is what re-arms once a device is available again — there
    // is deliberately no retry of our own here.
    const reportFailure = (message: string) => {
      if (disposed) return;
      void reportAmbientCaptureError(message)
        .then(setReport)
        .catch((error) => {
          console.warn(
            "[ambient] capture failure could not be reported:",
            error,
          );
        });
    };

    void (async () => {
      try {
        // The persisted device comes from the status report rather than a
        // second settings fetch, so a device change is one re-render of this
        // effect rather than a race between two async reads.
        const constraints: MediaTrackConstraints = {
          echoCancellation: true,
          noiseSuppression: true,
          sampleRate: 48000,
        };
        if (inputDeviceId) {
          constraints.deviceId = { exact: inputDeviceId };
        }
        const stream = await navigator.mediaDevices.getUserMedia({
          audio: constraints,
        });
        localStream = stream;
        if (disposed) {
          releaseTracks(stream);
          return;
        }
        const track = stream.getAudioTracks()[0];
        // A device unplugged mid-session ends its track and simply stops
        // producing frames; `stop()` on our own teardown does not fire this.
        track?.addEventListener("ended", () =>
          reportFailure(AMBIENT_CAPTURE_LOST_MESSAGE),
        );
        const worklet = await setupAudioWorklet(track, "voice_activity", true, {
          command: "push_ambient_audio_pcm",
          pushToTalk: false,
        });
        if (disposed) {
          worklet.stop();
          releaseTracks(stream);
          return;
        }
        localWorklet = worklet;
        workletRef.current = worklet;
        streamRef.current = stream;
        setCaptureReady(true);
      } catch (error) {
        // Always release the device on any failure path, so a failed setup
        // cannot leave the OS microphone indicator lit.
        releaseTracks(localStream);
        console.error("[ambient] microphone setup failed:", error);
        reportFailure(ambientCaptureErrorMessage(error));
      }
    })();

    return () => {
      disposed = true;
      localWorklet?.stop();
      releaseTracks(localStream);
      if (workletRef.current === localWorklet) workletRef.current = null;
      if (streamRef.current === localStream) streamRef.current = null;
      setCaptureReady(false);
    };
  }, [shouldCapture, inputDeviceId]);

  // ── Audio flow ───────────────────────────────────────────────────────────
  //
  // Tell the native side what this webview has pushed, slowly. It is the half
  // of the audio path nothing else can see — batches pushed that never arrive
  // and batches never pushed at all look identical from the native side — and
  // it is also what makes a session that has gone quiet visible: staleness
  // moves with the clock rather than with a transition, so nothing else would
  // ever emit it.
  React.useEffect(() => {
    if (!shouldCapture) return;
    let disposed = false;
    const tick = () => {
      void reportAmbientAudioFlow(
        workletRef.current?.pushedBatches() ?? 0,
        captureReady,
      )
        .then((next) => {
          if (!disposed) setReport(next);
        })
        .catch((error) => {
          console.debug("[ambient] audio flow report failed:", error);
        });
    };
    const id = window.setInterval(tick, AMBIENT_AUDIO_FLOW_INTERVAL_MS);
    return () => {
      disposed = true;
      window.clearInterval(id);
    };
  }, [shouldCapture, captureReady]);

  // ── Replies ──────────────────────────────────────────────────────────────
  useAmbientReplySubscription(
    ambientReplyChannel(featureEnabled, report),
    report?.agentPubkey ?? null,
    selfPubkeyRef,
  );

  return null;
}
