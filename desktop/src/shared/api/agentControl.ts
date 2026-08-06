import { sendAgentObserverControl } from "@/shared/api/observerRelay";
import type { CancelManagedAgentTurnResult } from "@/shared/api/types";

export async function cancelManagedAgentTurn(
  pubkey: string,
  channelId: string,
): Promise<CancelManagedAgentTurnResult> {
  await sendAgentObserverControl(pubkey, {
    type: "cancel_turn",
    channelId,
  });
  return { status: "sent" };
}

/** Ask the running agent to cancel every active turn and discard all work already queued. */
export async function cancelAllAgentWork(pubkey: string): Promise<void> {
  await sendAgentObserverControl(pubkey, { type: "cancel_all" });
}

/**
 * Send a live model-switch control frame to a running agent. The switch rides
 * the harness's cancel-switch-requeue path (busy turn) or invalidate-and-reapply
 * (idle); the outcome arrives asynchronously as a `control_result` observer
 * frame, not as the return value here. This is fire-and-forget on the send side.
 */
export async function switchManagedAgentModel(
  pubkey: string,
  channelId: string,
  modelId: string,
): Promise<void> {
  await sendAgentObserverControl(pubkey, {
    type: "switch_model",
    channelId,
    modelId,
  });
}

/**
 * Ask a running agent to drain: stop admitting work, finish what it already
 * holds, then exit 0. The normative wire contract is
 * `crates/buzz-acp/src/drain.rs`; `crates/buzz-cli/src/agent_drain.rs` is the
 * other sender of the same frame.
 *
 * This resolves when the relay accepted the frame, which is *not* evidence the
 * agent honoured it — the agent's own answer arrives asynchronously as a
 * `control_result` frame. Use `awaitDrainAcknowledgement` rather than treating
 * this promise as an outcome.
 *
 * There is no owner check on this side, for the same reason the CLI has none:
 * which pubkey is the agent's owner is the agent's belief, not ours.
 * `handle_relay_observer_control_event` drops any control frame not signed by
 * the owner it resolved from its own NIP-OA auth tag, so authority is decided
 * where the truth lives. The frame is signed by this desktop's identity key
 * inside the Tauri `build_observer_control_event` command; no secret crosses
 * into the frontend.
 */
export async function drainAgent(
  pubkey: string,
  reason?: string,
): Promise<void> {
  const trimmed = reason?.trim();
  await sendAgentObserverControl(pubkey, {
    type: "drain",
    // Omitted rather than sent empty, matching the CLI sender: a blank reason
    // would put a field saying nothing into the agent's drain log line.
    ...(trimmed ? { reason: trimmed } : {}),
  });
}
