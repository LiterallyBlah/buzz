/**
 * Liveness stores: entries that exist only while something keeps saying so.
 *
 * Three surfaces in this app answer the same question with the same machine —
 * who is doing something *right now* — and each had hand-rolled it:
 *
 *   - channel typing indicators (kind 20002, 8s TTL),
 *   - observer-derived agent turns (turn_liveness frames, ~10s cadence),
 *   - NIP-PA project activity (45s staleness window).
 *
 * The mechanism is identical in all three: a frame arrives and puts an entry
 * under some scope key; later frames for the same scope keep it alive; a
 * terminal frame removes it; silence expires it; and whoever is watching gets
 * a reference-stable snapshot so `useSyncExternalStore` and `useMemo` can skip
 * work. Only the constants and the admission rules differ, so only those are
 * parameters here.
 *
 * Two layers, because the three consumers hold their state in genuinely
 * different places and forcing one shape on all of them would be a worse
 * abstraction than two small ones:
 *
 *   - `createLivenessMap` — the engine. Pure functions over an immutable
 *     entry record, returning the *same* object when nothing changed. Consumers
 *     that already keep their state in React (`useState`) or expose a pure
 *     reducer as their public API use this directly.
 *   - `createLivenessStore` — the engine plus the parts that need an owner:
 *     subscribers, cached snapshots, an observed-cadence estimator, and a prune
 *     timer that runs only while there is both something to prune and somebody
 *     watching. Module-level singletons use this.
 *
 * ## Value vs. liveness metadata
 *
 * A record separates *what the consumer surfaces* (`value`) from *what keeps it
 * alive* (`refreshedAt` / `baseAt`). That split is load-bearing: a heartbeat
 * frame moves only the metadata, so the state object keeps its identity and no
 * subscriber re-renders — which is exactly right, because a liveness ping says
 * "still true", not "something changed". Metadata is therefore mutated in
 * place; the value never is.
 */

/** One live scope, plus the bookkeeping that decides when it stops being one. */
export type LivenessRecord<Entry> = {
  /** Scope identity. Unique across the whole map, groups included. */
  readonly key: string;
  /**
   * Independent liveness domain. Cadence is observed per group and the prune
   * pause engages per group, because one agent's frame stream going quiet says
   * nothing about another's. Consumers with a single stream leave it `""`.
   */
  readonly group: string;
  /** What the consumer renders. Replaced wholesale by an accepted frame. */
  readonly value: Entry;
  /** Local-clock ms when this scope first went live; survives refreshes. */
  readonly firstSeenAt: number;
  /** Local-clock ms of the most recent frame accepted for this scope. */
  refreshedAt: number;
  /**
   * Clock the expiry window is measured from. Equal to `refreshedAt` unless the
   * frame carried its own timestamp, in which case it is the *earlier* of the
   * two: a producer that stamps a frame is making a claim about when it was
   * true, and believing a claim further into the future than our own clock
   * would extend a badge past its evidence.
   */
  baseAt: number;
};

/** The whole map. Structural changes are copy-on-write; refreshes are not. */
export type LivenessState<Entry> = Readonly<
  Record<string, LivenessRecord<Entry>>
>;

/** What a frame contributes beyond its value. */
export type LivenessFrame = {
  /** Local clock at ingest. Defaults to `Date.now()`. */
  nowMs?: number;
  /**
   * The frame's own timestamp in ms, when it carries one and its clock is
   * trustworthy enough to shorten (never lengthen) the window. Omit for
   * producers on a foreign clock — an agent host an hour behind would
   * otherwise expire every entry on arrival.
   */
  frameAtMs?: number;
};

