/**
 * Extension-side half of the internal stream transport.
 *
 * It exposes only the public `{sub,kind,...}` frames to the extension and sends
 * an ACK only after the MessagePort task dequeued the batch, every frame passed
 * structural adoption, and the caller accepted each frame without throwing.
 * It holds no authority; missing/invalid ACKs make the host fail closed.
 */

export type PublicStreamFrame = {
  sub: string;
  kind: "event" | "eose" | "closed";
  event?: unknown;
  reason?: string;
};

type Batch = {
  buzz: "stream-batch";
  generation: string;
  sub: string;
  seq: number;
  token: string;
  frames: PublicStreamFrame[];
  frameCount: number;
  encodedBytes: number;
  terminal: boolean;
};

function isBatch(value: unknown): value is Batch {
  if (typeof value !== "object" || value === null) return false;
  const batch = value as Record<string, unknown>;
  if (
    batch.buzz !== "stream-batch" ||
    typeof batch.generation !== "string" ||
    typeof batch.sub !== "string" ||
    typeof batch.seq !== "number" ||
    !Number.isSafeInteger(batch.seq) ||
    batch.seq < 1 ||
    typeof batch.token !== "string" ||
    !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
      batch.token,
    ) ||
    !Array.isArray(batch.frames) ||
    batch.frames.length !== batch.frameCount ||
    typeof batch.encodedBytes !== "number" ||
    !Number.isSafeInteger(batch.encodedBytes) ||
    typeof batch.terminal !== "boolean"
  ) {
    return false;
  }
  return batch.frames.every((frame) => {
    if (typeof frame !== "object" || frame === null) return false;
    const shaped = frame as Record<string, unknown>;
    return (
      shaped.sub === batch.sub &&
      (shaped.kind === "event" ||
        shaped.kind === "eose" ||
        shaped.kind === "closed")
    );
  });
}

export function installBridgeStreamClient(
  port: MessagePort,
  onFrame: (frame: PublicStreamFrame) => void,
  onMessage?: (message: unknown) => void,
): () => void {
  const nextSeq = new Map<string, number>();
  const closed = new Set<string>();
  const handler = (event: MessageEvent) => {
    if (!isBatch(event.data)) {
      onMessage?.(event.data);
      return;
    }
    const batch = event.data;
    const expected = nextSeq.get(batch.sub) ?? 1;
    if (closed.has(batch.sub) || batch.seq !== expected) {
      return;
    }
    const encodedBytes = batch.frames.reduce(
      (total, frame) =>
        total + new TextEncoder().encode(JSON.stringify(frame)).byteLength,
      0,
    );
    if (encodedBytes !== batch.encodedBytes) {
      return;
    }
    try {
      for (const frame of batch.frames) {
        onFrame(frame);
        if (frame.kind === "closed") closed.add(batch.sub);
      }
    } catch {
      // Adoption failed. Withhold credit; the host timeout closes the stream.
      return;
    }
    nextSeq.set(batch.sub, expected + 1);
    if (!batch.terminal) {
      port.postMessage({
        buzz: "stream-ack",
        generation: batch.generation,
        sub: batch.sub,
        seq: batch.seq,
        token: batch.token,
        frameCount: batch.frameCount,
        encodedBytes: batch.encodedBytes,
      });
    }
  };
  port.addEventListener("message", handler);
  port.start();
  return () => port.removeEventListener("message", handler);
}
