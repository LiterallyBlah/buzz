//! Durable terminal-authentication dispositions.
//!
//! When a batch dies of a terminal authentication failure, the events that
//! produced it must never run again — not after a requeue, not after a process
//! restart, and not after the relay replays the same history on reconnect. A
//! purely in-memory disposition cannot promise that: the harness exits, the
//! desktop restarts it, the relay replays, and the same expired-credential
//! request is dispatched a second time.
//!
//! This module is that promise, and nothing more. It is a file-backed set of
//! Nostr event IDs, owned by the [`EventQueue`](crate::queue::EventQueue),
//! with an all-or-none commit. There is no service, no database, and no
//! background task.
//!
//! # Durability contract
//!
//! The commit is the linearisation point. A crash *before* it leaves the batch
//! untouched and retryable. A crash *after* it suppresses the request with no
//! user-visible notice — accepted deliberately, because a lost notice is
//! recoverable by the user re-sending, while a revived request against expired
//! credentials is not recoverable by anyone.
//!
//! # Single writer
//!
//! One managed runtime owns one identity file. The path is keyed by agent
//! pubkey precisely so two identities never collide; two runtimes for the
//! *same* identity are outside the supported configuration and would race.
//! [`TerminalAuthStore::load`] does not attempt to arbitrate that.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// On-disk schema version. A file carrying anything else fails startup rather
/// than being silently migrated or reset.
const SCHEMA_VERSION: u32 = 1;

/// Hard ceiling on stored dispositions.
///
/// There is deliberately no eviction and no rotation: every entry is a promise
/// that a specific event will never run again, and evicting one silently
/// breaks that promise. At capacity the harness refuses new dispositions and
/// stops, which is loud, recoverable, and cannot revive anything.
pub(crate) const MAX_TERMINAL_AUTH_IDS: usize = 12_000;

/// Directory name appended to the resolved state directory.
const STORE_DIR: &str = "terminal-auth";

/// Errors from loading or committing the store.
#[derive(Debug, thiserror::Error)]
pub enum TerminalAuthStoreError {
    #[error("terminal-auth store at {path} is unreadable: {source}")]
    Unreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("terminal-auth store at {path} is not valid JSON: {source}")]
    Corrupt {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("terminal-auth store at {path} has unsupported version {version} (expected {SCHEMA_VERSION})")]
    UnsupportedVersion { path: String, version: u32 },

    #[error("terminal-auth store at {path} contains an invalid event ID")]
    InvalidId { path: String },

    #[error(
        "terminal-auth store at {path} holds {count} dispositions, over the {MAX_TERMINAL_AUTH_IDS} limit"
    )]
    OverCapacity { path: String, count: usize },

    #[error("terminal-auth store at {path} could not be committed: {source}")]
    CommitFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// No store is attached, so no durable promise can be made.
    ///
    /// Treated exactly like a write failure by callers: no notice, no
    /// completion, stop. An in-memory-only disposition would look identical
    /// from the outside and revive on the next restart.
    #[error("no terminal-auth store is attached — cannot make a durable disposition")]
    NoStore,
}

/// The serialised form. `BTreeSet` gives deterministic ordering for free, so
/// two runs that disposed the same events produce byte-identical files.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    version: u32,
    terminal_auth_event_ids: BTreeSet<String>,
}

/// Returns `true` when `id` is a well-formed lowercase-hex Nostr event ID.
///
/// Uppercase hex is rejected rather than normalised: the harness only ever
/// writes `Event::id.to_hex()`, which is lowercase, so an uppercase entry means
/// the file was edited by something that does not share our invariants.
fn is_valid_event_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A durable, file-backed set of terminally-disposed event IDs.
#[derive(Debug)]
pub struct TerminalAuthStore {
    path: PathBuf,
    ids: BTreeSet<String>,
}

impl TerminalAuthStore {
    /// The identity-specific store path under `state_dir`.
    pub fn path_for(state_dir: &Path, agent_pubkey_hex: &str) -> PathBuf {
        state_dir
            .join(STORE_DIR)
            .join(format!("{agent_pubkey_hex}.json"))
    }

