import * as React from "react";
import { AppHuddleBar } from "@/app/AppHuddleBar";
import * as BuzzTheme from "@/app/BuzzThemeSurfaces";
import {
  AmbientVoiceIndicator,
  AmbientVoiceProvider,
} from "@/features/ambient-voice";
import { HuddleProvider, useHuddle } from "@/features/huddle";
import { HUDDLE_SHORTCUT_EVENT } from "@/shared/lib/keyboard-shortcuts";
import { RemindMeLaterProvider } from "@/features/reminders/ui/RemindMeLaterProvider";
import { cn } from "@/shared/lib/cn";

type AppHuddleShellProps = {
  children: React.ReactNode;
  currentPubkey?: string;
  isCompanionOpen: boolean;
  isDrawerOpen: boolean;
  isRoom: boolean;
  onCompanionOpen: () => void;
  onHuddleStartPendingChange: (pending: boolean) => void;
  onHuddleStarted: (ephemeralChannelId: string) => void | Promise<void>;
  onShowHuddleInMainApp: (ephemeralChannelId: string) => void;
  onViewHuddleChannel: (ephemeralChannelId: string) => void;
  onVisibilityChange: (visible: boolean) => void;
};

type HuddleShortcutHandlerProps = {
  children: React.ReactNode;
};

function HuddleShortcutHandler({ children }: HuddleShortcutHandlerProps) {
  const { activeEphemeralChannelId, leaveHuddle } = useHuddle();

  React.useEffect(() => {
    if (!activeEphemeralChannelId) return;

    function handleHuddleShortcut() {
      void leaveHuddle();
    }

    window.addEventListener(HUDDLE_SHORTCUT_EVENT, handleHuddleShortcut);
    return () =>
      window.removeEventListener(HUDDLE_SHORTCUT_EVENT, handleHuddleShortcut);
  }, [activeEphemeralChannelId, leaveHuddle]);

  return children;
}

export function AppHuddleShell({
  children,
  currentPubkey,
  isCompanionOpen,
  isDrawerOpen,
  isRoom,
  onCompanionOpen,
  onHuddleStartPendingChange,
  onHuddleStarted,
  onShowHuddleInMainApp,
  onViewHuddleChannel,
  onVisibilityChange,
}: AppHuddleShellProps) {
  return (
    <HuddleProvider
      ownsAudioSession={!isRoom}
      onHuddleStartPendingChange={
        isRoom ? undefined : onHuddleStartPendingChange
      }
      onHuddleStarted={isRoom ? undefined : onHuddleStarted}
      onShowHuddleInMainApp={isRoom ? undefined : onShowHuddleInMainApp}
      onViewHuddleChannel={isRoom ? undefined : onViewHuddleChannel}
    >
      {/* Inside HuddleProvider so it can see the active huddle, and gated on
          audio-session ownership so the dedicated huddle window never opens a
          second microphone. Renders nothing while the flag is off. */}
      <AmbientVoiceArbitration ownsAudioSession={!isRoom} />
      <HuddleShortcutHandler>
        <RemindMeLaterProvider pubkey={currentPubkey}>
          <div
            className="buzz-huddle-shell relative h-dvh overflow-hidden overscroll-none"
            data-huddle-open={isDrawerOpen}
            data-huddle-window={isRoom}
          >
            <div
              aria-hidden="true"
              className={cn(
                "buzz-huddle-drawer-backdrop",
                isDrawerOpen && "buzz-huddle-drawer-backdrop-open",
              )}
            />
            <div
              className={cn(
                "buzz-huddle-app-surface z-10 flex min-h-0 flex-row overflow-hidden bg-background",
                isDrawerOpen &&
                  (isRoom
                    ? "buzz-huddle-app-surface-room-open"
                    : "buzz-huddle-app-surface-open"),
              )}
            >
              <BuzzTheme.GradientLayer />
              {children}
            </div>
            {isRoom || !isCompanionOpen ? (
              <div className="buzz-huddle-drawer-slot absolute inset-x-0 bottom-0 z-[2] h-(--buzz-huddle-drawer-height)">
                <AppHuddleBar
                  mode={isRoom ? "room" : "main"}
                  onOpenHuddleWindow={isRoom ? undefined : onCompanionOpen}
                  onVisibilityChange={onVisibilityChange}
                />
              </div>
            ) : null}
          </div>
        </RemindMeLaterProvider>
      </HuddleShortcutHandler>
    </HuddleProvider>
  );
}

/**
 * Reads the live huddle state and hands it to the ambient session.
 *
 * A separate component because `useHuddle` needs a consumer *inside* the
 * provider. The native side also suspends the ambient session from the huddle
 * start/join paths — this is the webview half of the same arbitration, and it
 * is what releases the microphone device rather than merely ignoring its
 * frames.
 */
function AmbientVoiceArbitration({
  ownsAudioSession,
}: {
  ownsAudioSession: boolean;
}) {
  const { activeEphemeralChannelId, isStarting } = useHuddle();
  return (
    <>
      <AmbientVoiceProvider
        activeHuddleChannelId={
          isStarting ? "starting" : activeEphemeralChannelId
        }
        ownsAudioSession={ownsAudioSession}
      />
      {/* The indicator renders nothing unless the feature is on and
          configured. While it is, the OS microphone light is permanently lit,
          so the app owes the user a visible state and a one-click mute.
          It positions itself: the pill is draggable and remembers where it was
          left, so the corner is its own business rather than the shell's. */}
      {ownsAudioSession ? (
        <AmbientVoiceIndicator className="pointer-events-auto z-50 max-w-64 bg-background/90 backdrop-blur-sm" />
      ) : null}
    </>
  );
}