export type LivenessMapConfig<Entry> = {
  /**
   * How long an entry stays believable after the frame that refreshed it —
   * a constant, or a per-record window (what the store uses to make expiry a
   * function of the cadence it has actually observed for that group).
   */
  ttlMs: number | ((record: LivenessRecord<Entry>) => number);
  /** Liveness domain for an entry. Defaults to a single shared domain. */
  groupOf?: (value: Entry) => string;
  /**
   * Refuse an incoming frame that must not displace the stored one — the rules
   * that are genuinely about *this* signal's ordering (an older announcement, a
   * weaker state) and cannot be expressed as "newest wins". Returning true
   * keeps the existing entry and the existing state reference.
   */
  supersede?: (existing: Entry, incoming: Entry) => boolean;
  /**
   * True when the incoming value says nothing new about what is on screen, so
   * the frame is a pure refresh: metadata moves, state identity does not.
   * Without it every accepted frame replaces the value and re-renders.
   */
  sameValue?: (existing: Entry, incoming: Entry) => boolean;
  /** Order of the surfaced snapshot. Defaults to insertion order. */
  compare?: (a: LivenessRecord<Entry>, b: LivenessRecord<Entry>) => number;
};

export type LivenessMap<Entry> = ReturnType<typeof createLivenessMap<Entry>>;

/**
 * The pure engine. Every operation returns the same state object when it
 * changed nothing, so a React consumer can hand the result straight back to
 * `setState` and get a bail-out instead of a render.
 */