    /// Load and strictly validate the store for one agent identity.
    ///
    /// A missing file is an empty store — the ordinary first-run case. Anything
    /// else that cannot be read as a valid snapshot is an error: silently
    /// resetting would re-arm every event the previous run disposed of.
    pub fn load(state_dir: &Path, agent_pubkey_hex: &str) -> Result<Self, TerminalAuthStoreError> {
        let path = Self::path_for(state_dir, agent_pubkey_hex);
        let display = path.display().to_string();

        let raw = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    ids: BTreeSet::new(),
                });
            }
            Err(source) => {
                return Err(TerminalAuthStoreError::Unreadable {
                    path: display,
                    source,
                })
            }
        };

        let parsed: StoreFile =
            serde_json::from_slice(&raw).map_err(|source| TerminalAuthStoreError::Corrupt {
                path: display.clone(),
                source,
            })?;

        if parsed.version != SCHEMA_VERSION {
            return Err(TerminalAuthStoreError::UnsupportedVersion {
                path: display,
                version: parsed.version,
            });
        }
        if parsed
            .terminal_auth_event_ids
            .iter()
            .any(|id| !is_valid_event_id(id))
        {
            return Err(TerminalAuthStoreError::InvalidId { path: display });
        }
        if parsed.terminal_auth_event_ids.len() > MAX_TERMINAL_AUTH_IDS {
            return Err(TerminalAuthStoreError::OverCapacity {
                path: display,
                count: parsed.terminal_auth_event_ids.len(),
            });
        }

        Ok(Self {
            path,
            ids: parsed.terminal_auth_event_ids,
        })
    }

    /// Whether `event_id` has been terminally disposed of.
    pub fn contains(&self, event_id: &str) -> bool {
        self.ids.contains(event_id)
    }

    /// Number of stored dispositions.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the store holds no dispositions.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The store's on-disk path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Durably record every ID in `event_ids` as terminally disposed.
    ///
    /// All-or-none: the complete candidate snapshot (existing ∪ new) is
    /// validated and written before memory is updated. A failure anywhere
    /// leaves both disk and memory exactly as they were, so the caller can
    /// refuse to send a notice and refuse to report completion.
    ///
    /// Duplicate IDs — the same event appearing in both the normal and the
    /// cancelled half of a batch — compact into one entry.
    pub fn commit_batch<I, S>(&mut self, event_ids: I) -> Result<(), TerminalAuthStoreError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let display = self.path.display().to_string();

        let mut candidate = self.ids.clone();
        for id in event_ids {
            let id = id.as_ref();
            if !is_valid_event_id(id) {
                return Err(TerminalAuthStoreError::InvalidId { path: display });
            }
            candidate.insert(id.to_string());
        }

        // Admission is all-or-none: a batch that would cross the ceiling is
        // refused whole, never partially recorded.
        if candidate.len() > MAX_TERMINAL_AUTH_IDS {
            return Err(TerminalAuthStoreError::OverCapacity {
                path: display,
                count: candidate.len(),
            });
        }

        // Nothing new — the events were already disposed of. Still a success:
        // the durable promise the caller needs already holds.
        if candidate.len() == self.ids.len() {
            return Ok(());
        }

        let payload = serde_json::to_vec(&StoreFile {
            version: SCHEMA_VERSION,
            terminal_auth_event_ids: candidate.clone(),
        })
        .map_err(|e| TerminalAuthStoreError::CommitFailed {
            path: display.clone(),
            source: std::io::Error::other(e),
        })?;

        durable_replace(&self.path, &payload).map_err(|source| {
            TerminalAuthStoreError::CommitFailed {
                path: display,
                source,
            }
        })?;

        // Memory is updated only after the bytes are durable, so a failed
        // commit can never make an undisposed event look disposed.
        self.ids = candidate;
        Ok(())
    }
}

/// Same-directory durable atomic replacement.
///
/// Temporary write → file sync → rename → parent-directory sync. The temp file
/// lives in the destination directory so the rename is within one filesystem
/// and therefore atomic; the parent sync is what makes the *rename itself*
/// survive a power loss, not just the bytes.
fn durable_replace(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("terminal-auth store path has no parent"))?;
    std::fs::create_dir_all(parent)?;

    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("terminal-auth")
    ));

    {
        let mut file = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(payload)?;
        file.sync_all()?;
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => {}
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }

    // Directory sync: without it the rename can be lost even though the file
    // contents were synced. Best-effort on platforms that refuse to open a
    // directory for syncing.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Resolve the state directory from an explicit override or the resolved
