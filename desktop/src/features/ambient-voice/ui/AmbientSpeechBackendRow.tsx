import * as React from "react";
import { ChevronDown } from "lucide-react";

import { SettingsOptionRow } from "@/features/settings/ui/SettingsOptionGroup";
import { Button } from "@/shared/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/shared/ui/dropdown-menu";
import { Input } from "@/shared/ui/input";
import {
  checkSpeechEndpoint,
  type SpeechBackend,
  type SpeechBackendSettings,
} from "../lib/ambientVoiceApi";
import {
  speechBackendLabel,
  speechBackendNotice,
  speechCheckIsProblem,
  speechCheckLabel,
  SPEECH_BACKEND_OPTIONS,
  SPEECH_CHECK_IDLE,
  SPEECH_ENDPOINT_PLACEHOLDER,
  SPEECH_ROLE_COPY,
  type SpeechCheckState,
  type SpeechRole,
} from "../lib/ambientSpeechBackend";

/**
 * One speech role's backend: this computer, or a server the user names.
 *
 * The address is committed on blur rather than on every keystroke, exactly as
 * the wake-word field is: each commit is a settings write, and a settings write
 * restarts the session so the change reaches the engines.
 */
export function AmbientSpeechBackendRow({
  onChange,
  role,
  value,
}: {
  onChange: (next: SpeechBackendSettings) => void;
  role: SpeechRole;
  value: SpeechBackendSettings;
}) {
  const copy = SPEECH_ROLE_COPY[role];
  const [url, setUrl] = React.useState(value.endpointUrl ?? "");
  const [check, setCheck] = React.useState<SpeechCheckState>(SPEECH_CHECK_IDLE);

  // Follow the persisted value when it changes underneath us — a save that was
  // refused, or another window's write, must not leave a stale address on
  // screen looking accepted.
  React.useEffect(() => {
    setUrl(value.endpointUrl ?? "");
  }, [value.endpointUrl]);

  const commit = React.useCallback(
    (backend: SpeechBackend, nextUrl: string) => {
      const trimmed = nextUrl.trim();
      const endpointUrl = trimmed.length > 0 ? trimmed : null;
      if (backend === value.backend && endpointUrl === value.endpointUrl) {
        return;
      }
      onChange({ backend, endpointUrl });
    },
    [onChange, value.backend, value.endpointUrl],
  );

  const runCheck = React.useCallback(() => {
    setCheck({ phase: "checking" });
    void checkSpeechEndpoint(url)
      .then((next) => setCheck({ phase: "done", check: next }))
      .catch((error) => {
        setCheck({
          phase: "failed",
          message:
            error instanceof Error
              ? error.message
              : "The server could not be checked.",
        });
      });
  }, [url]);

  const notice = speechBackendNotice(value);
  const checkLine = speechCheckLabel(check);
  const testId = `ambient-speech-${role}`;

  return (
    <>
      <SettingsOptionRow>
        <div className="min-w-0">
          <p className="text-sm font-medium">{copy.label}</p>
          <p className="text-sm font-normal text-muted-foreground">
            {copy.description}
          </p>
        </div>
        <DropdownMenu modal={false}>
          <DropdownMenuTrigger asChild>
            <Button
              className="h-7 min-w-40 justify-between gap-1.5"
              data-testid={`${testId}-trigger`}
              size="sm"
              type="button"
              variant="ghost"
            >
              <span className="truncate">{speechBackendLabel(value)}</span>
              <ChevronDown className="h-4 w-4 text-muted-foreground" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="min-w-56">
            <DropdownMenuRadioGroup
              onValueChange={(next) => commit(next as SpeechBackend, url)}
              value={value.backend}
            >
              {SPEECH_BACKEND_OPTIONS.map((option) => (
                <DropdownMenuRadioItem
                  data-testid={`${testId}-${option.value}`}
                  key={option.value}
                  value={option.value}
                >
                  {option.label}
                </DropdownMenuRadioItem>
              ))}
            </DropdownMenuRadioGroup>
          </DropdownMenuContent>
        </DropdownMenu>
      </SettingsOptionRow>

      {value.backend === "http" ? (
        <div className="flex flex-col gap-1.5 px-4 pb-3">
          <div className="flex items-center gap-2">
            <Input
              aria-label={`${copy.label} server address`}
              className="max-w-80"
              data-testid={`${testId}-url`}
              onBlur={() => commit(value.backend, url)}
              onChange={(event) => setUrl(event.target.value)}
              placeholder={SPEECH_ENDPOINT_PLACEHOLDER}
              value={url}
            />
            <Button
              data-testid={`${testId}-check`}
              onClick={runCheck}
              size="sm"
              type="button"
              variant="ghost"
            >
              Check
            </Button>
          </div>
          <p
            className="text-2xs text-muted-foreground"
            data-testid={`${testId}-hint`}
          >
            {copy.hint}
          </p>
          {notice ? (
            <p
              className="text-2xs text-muted-foreground"
              data-testid={`${testId}-notice`}
            >
              {notice}
            </p>
          ) : null}
          {checkLine ? (
            <p
              className={
                speechCheckIsProblem(check)
                  ? "text-2xs text-destructive"
                  : "text-2xs text-muted-foreground"
              }
              data-testid={`${testId}-check-result`}
            >
              {checkLine}
            </p>
          ) : null}
        </div>
      ) : null}
    </>
  );
}
