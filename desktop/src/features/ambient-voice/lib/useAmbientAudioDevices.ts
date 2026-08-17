import { invoke } from "@tauri-apps/api/core";
import * as React from "react";

export type AmbientInputDevice = { deviceId: string; label: string };
export type AmbientOutputDevice = { name: string; is_default: boolean };

/**
 * Device lists for the ambient settings pickers.
 *
 * Inputs come from the webview (that is where `getUserMedia` runs); outputs
 * come from Rust (that is where rodio plays). Same split as huddles — the only
 * difference is that the ambient choices are persisted, which is why the
 * pickers write into `ambient-voice-settings.json` rather than React state.
 */
export function useAmbientAudioDevices() {
  const [inputDevices, setInputDevices] = React.useState<AmbientInputDevice[]>(
    [],
  );
  const [outputDevices, setOutputDevices] = React.useState<
    AmbientOutputDevice[]
  >([]);

  React.useEffect(() => {
    let disposed = false;
    const refresh = () => {
      navigator.mediaDevices
        ?.enumerateDevices()
        .then((devices) => {
          if (disposed) return;
          setInputDevices(
            devices
              .filter((device) => device.kind === "audioinput")
              .map((device) => ({
                deviceId: device.deviceId,
                label: device.label,
              })),
          );
        })
        .catch(() => {
          /* best-effort: labels need a permission grant, ids do not */
        });
    };
    refresh();
    navigator.mediaDevices?.addEventListener("devicechange", refresh);
    return () => {
      disposed = true;
      navigator.mediaDevices?.removeEventListener("devicechange", refresh);
    };
  }, []);

  React.useEffect(() => {
    let disposed = false;
    void invoke<AmbientOutputDevice[]>("list_audio_output_devices")
      .then((devices) => {
        if (!disposed) setOutputDevices(devices);
      })
      .catch(() => {
        /* best-effort; the picker falls back to "System default" */
      });
    return () => {
      disposed = true;
    };
  }, []);

  return { inputDevices, outputDevices };
}