/// config-file location.
///
/// Defaults to `buzz-acp-state/` beside `buzz-acp.toml`, so an operator who
/// moved their config also moved their state without configuring anything.
pub fn resolve_state_dir(explicit: Option<&Path>, config_path: &Path) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    let base = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("buzz-acp-state")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBKEY: &str = "aa11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66";

    fn id(seed: u8) -> String {
        format!("{seed:02x}").repeat(32)
    }

    #[test]
    fn missing_store_loads_empty() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = TerminalAuthStore::load(temp.path(), PUBKEY).expect("missing store is empty");
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(!store.contains(&id(1)));
    }

    #[test]
    fn committed_snapshot_survives_reopen() {
        let temp = tempfile::tempdir().expect("temp dir");
        {
            let mut store = TerminalAuthStore::load(temp.path(), PUBKEY).expect("load");
            store.commit_batch([id(1), id(2)]).expect("commit");
        }
        let reopened = TerminalAuthStore::load(temp.path(), PUBKEY).expect("reload");
        assert_eq!(reopened.len(), 2);
        assert!(reopened.contains(&id(1)));
        assert!(reopened.contains(&id(2)));
        assert!(!reopened.contains(&id(3)));
    }

    #[test]
    fn duplicate_ids_compact_to_one_entry() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut store = TerminalAuthStore::load(temp.path(), PUBKEY).expect("load");
        store
            .commit_batch([id(7), id(7), id(7)])
            .expect("duplicates commit");
        assert_eq!(store.len(), 1);

        // A second commit of the same ID is a no-op success.
        store.commit_batch([id(7)]).expect("idempotent commit");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn stored_order_is_deterministic() {
        let temp_a = tempfile::tempdir().expect("temp a");
        let temp_b = tempfile::tempdir().expect("temp b");
        let mut a = TerminalAuthStore::load(temp_a.path(), PUBKEY).expect("load a");
        let mut b = TerminalAuthStore::load(temp_b.path(), PUBKEY).expect("load b");
        a.commit_batch([id(3), id(1), id(2)]).expect("commit a");
        b.commit_batch([id(2), id(3), id(1)]).expect("commit b");

        let bytes_a = std::fs::read(a.path()).expect("read a");
        let bytes_b = std::fs::read(b.path()).expect("read b");
        assert_eq!(
            bytes_a, bytes_b,
            "insertion order must not affect the serialised snapshot"
        );
    }

    #[test]
    fn identities_resolve_to_separate_files() {
        let temp = tempfile::tempdir().expect("temp dir");
        let other = "bb11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66";
        let mut first = TerminalAuthStore::load(temp.path(), PUBKEY).expect("load first");
        first.commit_batch([id(9)]).expect("commit first");

        let second = TerminalAuthStore::load(temp.path(), other).expect("load second");
        assert!(second.is_empty(), "a second identity must start empty");
        assert_ne!(first.path(), second.path());
    }

    fn write_raw(temp: &Path, body: &str) {
        let path = TerminalAuthStore::path_for(temp, PUBKEY);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, body).expect("write raw");
    }

    #[test]
    fn corrupt_json_fails_closed() {
        let temp = tempfile::tempdir().expect("temp dir");
        write_raw(temp.path(), "{not json");
        assert!(matches!(
            TerminalAuthStore::load(temp.path(), PUBKEY),
            Err(TerminalAuthStoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn unknown_version_fails_closed() {
        let temp = tempfile::tempdir().expect("temp dir");
        write_raw(
            temp.path(),
            r#"{"version":99,"terminal_auth_event_ids":[]}"#,
        );
        assert!(matches!(
            TerminalAuthStore::load(temp.path(), PUBKEY),
            Err(TerminalAuthStoreError::UnsupportedVersion { version: 99, .. })
        ));
    }

    #[test]
    fn invalid_and_uppercase_ids_fail_closed() {
        for bad in [
            "\"short\"",
            "\"AA11BB22CC33DD44EE55FF66AA77BB88CC99DD00EE11FF22AA33BB44CC55DD66\"",
            "\"zz11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66\"",
        ] {
            let temp = tempfile::tempdir().expect("temp dir");
            write_raw(
                temp.path(),
                &format!(r#"{{"version":1,"terminal_auth_event_ids":[{bad}]}}"#),
            );
            assert!(
                matches!(
                    TerminalAuthStore::load(temp.path(), PUBKEY),
                    Err(TerminalAuthStoreError::InvalidId { .. })
                ),
                "{bad} must fail closed"
            );
        }
    }

    #[test]
    fn over_limit_state_fails_closed_on_load() {
        let temp = tempfile::tempdir().expect("temp dir");
        let ids: Vec<String> = (0..=MAX_TERMINAL_AUTH_IDS)
            .map(|i| format!("{i:064x}"))
            .collect();
        let body = serde_json::json!({"version": 1, "terminal_auth_event_ids": ids});
        write_raw(temp.path(), &body.to_string());
        assert!(matches!(
            TerminalAuthStore::load(temp.path(), PUBKEY),
            Err(TerminalAuthStoreError::OverCapacity { .. })
        ));
    }

    #[test]
    fn capacity_admission_is_all_or_none() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut store = TerminalAuthStore::load(temp.path(), PUBKEY).expect("load");
        let filled: Vec<String> = (0..MAX_TERMINAL_AUTH_IDS - 1)
            .map(|i| format!("{i:064x}"))
            .collect();
        store.commit_batch(filled).expect("fill to one below limit");
        assert_eq!(store.len(), MAX_TERMINAL_AUTH_IDS - 1);

        // A two-event batch cannot fit in the single remaining slot; neither
        // event may be recorded.
        let overflow = vec![
            format!("{:064x}", MAX_TERMINAL_AUTH_IDS + 1),
            format!("{:064x}", MAX_TERMINAL_AUTH_IDS + 2),
        ];
        assert!(matches!(
            store.commit_batch(overflow.clone()),
            Err(TerminalAuthStoreError::OverCapacity { .. })
        ));
        assert_eq!(store.len(), MAX_TERMINAL_AUTH_IDS - 1);
        for id in &overflow {
            assert!(!store.contains(id), "refused batch must record nothing");
        }

        let on_disk = TerminalAuthStore::load(temp.path(), PUBKEY).expect("reload");
        assert_eq!(on_disk.len(), MAX_TERMINAL_AUTH_IDS - 1);
    }

    #[test]
    fn commit_failure_preserves_old_disk_and_memory_state() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut store = TerminalAuthStore::load(temp.path(), PUBKEY).expect("load");
        store.commit_batch([id(1)]).expect("first commit");
        let before = std::fs::read(store.path()).expect("read committed snapshot");

        // An invalid ID in the middle of a batch aborts the whole commit.
        let outcome = store.commit_batch([id(2), "not-an-event-id".to_string(), id(3)]);
        assert!(matches!(
            outcome,
            Err(TerminalAuthStoreError::InvalidId { .. })
        ));
        assert_eq!(store.len(), 1, "memory must be unchanged");
        assert!(!store.contains(&id(2)));
        assert!(!store.contains(&id(3)));
        assert_eq!(
            std::fs::read(store.path()).expect("re-read"),
            before,
            "disk must be unchanged"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_directory_fails_commit_without_mutating_memory() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let store_dir = temp.path().join(STORE_DIR);
        std::fs::create_dir_all(&store_dir).expect("mkdir");
        std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o500))
            .expect("chmod read-only");

        let mut store = TerminalAuthStore::load(temp.path(), PUBKEY).expect("load");
        let outcome = store.commit_batch([id(4)]);

        // Restore permissions before asserting so the tempdir can be cleaned up
        // even when the assertion fails.
        std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore perms");

        assert!(
            matches!(outcome, Err(TerminalAuthStoreError::CommitFailed { .. })),
            "an unwritable store directory must fail the commit"
        );
        assert!(store.is_empty(), "memory must not record a failed commit");
    }

    #[test]
    fn state_dir_defaults_beside_the_config_file() {
        assert_eq!(
            resolve_state_dir(None, Path::new("/etc/buzz/buzz-acp.toml")),
            PathBuf::from("/etc/buzz/buzz-acp-state")
        );
        assert_eq!(
            resolve_state_dir(None, Path::new("buzz-acp.toml")),
            PathBuf::from("./buzz-acp-state")
        );
        assert_eq!(
            resolve_state_dir(
                Some(Path::new("/var/lib/buzz")),
                Path::new("x/buzz-acp.toml")
            ),
            PathBuf::from("/var/lib/buzz")
        );
    }

    #[test]
    fn commit_creates_the_store_directory_when_absent() {
        let temp = tempfile::tempdir().expect("temp dir");
        let nested = temp.path().join("deep").join("nested");
        let mut store = TerminalAuthStore::load(&nested, PUBKEY).expect("load");
        store.commit_batch([id(5)]).expect("commit into fresh tree");
        assert!(store.path().exists());
        assert!(TerminalAuthStore::load(&nested, PUBKEY)
            .expect("reload")
            .contains(&id(5)));
    }
}
