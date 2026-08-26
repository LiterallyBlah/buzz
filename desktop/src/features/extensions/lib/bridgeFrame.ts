/**
 * Strict validation of an inbound §2 request frame.
 *
 * The port carries **structured clone**, not JSON. That is the seam this
 * module exists for: structured clone transports `ArrayBuffer`, typed arrays,
 * `Map`, `Set`, `Blob`, `File`, `MessagePort`, `BigInt`, cycles and exotic
 * objects, and `JSON.stringify` renders most of them as `{}`. A bound measured
 * on `JSON.stringify(...).length` is therefore blind to an 8 MiB `ArrayBuffer`
 * — it reads as two characters.
 *
 * So the frame is validated against an explicit allowlist of JSON-compatible
 * types rather than by serialising it and measuring the result.
 *
 * # Iterative, not recursive
 *
 * Traversal uses an explicit stack. A recursive validator would trade a size
 * DoS for a stack DoS — a frame nested 100 000 deep would overflow before any
 * depth limit was consulted, and the overflow is not catchable in a way that
 * yields a clean refusal.
 *
 * # Bytes, not code units
 *
 * Size is accumulated as **UTF-8 bytes** of the JSON the frame would encode
 * to. `String.length` counts UTF-16 code units, which undercounts every
 * multibyte character — a frame of 64 K emoji is ~256 KiB on the wire but
 * measures 128 K by `.length`.
 *
 * # Repeated references are rejected
 *
 * A shared subtree is legal JSON, but `JSON.stringify` expands it once per
 * reference. Ten references to one 1 MiB subtree is 10 MiB encoded while the
 * node count stays small, so a byte budget that counted nodes once would be
 * unsound. Rejecting any container seen twice closes that amplification and
 * gives cycle detection for free.
 */

/** Wire version this client speaks (§2). */
export const WIRE_VERSION = 1;

/**
 * Limits. Each is far above any legitimate caller and far below anything that
 * makes the host do unbounded work.
 */
/** Total UTF-8 bytes of the frame's JSON encoding. */
export const MAX_FRAME_BYTES = 64 * 1024;
/** Deepest container nesting. */
export const MAX_DEPTH = 32;
/** Total values visited, so a wide frame is bounded as well as a deep one. */
export const MAX_NODES = 10_000;
/** Longest single string, in UTF-8 bytes. */
export const MAX_STRING_BYTES = 16 * 1024;
/** Longest `method`, in UTF-8 bytes. Matches the Rust-side cap. */
export const MAX_METHOD_BYTES = 64;
/** Largest `v` that can cross into Rust's `u32`. */
const MAX_U32 = 0xff_ff_ff_ff;

/** §8 codes this module can produce. */
const INVALID_PARAMS = "invalid_params";
/**
 * §2: "The host MUST reject a request whose `v` it does not support with
 * `unsupported_version`."
 *
 * Every numeric `v` that is not the supported integer lands here, whether or
 * not Rust could have received it — `1.5` is a version this host does not
 * support just as `2` is. Splitting "numeric but unrepresentable" from
 * "numeric but unsupported" would hand a client a distinction it cannot act
 * on, where "send `v: 1` or get `unsupported_version`" is one rule it can.
 */
const UNSUPPORTED_VERSION = "unsupported_version";

/**
 * Exact v4-shaped UUID, as `uuid::Uuid::new_v4().to_string()` produces and as
 * §2's `"id": "<uuid>"` requires. Hex is accepted in either case (RFC 4122
 * permits both on input); everything else — wrong length, missing or extra
 * hyphen, non-hex, surrounding whitespace — is not a UUID.
 */
const UUID_PATTERN =
  /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;

export function isUuid(value: string): boolean {
  return UUID_PATTERN.test(value);
}

export type RequestFrame = {
  id: string;
  v: number;
  method: string;
  params?: unknown;
};

export type FrameCheck =
  | { kind: "ok"; frame: RequestFrame }
  | { kind: "refuse"; id: string; code: string; message: string }
  | { kind: "drop" };

/** Top-level fields §2 defines. Anything else makes the frame malformed. */
const FRAME_FIELDS = new Set(["id", "v", "method", "params"]);

/**
 * UTF-8 byte length, computed without allocating an encoded copy.
 *
 * A lone surrogate encodes as U+FFFD (3 bytes) rather than throwing, which is
 * what `TextEncoder` does, so the count stays an upper bound on the real
 * encoding in every case.
 */
export function utf8Length(text: string): number {
  let bytes = 0;
  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index);
    if (code < 0x80) {
      bytes += 1;
    } else if (code < 0x800) {
      bytes += 2;
    } else if (code >= 0xd800 && code <= 0xdbff) {
      const next = index + 1 < text.length ? text.charCodeAt(index + 1) : 0;
      if (next >= 0xdc00 && next <= 0xdfff) {
        bytes += 4;
        index += 1;
      } else {
        bytes += 3; // lone high surrogate → replacement character
      }
    } else {
      bytes += 3;
    }
  }
  return bytes;
}