export function createLivenessMap<Entry>(config: LivenessMapConfig<Entry>) {
  const { ttlMs, groupOf, supersede, sameValue, compare } = config;
  const empty: LivenessState<Entry> = Object.freeze({});

  function windowFor(record: LivenessRecord<Entry>): number {
    return typeof ttlMs === "function" ? ttlMs(record) : ttlMs;
  }

  /** Local-clock instant at which this entry stops being believable. */
  function expiresAt(record: LivenessRecord<Entry>): number {
    return record.baseAt + windowFor(record);
  }

  /** Live means strictly before the deadline: at the bound it is already gone. */
  function isLive(record: LivenessRecord<Entry>, nowMs: number): boolean {
    return nowMs < expiresAt(record);
  }

  function sortedRecords(state: LivenessState<Entry>): LivenessRecord<Entry>[] {
    const records = Object.values(state);
    return compare ? records.sort(compare) : records;
  }

  return {
    /** The canonical empty state. Shared, so `empty === empty` holds. */
    empty,
    expiresAt,
    isLive,

    size(state: LivenessState<Entry>): number {
      return Object.keys(state).length;
    },

    get(state: LivenessState<Entry>, key: string): Entry | undefined {
      return state[key]?.value;
    },

    record(
      state: LivenessState<Entry>,
      key: string,
    ): LivenessRecord<Entry> | undefined {
      return state[key];
    },

    /** Every record, ordered. Includes entries that are already past due. */
    records: sortedRecords,

    /** Every value, ordered. Expiry is the prune sweep's job, not this one's. */
    list(state: LivenessState<Entry>): Entry[] {
      return sortedRecords(state).map((record) => record.value);
    },

    /**
     * The values still believable at `nowMs`, ordered. For consumers that
     * evaluate staleness at read time instead of pruning it away — a view whose
     * clock ticks does not need the state to change to stop showing an entry.
     */
    live(state: LivenessState<Entry>, nowMs: number = Date.now()): Entry[] {
      return sortedRecords(state)
        .filter((record) => isLive(record, nowMs))
        .map((record) => record.value);
    },

    /**
     * Put `value` under `key`, or refuse it. Returns the same state when the
     * supersede rule refuses the frame, and when `sameValue` says the frame
     * repeats what is already stored (metadata still moves — that is the point
     * of a heartbeat).
     */
    upsert(
      state: LivenessState<Entry>,
      key: string,
      value: Entry,
      frame: LivenessFrame = {},
    ): LivenessState<Entry> {
      const nowMs = frame.nowMs ?? Date.now();
      const baseAt =
        frame.frameAtMs === undefined
          ? nowMs
          : Math.min(nowMs, frame.frameAtMs);
      const existing = state[key];

      if (existing) {
        if (supersede?.(existing.value, value)) return state;
        // A refresh never walks the window backwards: an out-of-order frame
        // that survived the supersede rule must not shorten a live entry.
        existing.refreshedAt = Math.max(existing.refreshedAt, nowMs);
        existing.baseAt = Math.max(existing.baseAt, baseAt);
        if (sameValue?.(existing.value, value)) return state;
        return {
          ...state,
          [key]: {
            key,
            group: existing.group,
            value,
            firstSeenAt: existing.firstSeenAt,
            refreshedAt: existing.refreshedAt,
            baseAt: existing.baseAt,
          },
        };
      }

      return {
        ...state,
        [key]: {
          key,
          group: groupOf?.(value) ?? "",
          value,
          firstSeenAt: nowMs,
          refreshedAt: nowMs,
          baseAt,
        },
      };
    },

    /**
     * Keep an existing entry alive without changing what it says. Reports
     * whether the scope was still tracked; the state reference never moves,
     * because nothing a subscriber can see has changed.
     */
    refresh(
      state: LivenessState<Entry>,
      key: string,
      frame: LivenessFrame = {},
    ): boolean {
      const record = state[key];
      if (!record) return false;
      const nowMs = frame.nowMs ?? Date.now();
      const baseAt =
        frame.frameAtMs === undefined
          ? nowMs
          : Math.min(nowMs, frame.frameAtMs);
      record.refreshedAt = Math.max(record.refreshedAt, nowMs);
      record.baseAt = Math.max(record.baseAt, baseAt);
      return true;
    },

    /**
     * Terminal frame for one scope. `guard` is how a consumer says "only the
     * entry this frame is actually about" — a late terminal for a turn that has
     * already been replaced must not clear the turn now running.
     */
    drop(
      state: LivenessState<Entry>,
      key: string,
      guard?: (existing: Entry) => boolean,
    ): LivenessState<Entry> {
      const record = state[key];
      if (!record) return state;
      if (guard && !guard(record.value)) return state;
      const next = { ...state };
      delete next[key];
      return next;
    },

    /**
     * Remove up to `limit` records matching `predicate`, reporting which went.
     * The escape hatch for terminal frames that identify their scope indirectly
     * (by channel rather than by turn), where the caller needs the removed
     * identity back to record it.
     */
    take(
      state: LivenessState<Entry>,
      predicate: (record: LivenessRecord<Entry>) => boolean,
      limit = Number.POSITIVE_INFINITY,
    ): { state: LivenessState<Entry>; taken: LivenessRecord<Entry>[] } {
      const taken: LivenessRecord<Entry>[] = [];
      for (const record of Object.values(state)) {
        if (taken.length >= limit) break;
        if (predicate(record)) taken.push(record);
      }
      if (taken.length === 0) return { state, taken };
      const next = { ...state };
      for (const record of taken) delete next[record.key];
      return { state: next, taken };
    },

    /**
     * Drop everything past its deadline. `skipGroup` is the pause heuristic's
     * veto: a group whose whole stream is missing is not a group whose work
     * finished, and pruning it would wipe live badges over a transport hiccup.
     */
    prune(
      state: LivenessState<Entry>,
      nowMs: number = Date.now(),
      skipGroup?: (group: string) => boolean,
    ): LivenessState<Entry> {
      let next: Record<string, LivenessRecord<Entry>> | null = null;
      for (const record of Object.values(state)) {
        if (isLive(record, nowMs)) continue;
        if (skipGroup?.(record.group)) continue;
        if (!next) next = { ...state };
        delete next[record.key];
      }
      return next ?? state;
    },
  };
}

// ---------------------------------------------------------------------------
// Cadence
// ---------------------------------------------------------------------------

/**
 * How often the producer is expected to say "still here".
 *
 * `fixed` is for producers whose interval is part of the protocol (a typing
 * indicator's TTL, NIP-PA's refresh window) — the number is a contract, not an
 * observation.
 *
 * `adaptive` is for producers whose interval is *configuration*. The agent
 * harness emits `turn_liveness` every `BUZZ_ACP_TURN_LIVENESS_SECS`, which
 * deployments change; a window derived from an assumed 10s is simply wrong at
 * 15s, where one dropped ping opens a 30s hole and wipes a badge mid-turn. So
 * the cadence is measured from the gaps between the frames that actually
 * arrive, clamped: never tighter than `floorMs` (so a burst of frames cannot
 * shrink the window below what the build has always tolerated) and never looser
 * than `ceilingMs` (past which the producer is not heartbeating at all, and the
 * pause backstop — not an ever-growing expiry window — is the right mechanism).
 */