/** Is this a plain object — `{}` or `Object.create(null)` — and nothing else? */
function isPlainObject(value: object): boolean {
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}

type Budget = {
  bytes: number;
  nodes: number;
};

const TOO_MANY_VALUES = "request frame has too many values";
const EXCEEDS_LIMITS = "request frame exceeds the wire limits";
const NOT_ALLOWED = "request frame carries a value the protocol does not allow";

/**
 * Walk a value, admitting only JSON-compatible types and charging it against
 * the budget. Returns an error string, or null if the value is acceptable.
 *
 * The stack holds `[value, depth]`. Containers already visited are held in a
 * `WeakSet` so a cycle or a repeated reference is refused rather than expanded.
 */
function checkValue(root: unknown, budget: Budget): string | null {
  const seen = new WeakSet<object>();
  const stack: Array<[unknown, number]> = [];

  /**
   * Charge a value when it is **enqueued**, not when it is visited.
   *
   * Charging on visit lets a container push all of its children before the
   * ceiling is next consulted, so a million-element array allocates a
   * million stack entries and *then* gets refused. Charging on enqueue makes
   * the node budget bound the stack itself.
   */
  const enqueue = (value: unknown, depth: number): boolean => {
    budget.nodes += 1;
    // The **node budget fires** — but through the array capacity precheck
    // below, not through this branch. A depth-13 binary tree encoding to
    // 41 051 bytes, well inside the byte cap, is refused with "too many
    // values" by that precheck.
    //
    // *This overflow branch* is what is unreachable at the current constants:
    // an array long enough to matter is refused on declared length before any
    // element is enqueued, and an object entry costs at least nine encoded
    // bytes against a budget of ~6.5 per node, so the byte cap arrives first.
    // No fixture can isolate the branch, which is why the mutation battery
    // does not claim one. It is kept because it is the bound that stays
    // correct if MAX_NODES, MAX_FRAME_BYTES or the per-entry costs are
    // retuned.
    if (budget.nodes > MAX_NODES) {
      return false;
    }
    stack.push([value, depth]);
    return true;
  };

  if (!enqueue(root, 0)) {
    return TOO_MANY_VALUES;
  }

  while (stack.length > 0) {
    const entry = stack.pop();
    if (entry === undefined) {
      break;
    }
    const [value, depth] = entry;

    if (depth > MAX_DEPTH) {
      return "request frame is nested too deeply";
    }

    if (value === null) {
      budget.bytes += 4; // "null"
    } else if (typeof value === "boolean") {
      budget.bytes += 5;
    } else if (typeof value === "number") {
      if (!Number.isFinite(value)) {
        // NaN and the infinities have no JSON form; `JSON.stringify` writes
        // `null`, which would silently change the value the host sees.
        return "request frame carries a non-finite number";
      }
      budget.bytes += 24; // generous upper bound on a JSON number
    } else if (typeof value === "string") {
      const size = utf8Length(value);
      if (size > MAX_STRING_BYTES) {
        return "request frame carries an oversized string";
      }
      budget.bytes += size + 2; // quotes
    } else if (typeof value === "object") {
      const container = value as object;
      if (seen.has(container)) {
        return "request frame repeats or cycles a value";
      }
      seen.add(container);

      if (Array.isArray(container)) {
        // Refuse on the declared length before touching a single element: an
        // enormous array must cost one comparison, not one push per element.
        if (budget.nodes + container.length > MAX_NODES) {
          return TOO_MANY_VALUES;
        }
        budget.bytes += 2 + Math.max(0, container.length - 1); // [] + commas
        for (const item of container) {
          if (!enqueue(item, depth + 1)) {
            return TOO_MANY_VALUES;
          }
        }
      } else if (isPlainObject(container)) {
        // `for...in` rather than `Object.keys`, which would materialise an
        // array of every key before any budget could refuse them. The
        // prototype is already known to be `Object.prototype` or null, so an
        // own-property filter is all the guard this needs.
        budget.bytes += 2; // {}
        let keys = 0;
        for (const key in container) {
          if (!Object.hasOwn(container, key)) {
            continue;
          }
          keys += 1;
          const size = utf8Length(key);
          if (size > MAX_STRING_BYTES) {
            return "request frame carries an oversized key";
          }
          budget.bytes += size + 3 + (keys > 1 ? 1 : 0); // quotes, colon, comma
          if (budget.bytes > MAX_FRAME_BYTES) {
            return EXCEEDS_LIMITS;
          }
          if (
            !enqueue((container as Record<string, unknown>)[key], depth + 1)
          ) {
            return TOO_MANY_VALUES;
          }
        }
      } else {
        // Everything structured clone carries that JSON cannot represent:
        // ArrayBuffer, typed arrays, DataView, Map, Set, Blob, File, Date,
        // RegExp, Error, MessagePort and any other exotic object.
        return NOT_ALLOWED;
      }
    } else {
      // undefined, bigint, function, symbol.
      return NOT_ALLOWED;
    }

    if (budget.bytes > MAX_FRAME_BYTES) {
      return EXCEEDS_LIMITS;
    }
  }

  return null;
}