export type LivenessCadence =
  | { kind: "fixed"; intervalMs: number }
  | {
      kind: "adaptive";
      floorMs: number;
      ceilingMs: number;
      /** Recent gaps the estimate is taken over. Defaults to 5. */
      sampleWindow?: number;
    };

const DEFAULT_CADENCE_SAMPLES = 5;

type CadenceSamples = { lastFrameAt: number | null; gaps: number[] };

/**
 * Median rather than mean or max, because the two failure modes are asymmetric.
 * A single dropped ping doubles exactly one gap; a mean drags the estimate up
 * for the whole window and a max keeps a dead entry alive for multiples of the
 * real cadence. A median ignores one outlier in a window of five and moves only
 * once the producer's actual interval has moved — which is the thing worth
 * tracking.
 */
function estimateCadence(
  gaps: readonly number[],
  floorMs: number,
  ceilingMs: number,
): number {
  if (gaps.length === 0) return floorMs;
  const sorted = [...gaps].sort((a, b) => a - b);
  const median = sorted[Math.floor(sorted.length / 2)];
  return Math.min(Math.max(median, floorMs), ceilingMs);
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

export type LivenessPausePolicy = {
  /**
   * A group is treated as "stream down" once *every* entry in it has been
   * silent for this multiple of the observed cadence. Below the expiry
   * multiplier, so the pause engages before the sweep would wipe anything.
   */
  gapMultiplier: number;
  /** Silence past this is not a hiccup; the producer is gone. */
  maxMs: number;
};

export type LivenessStoreConfig<Entry> = Omit<
  LivenessMapConfig<Entry>,
  "ttlMs"
> & {
  cadence: LivenessCadence;
  /** Expiry window = observed cadence × this. */
  expiryMultiplier: number;
  /** Optional group-wide veto on pruning. Omit for producers with no stream. */
  pause?: LivenessPausePolicy;
  /** Sweep period. Only ever running while it has work and an audience. */
  pruneIntervalMs: number;
  /**
   * A group's entries changed structurally. Consumers derive their own cached
   * projections (per-agent summaries, cross-agent aggregates) and this is where
   * they drop them; the store cannot know what they built.
   */
  onInvalidate?: (group: string) => void;
};

/**
 * A liveness store with an owner: subscribers, cached snapshots, an observed
 * cadence per group, and a prune timer.
 *
 * Mutators report whether anything changed but never broadcast on their own —
 * one inbound frame routinely touches several entries and should produce one
 * notification, and only the caller knows where that boundary is. The sweep is
 * the exception: it has no caller, so it notifies itself.
 */
export function createLivenessStore<Entry>(config: LivenessStoreConfig<Entry>) {
  const { cadence, expiryMultiplier, pause, pruneIntervalMs, onInvalidate } =
    config;

  const cadenceByGroup = new Map<string, CadenceSamples>();

  function cadenceMs(group: string): number {
    if (cadence.kind === "fixed") return cadence.intervalMs;
    const samples = cadenceByGroup.get(group);
    return estimateCadence(
      samples?.gaps ?? [],
      cadence.floorMs,
      cadence.ceilingMs,
    );
  }

  const map = createLivenessMap<Entry>({
    ttlMs: (record) => cadenceMs(record.group) * expiryMultiplier,
    groupOf: config.groupOf,
    supersede: config.supersede,
    sameValue: config.sameValue,
    compare: config.compare,
  });

  let state = map.empty;
  const listeners = new Set<() => void>();
  let timer: ReturnType<typeof setInterval> | null = null;

  // Snapshot caches. React reads a snapshot before it subscribes, so these are
  // maintained whether or not anyone is listening.
  const EMPTY_VALUES: readonly Entry[] = Object.freeze([]);
  const EMPTY_RECORDS: readonly LivenessRecord<Entry>[] = Object.freeze([]);
  let recordsCache: readonly LivenessRecord<Entry>[] | null = null;
  let listCache: readonly Entry[] | null = null;
  const groupRecordsCache = new Map<string, readonly LivenessRecord<Entry>[]>();
  const groupListCache = new Map<string, readonly Entry[]>();

  function invalidate(groups: Iterable<string>) {
    recordsCache = null;
    listCache = null;
    for (const group of groups) {
      groupRecordsCache.delete(group);
      groupListCache.delete(group);
      onInvalidate?.(group);
    }
  }

  function notify() {
    for (const listener of listeners) listener();
  }

  /**
   * The sweep runs only while there is something to prune *and* somebody to
   * tell. An idle app with no live entries holds no timer at all, and the
   * timer restarts the moment the first entry lands.
   */
  function syncTimer() {
    const wanted = listeners.size > 0 && map.size(state) > 0;
    if (wanted && !timer) {
      timer = setInterval(() => {
        if (sweep()) notify();
      }, pruneIntervalMs);
    } else if (!wanted && timer) {
      clearInterval(timer);
      timer = null;
    }
  }

  /** True when the whole group has been silent long enough to look like a
   * stream outage rather than finished work — but not so long that "the
   * producer died without unwinding" stops being the better reading. */
  function isPaused(group: string, nowMs: number): boolean {
    if (!pause) return false;
    let newest: number | null = null;
    for (const record of Object.values(state)) {
      if (record.group !== group) continue;
      if (newest === null || record.refreshedAt > newest) {
        newest = record.refreshedAt;
      }
    }
    // An empty group has no silence to interpret — there is nothing in it to
    // save from the sweep.
    if (newest === null) return false;
    const silentFor = nowMs - newest;
    return (
      silentFor > cadenceMs(group) * pause.gapMultiplier &&
      silentFor < pause.maxMs
    );
  }

  function recordsInGroup(group: string): readonly LivenessRecord<Entry>[] {
    const cached = groupRecordsCache.get(group);
    if (cached) return cached;
    const result = map
      .records(state)
      .filter((record) => record.group === group);
    if (result.length === 0) return EMPTY_RECORDS;
    groupRecordsCache.set(group, result);
    return result;
  }

  function sweep(nowMs: number = Date.now()): boolean {
    const pausedGroups = new Map<string, boolean>();
    const next = map.prune(state, nowMs, (group) => {
      let paused = pausedGroups.get(group);
      if (paused === undefined) {
        paused = isPaused(group, nowMs);
        pausedGroups.set(group, paused);
      }
      return paused;
    });
    if (next === state) return false;
    const dropped = new Set<string>();
    for (const record of Object.values(state)) {
      if (!(record.key in next)) dropped.add(record.group);
    }
    state = next;
    invalidate(dropped);
    syncTimer();
    return true;
  }

  return {
    /** `useSyncExternalStore`-shaped subscription. */
    subscribe(listener: () => void): () => void {
      listeners.add(listener);
      syncTimer();
      return () => {
        listeners.delete(listener);
        syncTimer();
      };
    },

    /** Broadcast without a state change — for consumer-owned derived data. */
    notify,

    upsert(key: string, value: Entry, frame: LivenessFrame = {}): boolean {
      const next = map.upsert(state, key, value, frame);
      if (next === state) return false;
      const group = next[key].group;
      state = next;
      invalidate([group]);
      syncTimer();
      return true;
    },

    refresh(key: string, frame: LivenessFrame = {}): boolean {
      return map.refresh(state, key, frame);
    },

    drop(key: string, guard?: (existing: Entry) => boolean): boolean {
      const group = state[key]?.group;
      const next = map.drop(state, key, guard);
      if (next === state) return false;
      state = next;
      invalidate(group === undefined ? [] : [group]);
      syncTimer();
      return true;
    },

    take(
      predicate: (record: LivenessRecord<Entry>) => boolean,
      limit?: number,
    ): LivenessRecord<Entry>[] {
      const result = map.take(state, predicate, limit);
      if (result.taken.length === 0) return result.taken;
      state = result.state;
      invalidate(new Set(result.taken.map((record) => record.group)));
      syncTimer();
      return result.taken;
    },

    /** Drop a whole domain — the producer is known to be gone (a restart). */
    clearGroup(group: string): boolean {
      const taken = map.take(state, (record) => record.group === group);
      if (taken.taken.length === 0) return false;
      state = taken.state;
      cadenceByGroup.delete(group);
      invalidate([group]);
      syncTimer();
      return true;
    },

    clear(): void {
      const groups = new Set<string>();
      for (const record of Object.values(state)) groups.add(record.group);
      state = map.empty;
      cadenceByGroup.clear();
      invalidate(groups);
      syncTimer();
    },

    /**
     * Record that a heartbeat frame arrived for `group` at `frameAtMs` (the
     * producer's own clock, so the measured gap is the producer's interval and
     * not our delivery jitter). Only strictly-positive gaps count: frames that
     * share a timestamp are one emission, not a zero-length cadence.
     */
    observeCadence(group: string, frameAtMs: number | null): void {
      if (cadence.kind !== "adaptive") return;
      if (frameAtMs === null || !Number.isFinite(frameAtMs)) return;
      const samples = cadenceByGroup.get(group) ?? {
        lastFrameAt: null,
        gaps: [],
      };
      if (samples.lastFrameAt !== null && frameAtMs > samples.lastFrameAt) {
        samples.gaps.push(frameAtMs - samples.lastFrameAt);
        const keep = cadence.sampleWindow ?? DEFAULT_CADENCE_SAMPLES;
        if (samples.gaps.length > keep) {
          samples.gaps.splice(0, samples.gaps.length - keep);
        }
      }
      if (samples.lastFrameAt === null || frameAtMs > samples.lastFrameAt) {
        samples.lastFrameAt = frameAtMs;
      }
      cadenceByGroup.set(group, samples);
    },

    /** The cadence currently believed for a group, clamped to its bounds. */
    cadenceMs,

    /** How long silence is tolerated for a group before its entries expire. */
    expiryMs(group: string): number {
      return cadenceMs(group) * expiryMultiplier;
    },

    size(): number {
      return map.size(state);
    },

    groupSize(group: string): number {
      let count = 0;
      for (const record of Object.values(state)) {
        if (record.group === group) count += 1;
      }
      return count;
    },

    get(key: string): Entry | undefined {
      return map.get(state, key);
    },

    has(key: string): boolean {
      return map.record(state, key) !== undefined;
    },

    /** Reference-stable until the entry set changes. */
    records(): readonly LivenessRecord<Entry>[] {
      if (!recordsCache) recordsCache = map.records(state);
      return recordsCache;
    },

    recordsInGroup,

    list(): readonly Entry[] {
      if (!listCache) {
        listCache = map.size(state) === 0 ? EMPTY_VALUES : map.list(state);
      }
      return listCache;
    },

    listGroup(group: string): readonly Entry[] {
      const cached = groupListCache.get(group);
      if (cached) return cached;
      const records = recordsInGroup(group);
      if (records.length === 0) return EMPTY_VALUES;
      const result = records.map((record) => record.value);
      groupListCache.set(group, result);
      return result;
    },

    /**
     * Run the sweep now, off-schedule. Returns whether anything was dropped and,
     * like the other mutators, leaves broadcasting to the caller — only the
     * timer-driven sweep notifies on its own, because it has no caller.
     */
    sweep,

    /** Whether a prune timer is currently held. Diagnostics and tests. */
    isSweeping(): boolean {
      return timer !== null;
    },
  };
}

export type LivenessStore<Entry> = ReturnType<
  typeof createLivenessStore<Entry>
>;