/**
 * Classify an inbound frame without letting it reach a handler.
 *
 * `id` is checked first and decides whether a reply is possible at all: a
 * missing, non-string or non-UUID `id` leaves nothing to correlate against, so
 * the frame is dropped in silence. Everything past that point is correlatable
 * and therefore answered (§9: in-flight requests settle rather than dangle).
 */
export function checkFrame(data: unknown): FrameCheck {
  if (typeof data !== "object" || data === null || Array.isArray(data)) {
    return { kind: "drop" };
  }
  if (!isPlainObject(data)) {
    return { kind: "drop" };
  }
  const frame = data as Partial<RequestFrame>;
  if (typeof frame.id !== "string" || !isUuid(frame.id)) {
    return { kind: "drop" };
  }
  const id = frame.id;
  const refuseWith = (code: string, message: string): FrameCheck => ({
    kind: "refuse",
    id,
    code,
    message,
  });
  const refuse = (message: string): FrameCheck =>
    refuseWith(INVALID_PARAMS, message);

  // `for...in` with a running count, not `Object.keys`: a frame carrying a
  // million top-level keys would otherwise materialise a million-element array
  // before anything could refuse it. This refuses on the fifth key.
  let fields = 0;
  for (const key in data) {
    if (!Object.hasOwn(data, key)) {
      continue;
    }
    fields += 1;
    if (fields > FRAME_FIELDS.size || !FRAME_FIELDS.has(key)) {
      return refuse("request frame carries an unrecognised field");
    }
  }

  if (typeof frame.v !== "number") {
    // Absent or wrong-typed is a malformed frame, not a version question:
    // `v: "1"` is a shape error and says nothing about which versions exist.
    return refuse("request frame has no usable version");
  }
  // A number, but not one `u32` can carry. Settled here rather than allowed to
  // fail deserialisation and arrive as `internal` — but under the *same* code a
  // representable-yet-unsupported `v` gets from Rust, so a caller sees one rule
  // rather than a split it cannot act on. Representable integers are forwarded,
  // and Rust decides which of those it supports.
  if (!Number.isSafeInteger(frame.v) || frame.v < 0 || frame.v > MAX_U32) {
    return refuseWith(
      UNSUPPORTED_VERSION,
      `this host speaks bridge version ${WIRE_VERSION}`,
    );
  }

  if (typeof frame.method !== "string") {
    return refuse("request frame has no usable method");
  }
  const methodBytes = utf8Length(frame.method);
  if (frame.method.length === 0 || methodBytes > MAX_METHOD_BYTES) {
    return refuse("request frame has no usable method");
  }

  // The envelope scalars are settled first — each is O(1) or bounded by the
  // method cap — so a frame with a bad version is refused without walking a
  // large `params` tree at all. It also keeps `v` out of the traversal's
  // non-finite check, which would otherwise claim `NaN` as a malformed value
  // before the version rule could call it an unsupported one.
  const budget: Budget = { bytes: 0, nodes: 0 };
  const problem = checkValue(data, budget);
  if (problem !== null) {
    return refuse(problem);
  }

  // The walk charges **raw** UTF-8 bytes, but JSON escapes: a NUL is one raw
  // byte and six encoded ones (`\u0000`), and quotes and backslashes double.
  // So the running estimate can sit under the cap while the real encoding is
  // over it — 11 000 NULs measured 11 000 raw and 66 085 encoded against a
  // 65 536 limit. Only measuring the actual encoding is exact.
  //
  // Affordable precisely because the walk ran first: nodes, depth, per-string
  // size and raw bytes are all bounded by then, so what reaches `stringify`
  // is at most the frame budget plus one string's overshoot, and escaping
  // expands that by at most six. Doing this before the walk would be the DoS
  // the walk exists to prevent.
  let encoded: number;
  try {
    encoded = new TextEncoder().encode(JSON.stringify(data)).byteLength;
  } catch {
    // Unreachable for a value the walk admitted, which is why it refuses
    // rather than rethrowing: an exception here would mean the walk let
    // something through, and failing closed is the safer answer.
    return refuse(EXCEEDS_LIMITS);
  }
  if (encoded > MAX_FRAME_BYTES) {
    return refuse(EXCEEDS_LIMITS);
  }

  return {
    kind: "ok",
    frame: { id, v: frame.v, method: frame.method, params: frame.params },
  };
}
