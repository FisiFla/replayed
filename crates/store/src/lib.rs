//! SQLite index of clips, events and sessions.
//!
//! The storage policy is built on top of this: [`cleanup`] decides what may be deleted from the
//! rows this module hands out and applies that decision row-first, file-second, so no file is
//! unlinked without a committed delete naming it. Every unattended unlink is also confined to a
//! [`LibraryRoots`] pair (the clips and sessions directories), and one that fails is queued in
//! `pending_deletes` and retried by the next pass.

// The test bodies use `unwrap()` throughout; production code does not.
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod cleanup;
pub mod matches;
pub mod prune;
pub mod retention;
pub mod roots;

pub use roots::LibraryRoots;
use roots::Target;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The schema this build writes. Each version adds columns in place without reading, copying or
/// losing a row (see [`Store::migrate`]): v2 the session columns, v3 the media-clock anchor of a
/// session's event timeline, v4 the media length that timeline is plotted against, v5 the clip
/// library's title, game and tags, v6 the kind of artefact a clip row names (a video or a
/// screenshot), v7 the `events.clip_id` index and the `pending_deletes` table (see
/// [`V7_DDL`]).
const SCHEMA_VERSION: i32 = 7;

/// The mode of a session the ring buffer opened: the row groups the clips and events of one
/// buffer run, and there is no concatenated session file.
pub const SESSION_MODE_BUFFER: &str = "buffer";

/// The mode of a session the full-session recorder opened: the scratch segments are concatenated
/// into one file, which `sessions.final_path` names.
pub const SESSION_MODE_SESSION: &str = "session";

/// Every mode [`Store::start_session`] accepts. Closed, because every reader of `sessions.mode` switches on it.
const SESSION_MODES: [&str; 2] = [SESSION_MODE_BUFFER, SESSION_MODE_SESSION];

/// The v1 tables.
///
/// Created `IF NOT EXISTS` on every upgrade path, so this is both the schema of a new database
/// and the base a v1 database already has; the v1 -> v2 step carries it to the current shape.
const V1_DDL: &str = r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS sessions (
                id          INTEGER PRIMARY KEY,
                game        TEXT,
                started_at  INTEGER NOT NULL,
                ended_at    INTEGER,
                scratch_dir TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS clips (
                id          INTEGER PRIMARY KEY,
                session_id  INTEGER REFERENCES sessions(id),
                path        TEXT NOT NULL UNIQUE,
                started_at  INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                size_bytes  INTEGER NOT NULL,
                codec       TEXT NOT NULL,
                favourite   INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS events (
                id         INTEGER PRIMARY KEY,
                session_id INTEGER REFERENCES sessions(id),
                kind       TEXT NOT NULL,
                at         INTEGER NOT NULL,
                payload    TEXT,
                clip_id    INTEGER REFERENCES clips(id)
            );
            CREATE INDEX IF NOT EXISTS idx_clips_started_at ON clips(started_at);
            CREATE INDEX IF NOT EXISTS idx_events_session   ON events(session_id, at);
            "#;

/// The columns schema v2 adds to `sessions`, and the DDL that adds each.
///
/// Each is `ADD COLUMN`-able in place: SQLite adds a column with a constant default without
/// rebuilding the table.
///
/// A legacy session's `mode` defaults to `'buffer'` because a row from before this build was
/// written by the buffer engine or not at all (the full-session recorder never ran in a released
/// build), and `'session'` would assert a concatenated file the row does not have (`final_path`
/// is NULL, `size_bytes` is 0). `'buffer'` is the only claim a legacy row supports; anything else
/// would charge a phantom artefact to a retention cap.
const V2_SESSION_COLUMNS: [(&str, &str); 4] = [
    (
        "mode",
        "ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'buffer'",
    ),
    (
        "final_path",
        "ALTER TABLE sessions ADD COLUMN final_path TEXT",
    ),
    (
        "size_bytes",
        "ALTER TABLE sessions ADD COLUMN size_bytes INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "favourite",
        "ALTER TABLE sessions ADD COLUMN favourite INTEGER NOT NULL DEFAULT 0",
    ),
];

/// The columns schema v3 adds to `sessions`, and the DDL that adds each.
///
/// `events.at` is media time (the ledger's clock, which restarts with the scratch directory)
/// while `started_at` is wall clock, so their difference is arithmetic on two unrelated clocks
/// and plots events nowhere near when they happened. `media_epoch_ms` records the media position
/// the session began at, so the timeline is `at - media_epoch_ms`: one clock.
///
/// A legacy row defaults to 0, the only epoch this build can supply without inventing a
/// measurement (the same reasoning as v2's `mode`).
const V3_SESSION_COLUMNS: [(&str, &str); 1] = [(
    "media_epoch_ms",
    "ALTER TABLE sessions ADD COLUMN media_epoch_ms INTEGER NOT NULL DEFAULT 0",
)];

/// The columns schema v4 adds to `sessions`, and the DDL that adds each.
///
/// The media length of a session: the concatenated file's probed duration, recorded by
/// `end_session`. `started_at`/`ended_at` are wall clock and `size_bytes` is a byte count, so a
/// review UI had nothing to scale its scrubber against.
///
/// A legacy row defaults to 0, meaning "unknown" (no probed duration exists for it), which makes
/// a UI disable the scrubber rather than draw a bogus axis.
const V4_SESSION_COLUMNS: [(&str, &str); 1] = [(
    "duration_ms",
    "ALTER TABLE sessions ADD COLUMN duration_ms INTEGER NOT NULL DEFAULT 0",
)];

/// The columns schema v5 adds to `clips`, and the DDL that adds each.
///
/// `title` is a display name the user chose; the file on disk is never renamed (that would move
/// the only copy of the footage and invalidate every path the index, thumbnail cache and asset
/// scope hold). NULL means "show the file name". `game` is the game the session matched, NULL
/// when none was detected. Both are nullable with no default, so a legacy row claims nothing.
const V5_CLIP_COLUMNS: [(&str, &str); 2] = [
    ("title", "ALTER TABLE clips ADD COLUMN title TEXT"),
    ("game", "ALTER TABLE clips ADD COLUMN game TEXT"),
];

/// The v6 column: what the row is. A screenshot is an artefact the user keeps, tags and protects
/// exactly like a clip, so it lives in the same table rather than in a second list the storage
/// policy would not see and the library would have to merge by hand.
///
/// A default of `'clip'` is the only claim a v5 row supports: before this column existed there was
/// no way to make a screenshot, so every row is a video.
const V6_CLIP_COLUMNS: [(&str, &str); 1] = [(
    "kind",
    "ALTER TABLE clips ADD COLUMN kind TEXT NOT NULL DEFAULT 'clip'",
)];

/// A row that names a recorded video: the ordinary clip or session extract.
pub const CLIP_KIND_CLIP: &str = "clip";

/// A row that names a still image written by the screenshot hotkey.
pub const CLIP_KIND_SCREENSHOT: &str = "screenshot";

/// The v7 additions: an index on `events.clip_id` and the `pending_deletes` table.
///
/// * **The index.** The only index on `events` was `(session_id, at)`, so every clip delete (the
///   `UPDATE events SET clip_id = NULL WHERE clip_id = ?` that detaches its events, plus SQLite's
///   foreign-key child lookup) and `events_for_clip` scanned the whole table, inside a write
///   transaction that holds the lock the recorder's 200 ms size writes are waiting for.
/// * **The tombstones.** Retention deletes the row first and the file second, because the reverse
///   order can lose the only copy of footage. The cost is that a *failed* second step (the file is
///   open in a previewer, a scanner has it, the disk errored) used to leave a file on disk that no
///   row names: invisible to the library, excluded from the cap, never evicted and never retried.
///   `evict_*` now writes the path here **in the same transaction as the row delete**, and the
///   executor removes the tombstone only once the path is gone, so a failure *and* a crash between
///   the two steps are both retried by the next pass. `target` says what the path is, and so which
///   containment root vouches for it (see [`LibraryRoots`]).
const V7_DDL: &str = r#"
            CREATE INDEX IF NOT EXISTS idx_events_clip ON events(clip_id);
            CREATE TABLE IF NOT EXISTS pending_deletes (
                id        INTEGER PRIMARY KEY,
                target    TEXT NOT NULL
                          CHECK (target IN ('clip_file', 'session_file', 'session_dir')),
                path      TEXT NOT NULL,
                queued_at INTEGER NOT NULL
            );
            "#;

/// The v5 tag table: one row per (clip, tag). Tags are normalized by [`normalize_tag`] before
/// they are written, so the primary key makes a tag set a set.
const V5_TAGS_DDL: &str = r#"
            CREATE TABLE IF NOT EXISTS clip_tags (
                clip_id INTEGER NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
                tag     TEXT NOT NULL,
                PRIMARY KEY (clip_id, tag)
            );
            CREATE INDEX IF NOT EXISTS idx_clip_tags_tag ON clip_tags(tag);
            "#;

/// Longest tag, in characters.
pub const MAX_TAG_CHARS: usize = 32;
/// Most tags one clip carries.
pub const MAX_TAGS_PER_CLIP: usize = 20;
/// Longest display title, in characters.
pub const MAX_TITLE_CHARS: usize = 120;

/// A tag as stored: trimmed, internal whitespace collapsed, lower-cased. `None` for a tag that is
/// empty after that or longer than [`MAX_TAG_CHARS`]: refused, not truncated.
pub fn normalize_tag(raw: &str) -> Option<String> {
    let tag = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let n = tag.chars().count();
    (n > 0 && n <= MAX_TAG_CHARS && !tag.chars().any(char::is_control)).then_some(tag)
}

/// A display title as stored: trimmed; `None` (clear the title) when empty. Over-long or
/// control-character titles are an error rather than a silent edit.
pub fn normalize_title(raw: &str) -> Result<Option<String>> {
    let title = raw.trim();
    if title.is_empty() {
        return Ok(None);
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        anyhow::bail!("a clip title is at most {MAX_TITLE_CHARS} characters");
    }
    if title.chars().any(char::is_control) {
        anyhow::bail!("a clip title cannot contain control characters");
    }
    Ok(Some(title.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClip {
    pub path: PathBuf,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
}

/// One stored path to change, for [`Store::rewrite_paths`]: `(row id, the value the caller read,
/// the value to write)`.
pub type PathRewrite = (i64, String, String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub id: i64,
    /// [`CLIP_KIND_CLIP`] or [`CLIP_KIND_SCREENSHOT`], stored as text.
    ///
    /// Text rather than an enum, for the reason [`Session::mode`] is: a row written by a newer
    /// build must be readable here, and a kind this build does not know is better rendered
    /// cautiously than refused. [`Clip::is_screenshot`] is the only test a caller needs.
    pub kind: String,
    pub path: PathBuf,
    /// The clip's first frame on the encoder's media timeline, in ms: the ledger's clock, not wall
    /// clock (see the CLI's `index_clip`). Not comparable across captures; it restarts with the scratch directory.
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    pub favourite: bool,
    /// Wall-clock instant this row was written, in ms since the Unix epoch (`clips.created_at`,
    /// stamped by [`Store::insert_clip`], never by the caller).
    ///
    /// The only instant in the row comparable across runs, so the storage policy ages clips by it and
    /// orders a library by it (see [`cleanup::plan_cleanup`]).
    pub created_at_ms: i64,
    /// User-chosen display name (schema v5); `None` shows the file name.
    pub title: Option<String>,
    /// The game the recording matched (schema v5); `None` when none was detected.
    pub game: Option<String>,
    /// User tags (schema v5), normalized and sorted.
    pub tags: Vec<String>,
}

impl Clip {
    /// Whether this row names a still image rather than a video: no duration, no player, no trim,
    /// and the file itself is its own thumbnail.
    pub fn is_screenshot(&self) -> bool {
        self.kind == CLIP_KIND_SCREENSHOT
    }
}

/// A screenshot to index: the file is already written, and its size is what the storage policy
/// needs. There is no duration, no codec to probe and no media timeline to place it on — a still
/// image has none of those, which is the whole reason it is a different [`Clip::kind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewScreenshot {
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// A row to write into `events`.
///
/// `at_ms` is on the ledger's media timeline, the same clock `clips.started_at` is on, not wall
/// clock: events are only ever placed against a clip by timestamp, so a wall-clock value could
/// not be compared with the row it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    /// Always `None` for now: this engine opens no `sessions` row for events (see the recorder's `index_clip`).
    pub session_id: Option<i64>,
    /// `EventKind::as_tag`, e.g. `"kill"` or `"bomb_planted"`.
    pub kind: String,
    pub at_ms: u64,
    /// Integration-specific detail as one line of JSON.
    pub payload: Option<String>,
    /// The clip this event produced, when it produced one. `None` is meaningful: a marker (a game or
    /// round starting) has no clip, and neither does an event whose clip could not be indexed.
    pub clip_id: Option<i64>,
}

/// A row of `events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: i64,
    pub session_id: Option<i64>,
    pub kind: String,
    pub at_ms: u64,
    pub payload: Option<String>,
    pub clip_id: Option<i64>,
}

/// A row of `sessions`: one recording session, with what the session manager and review UI need to show and manage it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: i64,
    pub game: Option<String>,
    /// [`SESSION_MODE_BUFFER`] or [`SESSION_MODE_SESSION`]; stored as TEXT so a row written by a newer build is readable rather than an error.
    pub mode: String,
    /// When the session started on the wall clock: what retention ages a session by and the session
    /// store is ordered by (see [`retention::plan_retention`]), and what a UI shows as "recorded at".
    /// Not what the event timeline is measured from; that is [`Session::media_epoch_ms`].
    pub started_at_ms: i64,
    /// The media position this session began at, in ms: what [`Store::events_for_session`] subtracts
    /// from `events.at` to place an event on the session timeline. `0` for a session that started
    /// against an empty ledger (the ordinary case) and for every row before schema v3.
    ///
    /// Two clocks, two columns. Media time restarts with the scratch directory and is the axis frames
    /// are measured on; wall time survives a restart and is the axis "how old is this" is measured
    /// on. A session that reuses a scratch directory inherits a non-zero epoch, so one column cannot
    /// serve both and subtracting the wrong one misplaces every event.
    pub media_epoch_ms: i64,
    /// When the recording stopped. `None` means it is still recording: the one state that is not
    /// history, and the one retention must never evict.
    pub ended_at_ms: Option<i64>,
    /// The directory the session's segments are written into, and the one removed when the session is
    /// deleted. `NOT NULL`, and correct for a buffer-mode session too: the ring writes into a scratch directory as well.
    pub scratch_dir: String,
    /// The concatenated session file, once finalised. `None` until [`Store::end_session`] is told of
    /// one; a buffer-mode session never has one.
    pub final_path: Option<String>,
    /// Bytes the session occupies, as recorded at finalisation by [`Store::end_session`] (and while
    /// running by [`Store::set_session_size`]); the retention cap's arithmetic uses it.
    pub size_bytes: i64,
    /// How long the recorded media is, in ms: the concatenated file's probed duration, the axis a
    /// review timeline is drawn against. Written by [`Store::end_session`]; `0` means unknown (a
    /// session recovered from a crash has no file to probe; so does every row before schema v4).
    ///
    /// Not derivable from the other columns: `started_at`/`ended_at` measure how long the window was
    /// open, not how much footage it holds.
    pub duration_ms: i64,
    /// Exempt from both retention rules, as a favourited clip is. [`Store::set_session_favourite`] is the user's only way in.
    pub favourite: bool,
}

/// One row of the session timeline: an event, placed against the session that was recording when it happened.
///
/// Nothing is filtered: a manual hotkey bookmark and a game-derived kill are rows of the same
/// timeline, and which of them produced a clip is [`SessionEvent::clip_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvent {
    pub id: i64,
    /// `events.kind`, passed through verbatim. This crate does not know the vocabulary and must not:
    /// an unfamiliar tag is a row that exists, not an error.
    pub kind: String,
    /// The event's position on the session timeline: `events.at - sessions.media_epoch_ms`, in ms.
    /// Derived in one place ([`Store::events_for_session`]) so the scrubber, a test and the UI cannot
    /// disagree. A negative offset is possible and reported, not clamped: it means a row was written
    /// before the session's own start.
    pub offset_ms: i64,
    /// Integration-specific detail as one line of JSON, when there is any.
    pub payload: Option<String>,
    /// The clip this event produced, when it produced one. `None` for a marker: a bookmark never
    /// clipped, a round boundary, or an event whose clip could not be indexed.
    pub clip_id: Option<i64>,
}

/// What a deleted session row leaves for the caller to remove from disk. The row is already
/// committed gone; these are the paths it named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    /// The concatenated session file, when the session had one.
    pub final_path: Option<String>,
    /// The session's scratch directory: its segments, possibly still growing if the session never
    /// ended cleanly (see [`Store::delete_session_returning_paths`]).
    pub scratch_dir: String,
}

/// What an eviction attempt found, so an executor can tell "someone protected it" from "it was
/// already gone" from "I removed it".
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Eviction<T> {
    /// This call's own `DELETE` removed the row, and queued these paths for removal in the same
    /// transaction.
    Evicted(T),
    /// The row exists but is a favourite now: it was not when the plan was made. Nothing changed.
    Protected,
    /// No such row (another pass, another instance or the user got there first). Nothing changed.
    Gone,
}

/// A path a committed eviction named and the pass still has to remove (see [`V7_DDL`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tombstone {
    pub(crate) id: i64,
    pub(crate) target: Target,
    pub(crate) path: PathBuf,
}

pub struct Store {
    conn: Connection,
}

/// How long a write waits for another connection's lock before it gives up.
///
/// Two processes write this database: the desktop shell (favourite, rename, tag, trim, delete)
/// and the recorder (a row per hotkey press, a bulk delete per cleanup pass). They are separate
/// connections to the same file, and SQLite's default busy timeout is zero: a write that meets a
/// held lock fails with `SQLITE_BUSY` at once rather than waiting for it to clear.
///
/// The recorder treats an index failure as non-fatal but logs it at ERROR, so a hotkey press that
/// collided with a UI write left the clip's file on disk with no row naming it: invisible in the
/// library, and unmanaged by the storage policy, which can only manage what the index names.
///
/// Five seconds is far longer than any of these writes takes (each is a single statement, and the
/// cleanup pass's bulk delete is one transaction) and short enough that a writer which really is
/// stuck does not wedge the recorder's hotkey path.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

impl Store {
    /// A store on a private in-memory database.
    ///
    /// The busy timeout is set for symmetry with [`Store::open`]: an in-memory database has no
    /// other connection to collide with, but the two paths must not differ in anything else.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory db")?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .context("setting the in-memory db's busy timeout")?;
        Ok(Self { conn })
    }

    /// A store on the database file at `path`.
    ///
    /// The busy timeout is what makes two processes able to share the file (see
    /// [`BUSY_TIMEOUT`]): without it, a write that meets the other process's lock fails at once
    /// instead of waiting for it.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .with_context(|| format!("setting the busy timeout on {}", path.display()))?;
        Ok(Self { conn })
    }

    /// Forward-only migrations. A schema newer than this build is refused rather than downgraded.
    ///
    /// Every version is reached the same way, so a new database and an upgraded v1 end up the same
    /// shape: the v1 tables are created `IF NOT EXISTS`, each step the database has not had is
    /// applied in order, then the version is stamped. `PRAGMA user_version` is written last, so a
    /// crash part-way leaves a database one step behind, upgraded by the next call, never one
    /// claiming a version it lacks.
    ///
    /// # Two processes migrate the same file
    ///
    /// The desktop shell and the recorder (and the CLI) each open this database, and the first
    /// start after an upgrade has all of them calling `migrate` at once. Two things made that race:
    ///
    /// * The steps ran in `BEGIN DEFERRED` transactions that read (`column_exists`) before they
    ///   wrote (`ALTER`), so the loser's read lock could not be promoted and SQLite failed it with
    ///   `SQLITE_BUSY` at once, *ignoring* the busy timeout; or it passed the check, lost the race
    ///   and died on `duplicate column name`.
    /// * Both made the backup, and one's `VACUUM INTO` met the file the other had just created.
    ///
    /// Now the whole upgrade is one `BEGIN IMMEDIATE` transaction: the write lock is taken (and
    /// waited for, with the busy timeout) *before* anything is read, and `user_version` is read
    /// again under it, so the second process finds the work done and returns. A transaction also
    /// means an interrupted upgrade rolls back whole rather than leaving a half-built table.
    /// `foreign_keys` is set before the transaction because SQLite ignores that pragma inside one.
    pub fn migrate(&self) -> Result<()> {
        let current = self.schema_version()?;
        if current > SCHEMA_VERSION {
            anyhow::bail!(
                "database schema v{current} is newer than this build supports (v{SCHEMA_VERSION}); \
                 refusing to downgrade"
            );
        }
        if current == SCHEMA_VERSION {
            return Ok(());
        }

        // `VACUUM INTO` cannot run inside a transaction, so the backup is taken first, from the
        // version this process saw. If another process upgraded in between, the backup is of a
        // newer file than its name says: harmless, and still a restore point.
        self.backup_before_migration(current)?;
        self.conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        let tx = self.immediate_tx()?;
        let current = tx.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))?;
        if current > SCHEMA_VERSION {
            anyhow::bail!(
                "database schema v{current} is newer than this build supports (v{SCHEMA_VERSION}); \
                 refusing to downgrade"
            );
        }
        if current == SCHEMA_VERSION {
            // Another process finished the upgrade while this one waited for the lock.
            return Ok(());
        }
        tx.execute_batch(V1_DDL)?;
        if current < 2 {
            add_columns(&tx, "sessions", &V2_SESSION_COLUMNS)?;
        }
        if current < 3 {
            add_columns(&tx, "sessions", &V3_SESSION_COLUMNS)?;
        }
        if current < 4 {
            add_columns(&tx, "sessions", &V4_SESSION_COLUMNS)?;
        }
        if current < 5 {
            upgrade_to_v5(&tx)?;
        }
        if current < 6 {
            add_columns(&tx, "clips", &V6_CLIP_COLUMNS)?;
        }
        if current < 7 {
            tx.execute_batch(V7_DDL)?;
        }
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        tx.commit()?;
        Ok(())
    }

    /// Begin a transaction that takes the write lock now.
    ///
    /// Every transaction in this file that writes goes through here (or starts with a write). A
    /// `BEGIN DEFERRED` transaction that reads first holds a read lock when it asks for the write
    /// lock, and if another connection holds that lock SQLite refuses the promotion immediately
    /// instead of invoking the busy handler: the [`BUSY_TIMEOUT`] does not apply. With the recorder
    /// committing a size write every 200 ms, a desktop tag edit hit that window readily.
    fn immediate_tx(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    /// Copy an existing, populated database aside before upgrading it, so a bad migration costs a
    /// restore rather than the library. The two newest backups are kept. In-memory and brand-new
    /// databases are skipped. A backup that cannot be written aborts the migration before anything changed.
    fn backup_before_migration(&self, current: i32) -> Result<()> {
        let populated: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'clips')",
            [],
            |r| r.get(0),
        )?;
        let Some(db_path) = self
            .conn
            .path()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
        else {
            return Ok(());
        };
        if !populated {
            return Ok(());
        }
        let mut name = db_path.as_os_str().to_owned();
        name.push(format!(".v{current}.bak"));
        let backup = PathBuf::from(&name);
        // Written under a name only this call uses (process id plus a counter, so two threads of one
        // process are as separate as two processes), then renamed into place. `VACUUM INTO`
        // refuses an existing target, so writing straight to the shared name made the second of two
        // simultaneous upgraders fail with "output file already exists" and abort its startup; the
        // rename is atomic, so a reader never sees a half-written backup either.
        static STAGING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        name.push(format!(".tmp-{}-{unique}", std::process::id()));
        let staging = PathBuf::from(name);
        let _ = std::fs::remove_file(&staging);
        self.conn
            .execute("VACUUM INTO ?1", [staging.to_string_lossy().as_ref()])
            .with_context(|| {
                format!(
                    "backing up the database to {} before migrating",
                    backup.display()
                )
            })?;
        std::fs::rename(&staging, &backup).with_context(|| {
            format!(
                "moving the database backup into place at {}",
                backup.display()
            )
        })?;

        // Retention: keep the two newest `<db>.v*.bak`.
        if let (Some(dir), Some(base)) = (db_path.parent(), db_path.file_name()) {
            let prefix = format!("{}.v", base.to_string_lossy());
            let mut backups: Vec<_> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with(&prefix) && n.ends_with(".bak")
                })
                .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
                .collect();
            backups.sort();
            let excess = backups.len().saturating_sub(2);
            for (_, old) in backups.into_iter().take(excess) {
                let _ = std::fs::remove_file(old);
            }
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    pub fn insert_clip(&self, clip: &NewClip) -> Result<i64> {
        let path = clip.path.to_string_lossy();
        self.conn.execute(
            "INSERT INTO clips (path, started_at, duration_ms, size_bytes, codec, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                path,
                to_i64(clip.started_at_ms),
                to_i64(clip.duration_ms),
                to_i64(clip.size_bytes),
                clip.codec,
                now_ms(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Index a screenshot: a still image in the clips directory, with no duration and no timeline.
    ///
    /// `codec` is `"png"` rather than a guess: the column is what a reader shows beside a clip, and
    /// naming the real format is cheaper than having every surface special-case an empty string.
    pub fn insert_screenshot(&self, shot: &NewScreenshot) -> Result<i64> {
        let path = shot.path.to_string_lossy();
        self.conn.execute(
            "INSERT INTO clips (path, started_at, duration_ms, size_bytes, codec, created_at, kind)
             VALUES (?1, 0, 0, ?2, 'png', ?3, ?4)",
            params![
                path,
                to_i64(shot.size_bytes),
                now_ms(),
                CLIP_KIND_SCREENSHOT
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_clips(&self) -> Result<Vec<Clip>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, started_at, duration_ms, size_bytes, codec, favourite, created_at,
                    title, game, kind
             FROM clips ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Clip {
                id: r.get(0)?,
                kind: r.get(10)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                started_at_ms: to_u64(r.get::<_, i64>(2)?),
                duration_ms: to_u64(r.get::<_, i64>(3)?),
                size_bytes: to_u64(r.get::<_, i64>(4)?),
                codec: r.get(5)?,
                favourite: r.get::<_, i64>(6)? != 0,
                created_at_ms: r.get(7)?,
                title: r.get(8)?,
                game: r.get(9)?,
                tags: Vec::new(),
            })
        })?;
        let mut clips = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        let mut tags: std::collections::HashMap<i64, Vec<String>> =
            std::collections::HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT clip_id, tag FROM clip_tags ORDER BY tag")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, tag) = row?;
            tags.entry(id).or_default().push(tag);
        }
        for clip in &mut clips {
            clip.tags = tags.remove(&clip.id).unwrap_or_default();
        }
        Ok(clips)
    }

    /// Set (or, with an empty/blank `title`, clear) a clip's display title. Returns whether the clip
    /// exists. The file is never touched (see [`V5_CLIP_COLUMNS`]).
    pub fn set_clip_title(&self, id: i64, title: &str) -> Result<bool> {
        let title = normalize_title(title)?;
        let updated = self.conn.execute(
            "UPDATE clips SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(updated > 0)
    }

    /// Set or clear the game a clip belongs to. Returns whether the clip exists.
    pub fn set_clip_game(&self, id: i64, game: Option<&str>) -> Result<bool> {
        let game = game.map(str::trim).filter(|g| !g.is_empty());
        let updated = self.conn.execute(
            "UPDATE clips SET game = ?2 WHERE id = ?1",
            params![id, game],
        )?;
        Ok(updated > 0)
    }

    /// Replace a clip's whole tag set. Tags are normalized ([`normalize_tag`]) and de-duplicated; an
    /// invalid tag or more than [`MAX_TAGS_PER_CLIP`] is an error and leaves the existing set
    /// untouched. Returns whether the clip exists.
    pub fn set_clip_tags(&self, id: i64, tags: &[String]) -> Result<bool> {
        let mut clean = std::collections::BTreeSet::new();
        for raw in tags {
            match normalize_tag(raw) {
                Some(tag) => {
                    clean.insert(tag);
                }
                None => anyhow::bail!(
                    "invalid tag {raw:?}: tags are 1-{MAX_TAG_CHARS} characters, without control characters"
                ),
            }
        }
        if clean.len() > MAX_TAGS_PER_CLIP {
            anyhow::bail!("a clip carries at most {MAX_TAGS_PER_CLIP} tags");
        }
        // Immediate, not deferred: this reads before it writes (see `Store::immediate_tx`).
        let tx = self.immediate_tx()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM clips WHERE id = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(false);
        }
        tx.execute("DELETE FROM clip_tags WHERE clip_id = ?1", params![id])?;
        for tag in &clean {
            tx.execute(
                "INSERT INTO clip_tags (clip_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Write one `events` row and return its id.
    ///
    /// The foreign key on `clip_id` is enforced (the migration turns `foreign_keys` on for this
    /// connection), so an event cannot point at a clip that does not exist.
    pub fn insert_event(&self, event: &NewEvent) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (session_id, kind, at, payload, clip_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.session_id,
                event.kind,
                to_i64(event.at_ms),
                event.payload,
                event.clip_id,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every event, oldest first on the media timeline.
    pub fn list_events(&self) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, at, payload, clip_id FROM events ORDER BY at, id",
        )?;
        let rows = stmt.query_map([], read_event)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The events attached to one clip, oldest first, for the clip's detail pane.
    pub fn events_for_clip(&self, clip_id: i64) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, at, payload, clip_id FROM events
             WHERE clip_id = ?1 ORDER BY at, id",
        )?;
        let rows = stmt.query_map(params![clip_id], read_event)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One session's events with `at` in `[from_ms, to_ms)`, oldest first: the events a clip cut
    /// from that stretch of footage shows, whichever clip (if any) each produced.
    pub fn events_in_range(&self, session_id: i64, from_ms: u64, to_ms: u64) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, at, payload, clip_id FROM events
             WHERE session_id = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id",
        )?;
        let rows = stmt.query_map(
            params![session_id, to_i64(from_ms), to_i64(to_ms)],
            read_event,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark a clip as a favourite, or clear it. Favourites are exempt from both storage rules (the
    /// age rule and the total-bytes cap), so this is the user's only way to protect a clip.
    ///
    /// Returns whether a row was updated; `false` means no clip with that id exists.
    pub fn set_favourite(&self, id: i64, favourite: bool) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE clips SET favourite = ?2 WHERE id = ?1",
            params![id, favourite as i64],
        )?;
        Ok(updated > 0)
    }

    /// Total bytes the indexed clips claim: the figure `storage.max_total_bytes` caps. It is what the
    /// rows recorded when written, not a fresh `stat`, so a file changed behind the index will disagree.
    pub fn total_bytes(&self) -> Result<u64> {
        let bytes: i64 =
            self.conn
                .query_row("SELECT COALESCE(SUM(size_bytes), 0) FROM clips", [], |r| {
                    r.get(0)
                })?;
        Ok(to_u64(bytes))
    }

    /// Delete the row and return its path so the caller can unlink afterwards (row before file).
    /// Returns `None` if this call did not remove a row.
    ///
    /// The path comes back only when this call's own `DELETE` removed the row, so a row that vanished
    /// between the read and the delete (another cleanup pass, a second instance, a manual `sqlite3`)
    /// yields `None`: the returned path is one a committed delete removed, safe to unlink.
    ///
    /// Events that pointed at the clip stay and lose the link. `events.clip_id` is a foreign key, so
    /// deleting a clip an event names would otherwise fail with `FOREIGN KEY constraint failed`, and
    /// the policy could not evict exactly the clips a user clipped most. The events are history, not
    /// footage, so they are kept with `clip_id` NULL (the state a marker has). Both writes are one
    /// transaction, so a crash cannot leave events detached from a clip that still exists.
    pub fn delete_clip_returning_path(&self, id: i64) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row("SELECT path FROM clips WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(path) = path else {
            return Ok(None);
        };
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE events SET clip_id = NULL WHERE clip_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM clip_tags WHERE clip_id = ?1", params![id])?;
        let removed = tx.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok((removed == 1).then_some(path))
    }

    // ---------------------------------------------------------------- sessions

    /// Open a `sessions` row and return its id. The session is running from this moment: `ended_at`
    /// is NULL until [`Store::end_session`], and that NULL is what retention reads as "still
    /// recording" (see [`retention::plan_retention`]).
    ///
    /// `mode` is [`SESSION_MODE_BUFFER`] or [`SESSION_MODE_SESSION`] and is validated: every reader of
    /// the column switches on it, so a typo would quietly read as an unknown mode. The set is closed
    /// until a third engine exists.
    ///
    /// `started_at_ms` is wall clock and `media_epoch_ms` is the media-clock start; neither is
    /// derivable from the other. The event timeline is `events.at - media_epoch_ms`, while
    /// `started_at` ages the session and orders the store. Passing the wall clock as the epoch would
    /// misplace every event of a session that reused a scratch directory.
    ///
    /// The store has no access to the recorder's ledger, so it records the epoch it is handed without
    /// checking it. `0` is correct for an empty scratch directory, the ordinary case.
    pub fn start_session(
        &self,
        game: Option<&str>,
        mode: &str,
        started_at_ms: i64,
        scratch_dir: &str,
        media_epoch_ms: i64,
    ) -> Result<i64> {
        if !SESSION_MODES.contains(&mode) {
            anyhow::bail!(
                "unknown session mode {mode:?}: this build knows {}",
                SESSION_MODES.join(", ")
            );
        }
        self.conn.execute(
            "INSERT INTO sessions (game, mode, started_at, scratch_dir, media_epoch_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![game, mode, started_at_ms, scratch_dir, media_epoch_ms],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Finish a session: stamp `ended_at`, record the final file, the bytes the session holds, and
    /// how long the recorded media is.
    ///
    /// `duration_ms` is the media length, the axis a review timeline is drawn on. `0` means unknown,
    /// the honest value for a session recovered from a crash (`final_path` is `None`, no file to
    /// probe); a UI must treat it as "no axis", not a zero-length recording.
    ///
    /// # What this refuses
    ///
    /// A silent `Ok(())` for a missing or already-finalised session would be indistinguishable from
    /// success, so both are an `Err` that names the case:
    ///
    /// * no such session: the recorder is about to lose its bookkeeping, worth a warning;
    /// * already ended: the first finalisation wins and the row is not touched, so a retry, a double
    ///   stop or a second writer cannot rewrite a finished session's size or file. The
    ///   `WHERE ... AND ended_at IS NULL` on the update makes that a property of the write, and the
    ///   rowcount is checked, so a session ended by another writer between read and write is reported.
    ///
    /// This is also the escape hatch for a session left running by a crash (still `ended_at IS NULL`,
    /// see [`Store::delete_session_returning_paths`]): a recovery pass ends it explicitly, with a
    /// `final_path` of `None` and the bytes it measured, to make it evictable again. Nothing infers
    /// "this session is over" from a missing process.
    pub fn end_session(
        &self,
        id: i64,
        ended_at_ms: i64,
        final_path: Option<&str>,
        size_bytes: i64,
        duration_ms: i64,
    ) -> Result<()> {
        let ended: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        match ended {
            None => anyhow::bail!("cannot end session #{id}: no such session"),
            Some(Some(at)) => anyhow::bail!(
                "session #{id} was already ended at {at}ms; its row is untouched (the first \
                 finalisation of a session is the one that counts)"
            ),
            Some(None) => {}
        }

        let updated = self.conn.execute(
            "UPDATE sessions SET ended_at = ?2, final_path = ?3, size_bytes = ?4, duration_ms = ?5
             WHERE id = ?1 AND ended_at IS NULL",
            params![id, ended_at_ms, final_path, size_bytes, duration_ms],
        )?;
        if updated != 1 {
            anyhow::bail!(
                "session #{id} was ended by another writer between the read and the write; \
                 its row is untouched"
            );
        }
        Ok(())
    }

    /// Record how many bytes a running session currently holds.
    ///
    /// [`Store::end_session`] is otherwise the only writer of `size_bytes`, so without this a live
    /// recording is invisible to the sessions cap (`start_session` cannot know the bytes). The
    /// recorder calls it on the tick it already scans the scratch ring for. `0` is honest for a
    /// session with nothing on disk yet; a session that is not running is refused, since
    /// [`Store::end_session`] owns the finished number.
    pub fn set_session_size(&self, id: i64, size_bytes: i64) -> Result<()> {
        let ended: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        match ended {
            None => anyhow::bail!("cannot size session #{id}: no such session"),
            Some(Some(_)) => anyhow::bail!(
                "session #{id} has ended; its size is what end_session recorded, not a \
                 running total"
            ),
            Some(None) => {}
        }
        self.conn.execute(
            "UPDATE sessions SET size_bytes = ?2 WHERE id = ?1",
            params![id, size_bytes],
        )?;
        Ok(())
    }

    /// Every session, most recently started first: the review UI's order.
    ///
    /// Ordered by `started_at` then id, so the order is total and independent of SQLite's row order.
    /// The retention planner sorts the other way for itself.
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions ORDER BY started_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map([], read_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One session, or `None`.
    ///
    /// A missing id is `Ok(None)`, not an error: an evicted id, one a UI still has in a list, and
    /// one that never existed are the same state.
    pub fn get_session(&self, id: i64) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"
        ))?;
        Ok(stmt.query_row(params![id], read_session).optional()?)
    }

    /// The events recorded against one session, oldest first on the session timeline. Nothing is
    /// filtered: a manual hotkey bookmark and a game-derived kill are rows of the same timeline.
    ///
    /// `offset_ms` is derived here only: `events.at - sessions.media_epoch_ms`, one SQL expression,
    /// so the UI, the tests and any consumer agree on what a position means. Both sides are media
    /// time; subtracting `started_at` would be arithmetic across two clocks.
    ///
    /// A session that does not exist has no events (an inner join), and neither does one whose events
    /// were detached by its own deletion: the answer is an empty timeline, not an error.
    pub fn events_for_session(&self, id: i64) -> Result<Vec<SessionEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.kind, e.at - s.media_epoch_ms, e.payload, e.clip_id
             FROM events e
             JOIN sessions s ON s.id = e.session_id
             WHERE e.session_id = ?1
             ORDER BY e.at, e.id",
        )?;
        let rows = stmt.query_map(params![id], |row| {
            Ok(SessionEvent {
                id: row.get(0)?,
                kind: row.get(1)?,
                offset_ms: row.get(2)?,
                payload: row.get(3)?,
                clip_id: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark a session as a favourite, or clear it. Favourites are exempt from both retention rules,
    /// so this is the user's only way to protect a session.
    ///
    /// A session that does not exist is an `Err`: `Result<()>` has nowhere else to say "this write
    /// touched nothing", and silent success would let a UI report a favourite that is not stored.
    pub fn set_session_favourite(&self, id: i64, favourite: bool) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE sessions SET favourite = ?2 WHERE id = ?1",
            params![id, favourite as i64],
        )?;
        if updated == 0 {
            anyhow::bail!("no session #{id}: nothing was favourited");
        }
        Ok(())
    }

    /// Total bytes the indexed sessions claim, running ones included: the figure the sessions
    /// retention cap is measured against (see [`retention::RetentionRules`]).
    ///
    /// Like [`Store::total_bytes`], this is what the rows recorded, not a fresh walk of the scratch
    /// directories; they differ only while recording, which [`Store::set_session_size`] keeps in step.
    pub fn total_session_bytes(&self) -> Result<u64> {
        let bytes: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM sessions",
            [],
            |r| r.get(0),
        )?;
        Ok(to_u64(bytes))
    }

    /// Delete the session row and hand back the paths so the caller removes them afterwards: the
    /// same row-before-file contract as [`Store::delete_clip_returning_path`]. Returns `None` if this
    /// call did not remove a row.
    ///
    /// A session that is still recording is refused. Every other deletion destroys a copy of something
    /// regenerable; the scratch directory of a running session is the recording, so deleting it loses
    /// footage that exists nowhere else, and no ordering makes that safe. A session orphaned by a
    /// crash is made deletable by ending it (see [`Store::end_session`]), not by this method deciding
    /// the process is gone.
    ///
    /// Both links to it are detached, and nothing else is. `events.session_id` and `clips.session_id`
    /// are foreign keys into `sessions`, so with enforcement on, deleting a session an event or clip
    /// names would fail (`FOREIGN KEY constraint failed`) exactly when the manager tried to evict a
    /// session the user had clipped from. Both are set to NULL in the same transaction as the delete:
    ///
    /// * The events survive as history with `session_id` NULL, as [`Store::delete_clip_returning_path`]
    ///   treats an event whose clip went.
    /// * The clips survive as first-class artefacts: a clip extracted from a session is its own file
    ///   with its own row, and its footage lives in the clips directory, which this method cannot
    ///   touch. Only the pointer to the vanished row is cleared; NULL already means "belongs to no
    ///   session". (`ON DELETE SET NULL` on the two foreign keys would need a table rebuild, a
    ///   migration of its own; see the `retention` module for the same note.)
    pub fn delete_session_returning_paths(&self, id: i64) -> Result<Option<SessionPaths>> {
        let row: Option<(Option<i64>, Option<String>, String)> = self
            .conn
            .query_row(
                "SELECT ended_at, final_path, scratch_dir FROM sessions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((ended_at, final_path, scratch_dir)) = row else {
            return Ok(None);
        };
        if ended_at.is_none() {
            anyhow::bail!(
                "session #{id} is still recording (ended_at IS NULL); refusing to delete it — \
                 its scratch directory is the recording. End it with end_session first."
            );
        }

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE events SET session_id = NULL WHERE session_id = ?1",
            params![id],
        )?;
        tx.execute(
            "UPDATE clips SET session_id = NULL WHERE session_id = ?1",
            params![id],
        )?;
        let removed = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok((removed == 1).then_some(SessionPaths {
            final_path,
            scratch_dir,
        }))
    }

    /// Evict a clip for the storage policy: the row goes, its file is queued as a tombstone in the
    /// same transaction, and a **favourite is never evicted**, whatever the plan said.
    ///
    /// The executors work from a plan made from a snapshot, and between the snapshot and this call
    /// can lie seconds or minutes (a stat per clip, a multi-gigabyte unlink per deletion). A user who
    /// stars a clip in that window has made the one request the policy promises to honour, yet the
    /// unconditional [`Store::delete_clip_returning_path`] deleted it anyway. This method checks and
    /// deletes under one `BEGIN IMMEDIATE`, so the favourite flag cannot flip between the check and
    /// the delete (the desktop's `set_favourite` needs the same write lock), and the `DELETE` itself
    /// repeats `favourite = 0`. The user's explicit delete command keeps the unconditional method.
    ///
    /// Events and tags are handled exactly as [`Store::delete_clip_returning_path`] handles them.
    pub(crate) fn evict_clip(&self, id: i64) -> Result<Eviction<Tombstone>> {
        let tx = self.immediate_tx()?;
        let row: Option<(String, bool)> = tx
            .query_row(
                "SELECT path, favourite <> 0 FROM clips WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let path = match row {
            None => return Ok(Eviction::Gone),
            Some((_, true)) => return Ok(Eviction::Protected),
            Some((path, false)) => path,
        };
        tx.execute(
            "UPDATE events SET clip_id = NULL WHERE clip_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM clip_tags WHERE clip_id = ?1", params![id])?;
        let removed = tx.execute(
            "DELETE FROM clips WHERE id = ?1 AND favourite = 0",
            params![id],
        )?;
        if removed != 1 {
            return Ok(Eviction::Gone);
        }
        let tombstone = queue_tombstone(&tx, Target::ClipFile, &path)?;
        tx.commit()?;
        Ok(Eviction::Evicted(tombstone))
    }

    /// Evict a session for the storage policy: the row goes and what it owned on disk is queued as
    /// tombstones in the same transaction. A favourite is never evicted, as for
    /// [`Store::evict_clip`]; a session still recording is an error, as for
    /// [`Store::delete_session_returning_paths`].
    ///
    /// # What is queued, and what deliberately is not
    ///
    /// * the concatenated file, when there is one;
    /// * the scratch directory, **only for a session-mode row**. A buffer-mode row's `scratch_dir`
    ///   is the ring's one shared, user-configurable directory, written by no session in particular,
    ///   so evicting a row that merely *names* it must never remove it: it would take every other
    ///   file the user keeps there with it. Nor is a directory queued while another row still
    ///   names it.
    pub(crate) fn evict_session(&self, id: i64) -> Result<Eviction<Vec<Tombstone>>> {
        let tx = self.immediate_tx()?;
        let session: Option<Session> = tx
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                params![id],
                read_session,
            )
            .optional()?;
        let Some(Session {
            ended_at_ms: ended_at,
            final_path,
            scratch_dir,
            mode,
            favourite,
            ..
        }) = session
        else {
            return Ok(Eviction::Gone);
        };
        if ended_at.is_none() {
            anyhow::bail!(
                "session #{id} is still recording (ended_at IS NULL); refusing to evict it — \
                 its scratch directory is the recording. End it with end_session first."
            );
        }
        if favourite {
            return Ok(Eviction::Protected);
        }
        let shared: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE scratch_dir = ?1 AND id <> ?2)",
            params![scratch_dir, id],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE events SET session_id = NULL WHERE session_id = ?1",
            params![id],
        )?;
        tx.execute(
            "UPDATE clips SET session_id = NULL WHERE session_id = ?1",
            params![id],
        )?;
        let removed = tx.execute(
            "DELETE FROM sessions WHERE id = ?1 AND favourite = 0 AND ended_at IS NOT NULL",
            params![id],
        )?;
        if removed != 1 {
            return Ok(Eviction::Gone);
        }
        let mut queued = Vec::new();
        if let Some(final_path) = final_path {
            queued.push(queue_tombstone(&tx, Target::SessionFile, &final_path)?);
        }
        if mode == SESSION_MODE_SESSION && !shared {
            queued.push(queue_tombstone(&tx, Target::SessionDir, &scratch_dir)?);
        }
        tx.commit()?;
        Ok(Eviction::Evicted(queued))
    }

    /// Every path an earlier eviction committed and no pass has yet removed, oldest first.
    pub(crate) fn pending_deletes(&self) -> Result<Vec<Tombstone>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, target, path FROM pending_deletes ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, target, path) = row?;
            match Target::parse(&target) {
                Some(target) => out.push(Tombstone {
                    id,
                    target,
                    path: PathBuf::from(path),
                }),
                // The CHECK constraint makes this unreachable; a hand-edited file is skipped, not trusted.
                None => tracing::warn!(
                    "pending delete #{id} has an unknown target {target:?}; ignoring it"
                ),
            }
        }
        Ok(out)
    }

    /// Drop a tombstone once its path is gone (or was refused: a path that will never be admitted
    /// must not be retried for ever).
    pub(crate) fn forget_pending(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_deletes WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Whether a live row names `path` as what `target` is. A tombstone whose path a *new* row has
    /// since adopted (the same file name re-recorded while the old unlink kept failing) must not be
    /// removed: it is somebody's footage again.
    pub(crate) fn path_is_indexed(&self, target: Target, path: &Path) -> Result<bool> {
        let sql = match target {
            Target::ClipFile => "SELECT EXISTS(SELECT 1 FROM clips WHERE path = ?1)",
            Target::SessionFile => "SELECT EXISTS(SELECT 1 FROM sessions WHERE final_path = ?1)",
            Target::SessionDir => "SELECT EXISTS(SELECT 1 FROM sessions WHERE scratch_dir = ?1)",
        };
        Ok(self
            .conn
            .query_row(sql, params![path.to_string_lossy()], |r| r.get(0))?)
    }

    /// Rewrite stored paths, each one **only if the row still holds the value the caller read**,
    /// in one transaction. Each item is `(row id, expected current value, new value)`; the three
    /// slices are `clips.path`, `sessions.final_path` and `sessions.scratch_dir`. Returns how many
    /// columns were changed, so a caller can compare it with what it asked for.
    ///
    /// For repairing paths that a move left stale (the data directory was renamed, a library was
    /// relocated). The caller decides *which* rows move, after looking at the disk; this applies
    /// that decision without trusting that nothing happened in between. A row that has since been
    /// rewritten, edited or deleted no longer matches its expected value, and is **left exactly as
    /// it is** (it is not counted and it is not an error): a bulk rewrite computed from an old read
    /// must not overwrite a newer fact, which is the fail-closed side of a compare-and-set. This is
    /// also why it takes explicit rows rather than a prefix: a prefix would move every row under it
    /// whether or not the file is at the new place, and strand the ones it is not.
    ///
    /// Every new value must be an absolute path with no `..`, checked before anything is written:
    /// a relative path in the index would be refused by every executor later and would silently
    /// match a different file depending on the working directory. Moving the files is the
    /// caller's job and is done first. Thumbnails are not in the index (the shell keys its cache by
    /// clip), so nothing here has to follow them.
    ///
    /// All-or-nothing: a `UNIQUE` violation (a row already at the destination) rolls everything back
    /// and is returned, rather than leaving half a library moved.
    pub fn rewrite_paths(
        &self,
        clips: &[PathRewrite],
        session_finals: &[PathRewrite],
        session_scratch: &[PathRewrite],
    ) -> Result<usize> {
        for (_, _, new) in clips.iter().chain(session_finals).chain(session_scratch) {
            if !cleanup::is_absolute_and_clean(Path::new(new)) {
                anyhow::bail!("the new path {new} must be an absolute path with no `..`");
            }
        }
        let tx = self.immediate_tx()?;
        let mut changed = 0usize;
        for (sql, rows) in [
            (
                "UPDATE clips SET path = ?3 WHERE id = ?1 AND path = ?2",
                clips,
            ),
            (
                "UPDATE sessions SET final_path = ?3 WHERE id = ?1 AND final_path = ?2",
                session_finals,
            ),
            (
                "UPDATE sessions SET scratch_dir = ?3 WHERE id = ?1 AND scratch_dir = ?2",
                session_scratch,
            ),
        ] {
            let mut stmt = tx.prepare(sql)?;
            for (id, expected, new) in rows {
                changed += stmt.execute(params![id, expected, new])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }
}

/// Record a path an eviction is handing to the executor, inside the eviction's own transaction.
fn queue_tombstone(tx: &Transaction<'_>, target: Target, path: &str) -> Result<Tombstone> {
    tx.execute(
        "INSERT INTO pending_deletes (target, path, queued_at) VALUES (?1, ?2, ?3)",
        params![target.as_str(), path, now_ms()],
    )?;
    Ok(Tombstone {
        id: tx.last_insert_rowid(),
        target,
        path: PathBuf::from(path),
    })
}

/// The `sessions` columns [`read_session`] expects, in order: one definition, so a query and its reader cannot drift.
const SESSION_COLUMNS: &str = "id, game, mode, started_at, ended_at, scratch_dir, final_path, \
     size_bytes, favourite, media_epoch_ms, duration_ms";

fn read_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        game: row.get(1)?,
        mode: row.get(2)?,
        started_at_ms: row.get(3)?,
        ended_at_ms: row.get(4)?,
        scratch_dir: row.get(5)?,
        final_path: row.get(6)?,
        size_bytes: row.get(7)?,
        favourite: row.get::<_, i64>(8)? != 0,
        media_epoch_ms: row.get(9)?,
        duration_ms: row.get(10)?,
    })
}

/// Whether `table` has a column called `column`.
///
/// `PRAGMA table_info` cannot take a bound parameter, so the table name is interpolated; it is a
/// literal from this file, never caller input. Makes each `ADD COLUMN` idempotent on its own.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Add whichever of `columns` the named table does not already have.
///
/// Idempotent twice over: the version check in [`Store::migrate`] normally skips it, and each
/// column is checked against `PRAGMA table_info` first, so a hand-edited or half-upgraded file
/// upgrades instead of dying on `duplicate column name`. The caller's transaction makes an
/// interrupted upgrade leave the table as it was.
fn add_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> Result<()> {
    for (column, ddl) in columns {
        if !column_exists(conn, table, column)? {
            conn.execute_batch(ddl)?;
        }
    }
    Ok(())
}

/// v4 -> v5: `clips` gains a title and a game, and the tag table appears. The game is back-filled
/// from the clip's session where it has one. Idempotent like [`add_columns`].
fn upgrade_to_v5(conn: &Connection) -> Result<()> {
    add_columns(conn, "clips", &V5_CLIP_COLUMNS)?;
    conn.execute_batch(V5_TAGS_DDL)?;
    conn.execute_batch(
        "UPDATE clips SET game = (SELECT game FROM sessions WHERE sessions.id = clips.session_id)
         WHERE game IS NULL AND session_id IS NOT NULL",
    )?;
    Ok(())
}

fn read_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get(0)?,
        session_id: row.get(1)?,
        kind: row.get(2)?,
        at_ms: to_u64(row.get::<_, i64>(3)?),
        payload: row.get(4)?,
        clip_id: row.get(5)?,
    })
}

/// A `u64` as the signed integer SQLite's columns hold, clamped rather than wrapped.
///
/// The values come from outside the index — a file's size, a duration on the media timeline — and
/// the columns are signed, so a value beyond `i64::MAX` must not wrap into a negative one: a
/// negative size reads as "no bytes" and a negative timestamp as "before the epoch".
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// A signed integer from a row as the `u64` the API hands out, clamped rather than wrapped.
///
/// The inverse of [`to_i64`], and the same rule for a row this build did not write: a negative
/// column is a corrupt row, and `as u64` would turn it into a number near `u64::MAX` rather than
/// into the absence of a value.
fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrating_is_idempotent() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let v1 = s.schema_version().unwrap();
        s.migrate().unwrap();
        assert_eq!(
            s.schema_version().unwrap(),
            v1,
            "re-migrating must not bump the version"
        );
    }

    #[test]
    fn inserts_and_lists_clips_newest_first() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let a = s
            .insert_clip(&NewClip {
                path: "/clips/a.mp4".into(),
                started_at_ms: 1_000,
                duration_ms: 4_000,
                size_bytes: 100,
                codec: "h264".into(),
            })
            .unwrap();
        let _b = s
            .insert_clip(&NewClip {
                path: "/clips/b.mp4".into(),
                started_at_ms: 2_000,
                duration_ms: 4_000,
                size_bytes: 200,
                codec: "h264".into(),
            })
            .unwrap();

        let clips = s.list_clips().unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].path, PathBuf::from("/clips/b.mp4"), "newest first");
        assert_eq!(clips[1].id, a);
    }

    #[test]
    fn rejects_a_duplicate_clip_path() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let clip = NewClip {
            path: "/clips/dup.mp4".into(),
            started_at_ms: 1_000,
            duration_ms: 1_000,
            size_bytes: 1,
            codec: "h264".into(),
        };
        s.insert_clip(&clip).unwrap();
        assert!(s.insert_clip(&clip).is_err(), "path is UNIQUE");
    }

    #[test]
    fn upgrading_a_populated_database_backs_it_up_first_and_keeps_two() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("replayed.db");
        {
            let s = Store::open(&db).unwrap();
            s.migrate().unwrap();
            s.insert_clip(&NewClip {
                path: "/clips/a.mp4".into(),
                started_at_ms: 1,
                duration_ms: 1,
                size_bytes: 1,
                codec: "h264".into(),
            })
            .unwrap();
        }
        let backups = |dir: &Path| {
            let mut v: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".bak"))
                .collect();
            v.sort();
            v
        };
        assert!(
            backups(dir.path()).is_empty(),
            "a fresh database has nothing to back up"
        );

        // Pretend the file is one schema behind, three times over.
        for version in [2, 3, 2] {
            let s = Store::open(&db).unwrap();
            s.conn
                .execute_batch(&format!("PRAGMA user_version = {version}"))
                .unwrap();
            drop(s);
            let s = Store::open(&db).unwrap();
            s.migrate().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let kept = backups(dir.path());
        assert_eq!(kept.len(), 2, "only the two newest survive: {kept:?}");

        // A backup is a real, openable copy that still has the clip.
        let copy = dir.path().join(&kept[0]);
        let n: i64 = rusqlite::Connection::open(copy)
            .unwrap()
            .query_row("SELECT count(*) FROM clips", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Two writers on one database file wait for the lock instead of failing on it.
    ///
    /// The desktop shell and the recorder are separate processes on the same `replayed.db`, and
    /// SQLite's default busy timeout is zero: a write that meets the other connection's lock
    /// fails with `SQLITE_BUSY` at once. The recorder's answer to that is to log an error and
    /// carry on, which leaves the clip's file on disk with no row naming it: invisible in the
    /// library, and unmanaged by the storage policy, which can only manage what the index names.
    ///
    /// So the second writer has to wait, and this test holds the lock open to prove it: the
    /// second write is still pending while the lock is held, and it succeeds once the lock is
    /// released. A store without a busy timeout fails it at once, with "database is locked".
    #[test]
    fn two_writers_on_one_database_wait_for_the_lock_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("replayed.db");
        let first = Store::open(&db).unwrap();
        first.migrate().unwrap();
        let second = Store::open(&db).unwrap();

        // The first writer holds the database's write lock, the way a cleanup pass's bulk
        // delete does while it commits.
        first.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        first
            .conn
            .execute(
                "INSERT INTO clips (path, started_at, duration_ms, size_bytes, codec, created_at)
                 VALUES ('/clips/held.mp4', 1, 1, 1, 'h264', 1)",
                [],
            )
            .unwrap();

        // The second writer writes from another thread, so this thread can release the lock.
        let (tx, rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let id = second.insert_clip(&NewClip {
                path: "/clips/second.mp4".into(),
                started_at_ms: 2,
                duration_ms: 2,
                size_bytes: 2,
                codec: "h264".into(),
            });
            let _ = tx.send(id);
        });

        // It must be waiting, not failing: a store without a busy timeout reports "database is
        // locked" here, and the write is over before this timeout expires.
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(300)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "the second writer must wait for the lock rather than fail with SQLITE_BUSY"
        );

        first.conn.execute_batch("COMMIT").unwrap();
        let id = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the second writer finishes once the lock is released")
            .expect("the second write must not have failed with SQLITE_BUSY");
        writer.join().unwrap();

        let clips = first.list_clips().unwrap();
        assert_eq!(clips.len(), 2, "both writers' rows are in the database");
        assert!(
            clips.iter().any(|c| c.id == id),
            "and the second writer's own row is among them"
        );
    }

    /// A value too large for its column is clamped rather than wrapped into a negative one.
    ///
    /// The columns are signed and the values come from outside the index (a file's size, a
    /// duration on the media timeline). A `u64` beyond `i64::MAX` must not become a negative
    /// number: a negative size reads as "no bytes" and a negative timestamp as "before the
    /// epoch".
    #[test]
    fn a_value_too_large_for_its_column_is_clamped_rather_than_wrapped() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s
            .insert_clip(&NewClip {
                path: "/clips/huge.mp4".into(),
                started_at_ms: 1,
                duration_ms: u64::MAX,
                size_bytes: u64::MAX,
                codec: "h264".into(),
            })
            .unwrap();

        let clip = s
            .list_clips()
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap();
        assert_eq!(
            clip.duration_ms,
            i64::MAX as u64,
            "clamped, not wrapped into a negative duration"
        );
        assert_eq!(
            clip.size_bytes,
            i64::MAX as u64,
            "clamped, not wrapped into a negative size"
        );
        assert_eq!(
            s.total_bytes().unwrap(),
            i64::MAX as u64,
            "and the total counts what the row actually holds"
        );

        // A row this build did not write: a negative column is a corrupt row, and it must read
        // as the absence of a value rather than as a number near `u64::MAX`.
        s.conn
            .execute(
                "UPDATE clips SET size_bytes = -1 WHERE id = ?1",
                params![id],
            )
            .unwrap();
        let clip = s
            .list_clips()
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap();
        assert_eq!(
            clip.size_bytes, 0,
            "a negative size is no size, not a huge one"
        );
    }

    #[test]
    fn records_the_creation_time_of_a_clip() {
        // The age rule ages against `clips.created_at`, which the store stamps itself: a caller cannot
        // backdate a clip into (or out of) deletion.
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let before = now_ms();
        let id = s
            .insert_clip(&NewClip {
                path: "/clips/timed.mp4".into(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: 1,
                codec: "h264".into(),
            })
            .unwrap();
        let after = now_ms();

        let clip = s
            .list_clips()
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap();
        assert!(
            clip.created_at_ms >= before && clip.created_at_ms <= after,
            "got {}",
            clip.created_at_ms
        );
    }

    #[test]
    fn toggles_a_favourite_and_reports_a_missing_id() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s
            .insert_clip(&NewClip {
                path: "/clips/fav.mp4".into(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: 1,
                codec: "h264".into(),
            })
            .unwrap();
        assert!(
            !s.list_clips().unwrap()[0].favourite,
            "clips are not favourites by default"
        );

        assert!(s.set_favourite(id, true).unwrap());
        assert!(s.list_clips().unwrap()[0].favourite);
        assert!(s.set_favourite(id, false).unwrap());
        assert!(!s.list_clips().unwrap()[0].favourite);

        assert!(
            !s.set_favourite(id + 1, true).unwrap(),
            "no such row: the caller must be able to tell a no-op from an update"
        );
    }

    #[test]
    fn sums_the_indexed_bytes() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        assert_eq!(s.total_bytes().unwrap(), 0, "an empty index holds nothing");

        for (path, bytes) in [("/clips/a.mp4", 100u64), ("/clips/b.mp4", 250)] {
            s.insert_clip(&NewClip {
                path: path.into(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: bytes,
                codec: "h264".into(),
            })
            .unwrap();
        }
        assert_eq!(s.total_bytes().unwrap(), 350);
    }

    fn an_event(kind: &str, at_ms: u64, clip_id: Option<i64>) -> NewEvent {
        NewEvent {
            session_id: None,
            kind: kind.to_string(),
            at_ms,
            payload: Some(format!("{{\"source\":\"gsi\",\"event\":\"{kind}\"}}")),
            clip_id,
        }
    }

    #[test]
    fn records_an_event_with_and_without_a_clip() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let clip = s
            .insert_clip(&NewClip {
                path: "/clips/a.mp4".into(),
                started_at_ms: 30_000,
                duration_ms: 35_000,
                size_bytes: 100,
                codec: "h264_nvenc".into(),
            })
            .unwrap();

        // A highlight: the event that produced the clip.
        let kill = s
            .insert_event(&an_event("kill", 34_000, Some(clip)))
            .unwrap();
        // A marker: recorded, no clip.
        let round = s
            .insert_event(&an_event("round_start", 40_000, None))
            .unwrap();
        assert_ne!(kill, round);

        let events = s.list_events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "kill", "oldest first on the media timeline");
        assert_eq!(events[0].at_ms, 34_000);
        assert_eq!(events[0].clip_id, Some(clip));
        assert!(events[0].payload.as_deref().unwrap().contains("kill"));
        assert_eq!(
            events[0].session_id, None,
            "this engine opens no session row"
        );
        assert_eq!(events[1].kind, "round_start");
        assert_eq!(events[1].clip_id, None, "a marker has no clip");

        let for_clip = s.events_for_clip(clip).unwrap();
        assert_eq!(for_clip.len(), 1, "the clip's own events");
        assert_eq!(for_clip[0].id, kill);
        assert!(
            s.events_for_clip(clip + 1).unwrap().is_empty(),
            "no such clip, no events"
        );
    }

    #[test]
    fn events_in_range_is_one_session_half_open_and_ordered() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let mine = s
            .start_session(None, SESSION_MODE_BUFFER, 1, "/scratch/a", 0)
            .unwrap();
        let other = s
            .start_session(None, SESSION_MODE_BUFFER, 2, "/scratch/b", 0)
            .unwrap();
        let at = |session: i64, kind: &str, at_ms: u64| NewEvent {
            session_id: Some(session),
            ..an_event(kind, at_ms, None)
        };
        s.insert_event(&at(mine, "death", 2_000)).unwrap();
        s.insert_event(&at(mine, "kill", 1_000)).unwrap();
        s.insert_event(&at(mine, "before", 999)).unwrap();
        s.insert_event(&at(mine, "at_end", 3_000)).unwrap();
        s.insert_event(&at(other, "elsewhere", 1_500)).unwrap();

        let kinds: Vec<String> = s
            .events_in_range(mine, 1_000, 3_000)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["kill", "death"]);
    }

    #[test]
    fn an_event_cannot_point_at_a_clip_that_does_not_exist() {
        // `events.clip_id` is a foreign key with enforcement on, so a marker cannot attach to a clip that was never written.
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        assert!(
            s.insert_event(&an_event("kill", 1_000, Some(42))).is_err(),
            "there is no clip 42"
        );
        assert!(
            s.list_events().unwrap().is_empty(),
            "and nothing was written"
        );
    }

    #[test]
    fn an_event_with_no_payload_is_a_row_like_any_other() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        s.insert_event(&NewEvent {
            session_id: None,
            kind: "game_start".into(),
            at_ms: 0,
            payload: None,
            clip_id: None,
        })
        .unwrap();
        let events = s.list_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].payload, None,
            "the column is nullable and stays null"
        );
    }

    #[test]
    fn deleting_a_clip_releases_its_events_instead_of_failing() {
        // Evicting a clip an event names must detach the event, not be refused, or the storage policy
        // is unusable once an integration is on (`events.clip_id` is a foreign key).
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let clip = s
            .insert_clip(&NewClip {
                path: "/clips/gone.mp4".into(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: 10,
                codec: "h264".into(),
            })
            .unwrap();
        s.insert_event(&an_event("kill", 1_000, Some(clip)))
            .unwrap();

        // Delete the clip as the executor does: row first, then the file (none here, which the executor tolerates).
        assert_eq!(
            s.delete_clip_returning_path(clip).unwrap().as_deref(),
            Some("/clips/gone.mp4")
        );
        let events = s.list_events().unwrap();
        assert_eq!(
            events.len(),
            1,
            "the event row survives the clip it pointed at"
        );
        assert_eq!(
            events[0].clip_id, None,
            "and no longer names a row that is gone"
        );
        assert!(s.list_clips().unwrap().is_empty());
    }

    #[test]
    fn delete_before_unlink_ordering_is_exposed_to_the_caller() {
        // The store deletes the row and hands back the path, so the caller unlinks afterwards: no file
        // is deleted without a committed row naming it.
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s
            .insert_clip(&NewClip {
                path: "/clips/c.mp4".into(),
                started_at_ms: 1_000,
                duration_ms: 1_000,
                size_bytes: 1,
                codec: "h264".into(),
            })
            .unwrap();

        let removed = s.delete_clip_returning_path(id).unwrap();
        assert_eq!(removed.as_deref(), Some("/clips/c.mp4"));
        assert_eq!(
            s.delete_clip_returning_path(id).unwrap(),
            None,
            "second delete is a no-op"
        );
    }

    // ------------------------------------------------------------- sessions

    /// A session that is running: opened, not ended, and nothing on disk yet.
    fn running_session(store: &Store) -> i64 {
        store
            .start_session(
                Some("League of Legends"),
                SESSION_MODE_SESSION,
                1_000_000,
                "/scratch/1",
                0,
            )
            .expect("open the session")
    }

    /// `clips.session_id` for one row.
    ///
    /// `Clip` does not carry the column and the session recorder writes the link, so a test reads
    /// it the way an operator would.
    fn clip_session_id(store: &Store, clip_id: i64) -> Option<i64> {
        store
            .conn
            .query_row(
                "SELECT session_id FROM clips WHERE id = ?1",
                params![clip_id],
                |r| r.get(0),
            )
            .expect("the clip row")
    }

    #[test]
    fn a_session_is_opened_read_back_and_ended() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);

        let running = s
            .get_session(id)
            .expect("the session")
            .expect("it is there");
        assert_eq!(running.id, id);
        assert_eq!(running.game.as_deref(), Some("League of Legends"));
        assert_eq!(running.mode, SESSION_MODE_SESSION);
        assert_eq!(running.started_at_ms, 1_000_000);
        assert_eq!(
            running.ended_at_ms, None,
            "a session is running until it is ended"
        );
        assert_eq!(running.scratch_dir, "/scratch/1");
        assert_eq!(running.final_path, None, "there is no session file yet");
        assert_eq!(running.size_bytes, 0);
        assert_eq!(
            running.duration_ms, 0,
            "and no media length either: a session that has not been concatenated has no \
             probed duration, and 0 means unknown rather than zero-length"
        );
        assert!(!running.favourite, "a new session is not a favourite");

        s.end_session(id, 1_120_000, Some("/sessions/1.mp4"), 4_096, 118_400)
            .expect("the session ends");
        let ended = s
            .get_session(id)
            .expect("the session")
            .expect("it is still there");
        assert_eq!(ended.ended_at_ms, Some(1_120_000));
        assert_eq!(ended.final_path.as_deref(), Some("/sessions/1.mp4"));
        assert_eq!(ended.size_bytes, 4_096);
        assert_eq!(
            ended.duration_ms, 118_400,
            "the media length the row carries, which is what a review timeline is drawn on"
        );
        // Deliberately different from the wall-clock window (120s) in this fixture: equal values could not tell the two apart.
        assert_ne!(
            ended.duration_ms,
            ended.ended_at_ms.unwrap() - ended.started_at_ms,
            "media length is not the wall clock the window was open for"
        );
    }

    #[test]
    fn a_session_may_have_no_game_and_an_unknown_mode_is_refused() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = s
            .start_session(None, SESSION_MODE_BUFFER, 5, "/scratch/2", 0)
            .expect("the session opens");
        assert_eq!(
            s.get_session(id)
                .expect("the session")
                .expect("the session row")
                .game,
            None,
            "a session with no game"
        );

        // A typo must not be stored: every reader of `mode` switches on the column.
        let err = s
            .start_session(None, "ful-session", 6, "/scratch/3", 0)
            .expect_err("an unknown mode is refused");
        assert!(
            format!("{err:#}").contains("unknown session mode"),
            "{err:#}"
        );
        assert_eq!(
            s.list_sessions().expect("the sessions").len(),
            1,
            "and nothing was written"
        );
    }

    #[test]
    fn ending_a_session_twice_is_reported_and_the_first_finalisation_stands() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);
        s.end_session(id, 1_100_000, Some("/sessions/1.mp4"), 4_096, 0)
            .expect("the session ends");

        let err = s
            .end_session(id, 1_200_000, Some("/sessions/other.mp4"), 9_999, 0)
            .expect_err("a second finalisation is refused, not silently ignored");
        assert!(format!("{err:#}").contains("already ended"), "{err:#}");

        let row = s
            .get_session(id)
            .expect("the session")
            .expect("the session row");
        assert_eq!(
            row.ended_at_ms,
            Some(1_100_000),
            "the first end time still stands"
        );
        assert_eq!(row.final_path.as_deref(), Some("/sessions/1.mp4"));
        assert_eq!(row.size_bytes, 4_096);
    }

    #[test]
    fn ending_a_session_that_does_not_exist_is_reported_rather_than_assumed() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let err = s
            .end_session(42, 1, None, 0, 0)
            .expect_err("there is no session 42 — a silent Ok would read as success");
        assert!(format!("{err:#}").contains("no such session"), "{err:#}");
        assert!(s.list_sessions().expect("the sessions").is_empty());
    }

    #[test]
    fn getting_a_session_that_does_not_exist_is_none_with_no_events() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        assert_eq!(
            s.get_session(7).expect("the session"),
            None,
            "no such session, no error, no panic"
        );
        assert!(
            s.events_for_session(7).expect("the timeline").is_empty(),
            "and no timeline to plot"
        );
    }

    #[test]
    fn sessions_are_listed_newest_started_first() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let first = s
            .start_session(None, SESSION_MODE_BUFFER, 1_000, "/scratch/a", 0)
            .expect("the session opens");
        let second = s
            .start_session(None, SESSION_MODE_BUFFER, 2_000, "/scratch/b", 0)
            .expect("the session opens");
        // A tie on `started_at`: the id breaks it, so the order is total.
        let tied = s
            .start_session(None, SESSION_MODE_BUFFER, 2_000, "/scratch/c", 0)
            .expect("the session opens");

        assert_eq!(
            s.list_sessions()
                .expect("the sessions")
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            vec![tied, second, first],
            "most recently started first"
        );
    }

    #[test]
    fn a_session_timeline_is_chronological_with_offsets_from_the_start() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        // Two clocks, deliberately different numbers: the timeline is measured from the media epoch, not
        // the wall start. Subtracting `started_at` from `events.at` mixes two clocks and misplaces every
        // event of a session that reused a scratch directory, which equal fixture values would hide.
        let session = s
            .start_session(
                Some("League of Legends"),
                SESSION_MODE_SESSION,
                1_700_000_000_000, // wall: when the recording began
                "/scratch/timeline",
                1_000_000, // media: where the ledger stood when it began
            )
            .expect("open the session");
        let clip = s
            .insert_clip(&NewClip {
                path: "/clips/kill.mp4".into(),
                started_at_ms: 30_000,
                duration_ms: 12_000,
                size_bytes: 100,
                codec: "h264_nvenc".into(),
            })
            .expect("the clip is indexed");

        // Inserted out of order, of every provenance: a manual hotkey bookmark, a game-derived kill that
        // produced a clip, a game-derived death, a marker with no clip, and one event before the
        // session's own start.
        for (kind, at, clip_id) in [
            ("kill", 1_030_000u64, Some(clip)),
            ("bookmark", 1_000_000, None),
            ("death", 1_045_000, None),
            ("round_start", 1_020_000, None),
            ("assist", 999_999, None),
        ] {
            s.insert_event(&NewEvent {
                session_id: Some(session),
                kind: kind.into(),
                at_ms: at,
                payload: Some(format!("{{\"source\":\"test\",\"event\":\"{kind}\"}}")),
                clip_id,
            })
            .expect("the event is written");
        }
        // An event of another session must not appear; nor must one with no session.
        let other = s
            .start_session(None, SESSION_MODE_BUFFER, 2_000_000, "/scratch/other", 0)
            .expect("the session opens");
        s.insert_event(&an_event("kill", 2_000_001, None))
            .expect("the event is written");
        s.insert_event(&an_event("kill", 2_000_002, None))
            .expect("the event is written");
        s.conn
            .execute(
                "UPDATE events SET session_id = ?2 WHERE at = ?1",
                params![2_000_001, other],
            )
            .expect("the statement");

        let timeline = s.events_for_session(session).expect("the timeline");
        assert_eq!(
            timeline
                .iter()
                .map(|e| (e.kind.as_str(), e.offset_ms))
                .collect::<Vec<_>>(),
            vec![
                ("assist", -1),
                ("bookmark", 0),
                ("round_start", 20_000),
                ("kill", 30_000),
                ("death", 45_000),
            ],
            "oldest first, offsets from the session start, and nothing filtered by kind"
        );
        assert_eq!(
            timeline[1].offset_ms, 0,
            "an event at the session's own start instant is exactly 0"
        );
        assert_eq!(
            timeline[3].clip_id,
            Some(clip),
            "the clip an event produced travels too"
        );
        assert!(
            timeline[1].clip_id.is_none(),
            "a bookmark that produced no clip says so"
        );
        assert!(timeline[3]
            .payload
            .as_deref()
            .expect("the payload is the JSON the caller stored")
            .contains("kill"));

        let other_timeline = s.events_for_session(other).expect("the timeline");
        assert_eq!(other_timeline.len(), 1, "one event on the other session");
    }

    #[test]
    fn favouriting_a_session_reports_a_missing_id() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);

        s.set_session_favourite(id, true).expect("the toggle");
        assert!(
            s.get_session(id)
                .expect("the session")
                .expect("the session row")
                .favourite
        );
        s.set_session_favourite(id, false).expect("the toggle");
        assert!(
            !s.get_session(id)
                .expect("the session")
                .expect("the session row")
                .favourite
        );

        let err = s
            .set_session_favourite(id + 1, true)
            .expect_err("no such session");
        assert!(format!("{err:#}").contains("no session"), "{err:#}");
    }

    #[test]
    fn a_running_sessions_size_is_what_the_cap_can_see() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);
        assert_eq!(
            s.total_session_bytes().expect("the session store's bytes"),
            0,
            "an empty session store holds nothing"
        );

        // A recorder keeps the running total current on its scan tick; without this the sessions cap
        // would plan against zero bytes for a live recording.
        s.set_session_size(id, 3_000).expect("the running size");
        assert_eq!(
            s.total_session_bytes().expect("the session store's bytes"),
            3_000
        );

        // Once it has ended, `end_session` owns the number.
        s.end_session(id, 1_100_000, Some("/sessions/1.mp4"), 4_096, 0)
            .expect("the session ends");
        assert_eq!(
            s.total_session_bytes().expect("the session store's bytes"),
            4_096
        );
        assert!(
            s.set_session_size(id, 1).is_err(),
            "a finished session's size is final"
        );
        assert!(
            s.set_session_size(id + 1, 1).is_err(),
            "and there is no session to size"
        );
    }

    #[test]
    fn deleting_a_session_detaches_its_events_and_keeps_its_clips() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let session = running_session(&s);
        s.end_session(session, 1_120_000, Some("/sessions/1.mp4"), 9_000, 0)
            .expect("the session ends");

        // A clip extracted from the session. `insert_clip` takes no session_id, so the link is written
        // as the session recorder writes it; the foreign key would refuse the delete below if the store
        // did not detach it.
        let clip = s
            .insert_clip(&NewClip {
                path: "/clips/from-session.mp4".into(),
                started_at_ms: 30_000,
                duration_ms: 12_000,
                size_bytes: 100,
                codec: "h264_nvenc".into(),
            })
            .expect("the clip is indexed");
        s.conn
            .execute(
                "UPDATE clips SET session_id = ?2 WHERE id = ?1",
                params![clip, session],
            )
            .expect("the statement");
        assert_eq!(
            clip_session_id(&s, clip),
            Some(session),
            "the clip is linked to the session"
        );
        let event = s
            .insert_event(&NewEvent {
                session_id: Some(session),
                kind: "kill".into(),
                at_ms: 1_030_000,
                payload: None,
                clip_id: Some(clip),
            })
            .expect("the event is written");

        let paths = s
            .delete_session_returning_paths(session)
            .expect("the session is deleted");
        assert_eq!(
            paths,
            Some(SessionPaths {
                final_path: Some("/sessions/1.mp4".into()),
                scratch_dir: "/scratch/1".into(),
            }),
            "the row names what the caller must remove, and nothing else"
        );

        assert_eq!(
            s.get_session(session).expect("the session"),
            None,
            "the session row is gone"
        );
        let clips = s.list_clips().expect("the clips");
        assert_eq!(
            clips.len(),
            1,
            "an extracted clip is a first-class artefact and survives"
        );
        assert_eq!(clips[0].id, clip);
        assert_eq!(clips[0].path, PathBuf::from("/clips/from-session.mp4"));
        assert_eq!(
            clip_session_id(&s, clip),
            None,
            "only the link to the gone row is cleared"
        );
        let events = s.list_events().expect("the events");
        assert_eq!(events.len(), 1, "the event survives as history");
        assert_eq!(events[0].id, event);
        assert_eq!(
            events[0].session_id, None,
            "and no longer names a row that is gone"
        );
        assert_eq!(
            events[0].clip_id,
            Some(clip),
            "the clip it produced is still there"
        );
        assert!(
            s.events_for_session(session)
                .expect("the timeline")
                .is_empty(),
            "the timeline of a session that no longer exists is empty, not an error"
        );
        assert_eq!(
            s.delete_session_returning_paths(session)
                .expect("the session is deleted"),
            None,
            "a second delete is a no-op"
        );
    }

    #[test]
    fn a_session_that_is_still_recording_cannot_be_deleted() {
        let s = Store::open_in_memory().expect("an in-memory store");
        s.migrate().expect("migrate the store");
        let id = running_session(&s);

        // The scratch directory of a running session is the recording, with no second copy, so no ordering makes deleting it safe.
        let err = s
            .delete_session_returning_paths(id)
            .expect_err("a running session must not be deletable");
        assert!(format!("{err:#}").contains("still recording"), "{err:#}");
        assert!(
            s.get_session(id).expect("the session").is_some(),
            "the row is untouched"
        );

        // The escape hatch is explicit: end it and it becomes an ordinary deletable session. A session
        // left running by a crash is the same case; this is how a recovery pass resolves it.
        s.end_session(id, 1_200_000, None, 0, 0)
            .expect("the session ends");
        let paths = s
            .delete_session_returning_paths(id)
            .expect("the session is deleted");
        assert_eq!(
            paths,
            Some(SessionPaths {
                final_path: None,
                scratch_dir: "/scratch/1".into()
            }),
            "a session with no final file hands back only its scratch directory"
        );
        assert_eq!(
            s.delete_session_returning_paths(id + 1)
                .expect("the session is deleted"),
            None,
            "no such row"
        );
    }

    /// A database with the v1 tables, v1's `user_version`, and a row in each table.
    fn v1_database(path: &Path) -> Store {
        let conn = Connection::open(path).expect("open the v1 database");
        conn.execute_batch(V1_DDL)
            .expect("the v1 tables, exactly as v1 shipped them");
        conn.execute_batch("PRAGMA user_version = 1")
            .expect("stamp v1");
        conn.execute(
            "INSERT INTO sessions (id, game, started_at, ended_at, scratch_dir)
             VALUES (1, 'League of Legends', 1000, 61000, '/app/scratch')",
            [],
        )
        .expect("a v1 session row");
        conn.execute(
            "INSERT INTO clips (id, session_id, path, started_at, duration_ms, size_bytes, codec, created_at)
             VALUES (1, 1, '/app/clips/kill.mp4', 30000, 12000, 500000, 'h264_nvenc', 7000)",
            [],
        )
        .expect("a v1 clip row, linked to the session");
        conn.execute(
            "INSERT INTO events (id, session_id, kind, at, payload, clip_id)
             VALUES (1, 1, 'kill', 30000, '{\"source\":\"gsi\"}', 1)",
            [],
        )
        .expect("a v1 event row");
        drop(conn);
        Store::open(path).expect("open the v1 database through the store")
    }

    /// The column names, declared types and nullability of `table`, as SQLite sees them.
    fn columns(store: &Store, table: &str) -> Vec<(String, String, i64)> {
        let mut stmt = store
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("table_info");
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .expect("query");
        rows.collect::<rusqlite::Result<Vec<_>>>().expect("columns")
    }

    #[test]
    fn a_v1_database_is_upgraded_in_place_and_loses_no_rows() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("replayed.db");
        let store = v1_database(&path);
        assert_eq!(
            store.schema_version().expect("version"),
            1,
            "the fixture is a v1 database"
        );

        store.migrate().expect("the upgrade");

        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION,
            "the upgrade lands on the version this build writes"
        );

        // The v1 rows are all still there, with the values they had.
        let sessions = store.list_sessions().expect("the sessions");
        assert_eq!(sessions.len(), 1, "the legacy session survived");
        assert_eq!(sessions[0].game.as_deref(), Some("League of Legends"));
        assert_eq!(
            sessions[0].media_epoch_ms, 0,
            "a legacy row's media epoch is 0 — the only value its own contents support, and \
             the correct one for the ordinary case of a fresh scratch directory"
        );
        assert_eq!(
            sessions[0].duration_ms, 0,
            "and so is its media length: a row this build never concatenated has no probed \
             duration, and 0 means unknown rather than zero-length"
        );
        assert_eq!(sessions[0].started_at_ms, 1_000);
        assert_eq!(sessions[0].ended_at_ms, Some(61_000));
        assert_eq!(sessions[0].scratch_dir, "/app/scratch");
        let clips = store.list_clips().expect("the clips");
        assert_eq!(clips.len(), 1, "the legacy clip survived");
        assert_eq!(clips[0].path, PathBuf::from("/app/clips/kill.mp4"));
        assert_eq!(clips[0].size_bytes, 500_000);
        assert_eq!(clips[0].created_at_ms, 7_000);
        let events = store.list_events().expect("the events");
        assert_eq!(events.len(), 1, "the legacy event survived");
        assert_eq!(events[0].session_id, Some(1), "and still names its session");
        assert_eq!(events[0].clip_id, Some(1));

        // The new columns exist, with the defaults a legacy row must get.
        assert_eq!(
            sessions[0].mode, SESSION_MODE_BUFFER,
            "a legacy row was written by the buffer engine or not at all: 'buffer' is the \
             only mode that explains its own contents"
        );
        assert_eq!(
            sessions[0].final_path, None,
            "a legacy session has no session file"
        );
        assert_eq!(
            sessions[0].size_bytes, 0,
            "and no recorded size to invent one from"
        );
        assert!(
            !sessions[0].favourite,
            "favourites default to not favourited"
        );
        for column in ["mode", "final_path", "size_bytes", "favourite"] {
            assert!(
                column_exists(&store.conn, "sessions", column).expect("table_info"),
                "{column}"
            );
        }

        // The upgraded shape is the shape a new database gets: no drift between created and upgraded databases.
        let fresh = Store::open_in_memory().expect("a fresh store");
        fresh.migrate().expect("migrate");
        assert_eq!(columns(&store, "sessions"), columns(&fresh, "sessions"));
        assert_eq!(columns(&store, "clips"), columns(&fresh, "clips"));
        assert_eq!(columns(&store, "events"), columns(&fresh, "events"));

        // The legacy session's timeline is readable through the new API. Its epoch is the migration's
        // default (0, the only value its contents support, since it predates v3), so the offset is the
        // event's own media position, which for a session against a fresh scratch directory is the
        // session-relative offset.
        let timeline = store.events_for_session(1).expect("the legacy timeline");
        assert_eq!(timeline.len(), 1);
        assert_eq!(
            timeline[0].offset_ms, 30_000,
            "a legacy row's epoch defaults to 0, so the offset is the event's own position"
        );
        store.migrate().expect("migrating again");
        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION,
            "and it stays at the current version"
        );
        assert_eq!(store.list_sessions().expect("sessions").len(), 1);
    }

    #[test]
    fn the_v1_to_v2_step_is_safe_on_a_database_that_already_has_a_v2_column() {
        // An upgrade interrupted by a build that added columns one at a time, or a hand-edited file:
        // `ADD COLUMN` is not idempotent, so the step checks each column first.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("half-migrated.db");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(V1_DDL).expect("v1 tables");
            conn.execute_batch(
                "ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'buffer'",
            )
            .expect("one of v2's columns is already there");
            conn.execute_batch("PRAGMA user_version = 1")
                .expect("still stamped v1");
            conn.execute(
                "INSERT INTO sessions (id, game, started_at, ended_at, scratch_dir, mode)
                 VALUES (1, NULL, 5, 6, '/scratch/x', 'session')",
                [],
            )
            .expect("a row that already has a mode");
        }

        let store = Store::open(&path).expect("open through the store");
        store
            .migrate()
            .expect("the upgrade must not die on a duplicate column");
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
        for column in [
            "mode",
            "final_path",
            "size_bytes",
            "favourite",
            "media_epoch_ms",
        ] {
            assert!(
                column_exists(&store.conn, "sessions", column).expect("table_info"),
                "{column}"
            );
        }
        let session = store
            .get_session(1)
            .expect("read")
            .expect("the row survived");
        assert_eq!(
            session.mode, SESSION_MODE_SESSION,
            "an existing value is never overwritten"
        );
    }

    #[test]
    fn a_database_newer_than_this_build_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("from-the-future.db");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(V1_DDL).expect("tables");
            conn.execute_batch("PRAGMA user_version = 99")
                .expect("a newer build's version");
            conn.execute(
                "INSERT INTO sessions (id, game, started_at, ended_at, scratch_dir)
                 VALUES (1, NULL, 1, 2, '/scratch/future')",
                [],
            )
            .expect("a row");
        }

        let store = Store::open(&path).expect("open through the store");
        let err = store
            .migrate()
            .expect_err("a newer schema must be refused, never downgraded");
        assert!(
            format!("{err:#}").contains("newer than this build supports"),
            "{err:#}"
        );
        assert_eq!(
            store.schema_version().expect("version"),
            99,
            "the version is not rewritten"
        );
        // Read through SQL: a refused v99 database has no `mode` column to read, which is the
        // fail-closed outcome. What matters is that the row is still there.
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .expect("the v1 row is readable");
        assert_eq!(rows, 1, "and nothing was truncated");
        assert!(
            !column_exists(&store.conn, "sessions", "final_path").expect("table_info"),
            "a refused migration changes no structure either"
        );
    }

    // -- schema v5: clip title, game, tags --

    fn new_clip(path: &str) -> NewClip {
        NewClip {
            path: path.into(),
            started_at_ms: 1,
            duration_ms: 1_000,
            size_bytes: 1,
            codec: "h264".into(),
        }
    }

    #[test]
    fn a_screenshot_is_indexed_as_a_clip_row_with_no_duration_and_its_own_kind() {
        // The design choice this pins: one table for both, because a screenshot is kept, tagged and
        // protected from the storage policy exactly like a clip, and the library is one list.
        let store = Store::open_in_memory().unwrap();
        store.migrate().unwrap();

        let id = store
            .insert_screenshot(&NewScreenshot {
                path: "/clips/screenshot-1.png".into(),
                size_bytes: 40_000,
            })
            .unwrap();

        let clips = store.list_clips().unwrap();
        assert_eq!(clips.len(), 1);
        let shot = &clips[0];
        assert_eq!(shot.id, id);
        assert_eq!(shot.kind, CLIP_KIND_SCREENSHOT);
        assert!(shot.is_screenshot());
        assert_eq!(shot.duration_ms, 0, "a still image has no length");
        assert_eq!(shot.codec, "png");
        assert_eq!(shot.size_bytes, 40_000);
        // And it is a first-class row: tags and favourites work on it.
        assert!(store.set_clip_tags(id, &["ace".into()]).unwrap());
        assert!(store.set_favourite(id, true).unwrap());
        let again = store.list_clips().unwrap();
        assert_eq!(again[0].tags, vec!["ace"]);
        assert!(again[0].favourite);
    }

    #[test]
    fn a_v5_database_gains_the_kind_column_and_its_rows_stay_videos() {
        // The upgrade path a released v0.2.0 library takes. The column is added with a default, so
        // every existing row is what it always was: a video.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("replayed.db");
        let store = Store::open(&db).unwrap();
        store.migrate().unwrap();
        let id = store.insert_clip(&new_clip("/clips/a.mp4")).unwrap();
        // Rewind the file to v5 by dropping the column, as the released build's schema had it.
        store
            .conn
            .execute("ALTER TABLE clips DROP COLUMN kind", [])
            .unwrap();
        store.conn.execute("PRAGMA user_version = 5", []).unwrap();
        assert_eq!(store.schema_version().unwrap(), 5);
        assert!(!column_exists(&store.conn, "clips", "kind").unwrap());

        store.migrate().unwrap();

        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let clips = store.list_clips().unwrap();
        assert_eq!(clips.len(), 1, "no row lost");
        assert_eq!(clips[0].id, id);
        assert_eq!(clips[0].kind, CLIP_KIND_CLIP);
    }

    #[test]
    fn a_v1_database_gains_the_v5_columns_backfills_the_game_and_loses_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("replayed.db");
        let store = v1_database(&db);
        store.migrate().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

        let clips = store.list_clips().unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].path, PathBuf::from("/app/clips/kill.mp4"));
        assert_eq!(
            clips[0].game.as_deref(),
            Some("League of Legends"),
            "game back-filled from the session"
        );
        assert_eq!(
            clips[0].kind, CLIP_KIND_CLIP,
            "a pre-v6 row is a video: there was no way to make a screenshot before this build"
        );
        assert_eq!(clips[0].title, None);
        assert!(clips[0].tags.is_empty());
        assert_eq!(
            store.events_for_clip(1).unwrap().len(),
            1,
            "the event is untouched"
        );
    }

    /// `set_clip_tags` reads (does the clip exist?) and then writes. In a deferred transaction that
    /// is a read lock being promoted to a write lock, which SQLite refuses at once when another
    /// connection holds the write lock, without ever calling the busy handler: the five-second
    /// timeout the rest of this file relies on does not apply, and the desktop's tag edit failed with
    /// "database is locked" whenever it landed inside one of the recorder's 200 ms size writes.
    #[test]
    fn a_tag_edit_waits_for_the_write_lock_instead_of_failing_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("replayed.db");
        let recorder = Store::open(&db).unwrap();
        recorder.migrate().unwrap();
        let id = recorder.insert_clip(&new_clip("/clips/a.mp4")).unwrap();
        let desktop = Store::open(&db).unwrap();

        recorder.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        recorder
            .conn
            .execute("UPDATE clips SET size_bytes = 2 WHERE id = ?1", params![id])
            .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let editor = std::thread::spawn(move || {
            let _ = tx.send(desktop.set_clip_tags(id, &["ace".to_string()]));
        });
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(300)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "the tag edit must wait for the lock, not fail with SQLITE_BUSY"
        );
        recorder.conn.execute_batch("COMMIT").unwrap();
        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the edit finishes once the lock is released");
        assert!(result.expect("and succeeds"));
        editor.join().unwrap();
    }

    /// The desktop, the recorder and the CLI all call `migrate` on first start after an upgrade.
    /// Both must succeed and the file must end at the current version, with no staging file left
    /// beside the backups.
    #[test]
    fn two_processes_migrating_the_same_old_database_both_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("replayed.db");
        {
            let s = Store::open(&db).unwrap();
            s.migrate().unwrap();
            s.insert_clip(&new_clip("/clips/a.mp4")).unwrap();
        }
        for round in 0..8 {
            // Put the file back to what a v5 build left, every round, so the race is run repeatedly.
            {
                let s = Store::open(&db).unwrap();
                s.conn.execute_batch("PRAGMA user_version = 5").unwrap();
                let _ = s.conn.execute_batch(
                    "DROP TABLE IF EXISTS pending_deletes; DROP INDEX IF EXISTS idx_events_clip;",
                );
                if column_exists(&s.conn, "clips", "kind").unwrap() {
                    s.conn
                        .execute_batch("ALTER TABLE clips DROP COLUMN kind")
                        .unwrap();
                }
            }
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    let (db, barrier) = (db.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        let s = Store::open(&db).unwrap();
                        barrier.wait();
                        s.migrate()
                    })
                })
                .collect();
            for worker in workers {
                worker
                    .join()
                    .unwrap()
                    .unwrap_or_else(|err| panic!("round {round}: a migrator failed: {err:#}"));
            }
            let s = Store::open(&db).unwrap();
            assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
            assert_eq!(s.list_clips().unwrap().len(), 1);
        }
        let stray: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(
            stray.is_empty(),
            "no staging file is left behind: {stray:?}"
        );
    }

    #[test]
    fn v7_adds_the_events_clip_index_and_the_pending_deletes_table_without_losing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("replayed.db");
        let store = Store::open(&db).unwrap();
        store.migrate().unwrap();
        let id = store.insert_clip(&new_clip("/clips/a.mp4")).unwrap();
        // Put the file back to v6: no index, no table.
        store
            .conn
            .execute_batch(
                "DROP TABLE pending_deletes; DROP INDEX idx_events_clip; PRAGMA user_version = 6;",
            )
            .unwrap();

        store.migrate().unwrap();

        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let index: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_events_clip'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1, "events.clip_id is indexed");
        let table: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM pending_deletes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(table, 0, "the tombstone table exists and starts empty");
        assert_eq!(store.list_clips().unwrap()[0].id, id, "no row lost");
        let plan: String = store
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN UPDATE events SET clip_id = NULL WHERE clip_id = 1",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_events_clip"),
            "the detach uses it: {plan}"
        );
    }

    fn ended_session(s: &Store, mode: &str, scratch: &str, final_path: Option<&str>) -> i64 {
        let id = s.start_session(None, mode, 1_000, scratch, 0).unwrap();
        s.end_session(id, 2_000, final_path, 10, 1_000).unwrap();
        id
    }

    #[test]
    fn evicting_a_favourited_clip_deletes_nothing_and_queues_nothing() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s.insert_clip(&new_clip("/clips/a.mp4")).unwrap();
        s.set_favourite(id, true).unwrap();

        assert_eq!(s.evict_clip(id).unwrap(), Eviction::Protected);

        assert_eq!(s.list_clips().unwrap().len(), 1, "the favourite survives");
        assert!(
            s.pending_deletes().unwrap().is_empty(),
            "and nothing is queued for removal"
        );
    }

    #[test]
    fn evicting_a_clip_removes_the_row_and_queues_its_path_in_one_commit() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s.insert_clip(&new_clip("/clips/a.mp4")).unwrap();
        let tag_id = s.insert_clip(&new_clip("/clips/b.mp4")).unwrap();
        s.set_clip_tags(tag_id, &["ace".into()]).unwrap();

        let Eviction::Evicted(tombstone) = s.evict_clip(id).unwrap() else {
            panic!("an ordinary clip is evicted");
        };
        assert_eq!(tombstone.path, PathBuf::from("/clips/a.mp4"));
        assert_eq!(tombstone.target, Target::ClipFile);
        assert_eq!(s.pending_deletes().unwrap(), vec![tombstone.clone()]);
        assert_eq!(s.list_clips().unwrap().len(), 1);
        assert_eq!(
            s.evict_clip(id).unwrap(),
            Eviction::Gone,
            "a second eviction finds nothing"
        );
        assert!(
            matches!(s.evict_clip(tag_id).unwrap(), Eviction::Evicted(_)),
            "a tagged clip goes too"
        );

        s.forget_pending(tombstone.id).unwrap();
        assert_eq!(
            s.pending_deletes().unwrap().len(),
            1,
            "only the forgotten one is dropped"
        );
    }

    #[test]
    fn evicting_a_session_queues_its_file_and_its_own_scratch_directory() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = ended_session(
            &s,
            SESSION_MODE_SESSION,
            "/sessions/session-1",
            Some("/sessions/session-1.mp4"),
        );

        let Eviction::Evicted(queued) = s.evict_session(id).unwrap() else {
            panic!("an ended session is evicted");
        };
        let targets: Vec<_> = queued.iter().map(|t| (t.target, t.path.clone())).collect();
        assert_eq!(
            targets,
            vec![
                (
                    Target::SessionFile,
                    PathBuf::from("/sessions/session-1.mp4")
                ),
                (Target::SessionDir, PathBuf::from("/sessions/session-1")),
            ]
        );
        assert!(s.list_sessions().unwrap().is_empty());
    }

    /// The replay buffer's rows name the one shared, user-configurable scratch directory, which
    /// the row does not own. Evicting such a row must never queue it.
    #[test]
    fn evicting_a_buffer_session_never_queues_the_shared_scratch_directory() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = ended_session(&s, SESSION_MODE_BUFFER, "/data/Replays", None);

        let Eviction::Evicted(queued) = s.evict_session(id).unwrap() else {
            panic!("the row is evicted");
        };
        assert!(
            queued.is_empty(),
            "a buffer row owns no scratch tree: {queued:?}"
        );
        assert!(s.pending_deletes().unwrap().is_empty());
    }

    #[test]
    fn a_scratch_directory_another_row_still_names_is_not_queued() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let first = ended_session(&s, SESSION_MODE_SESSION, "/sessions/session-1", None);
        let _second = ended_session(&s, SESSION_MODE_SESSION, "/sessions/session-1", None);
        let Eviction::Evicted(queued) = s.evict_session(first).unwrap() else {
            panic!("evicted");
        };
        assert!(queued.is_empty(), "the second row still owns the directory");
    }

    #[test]
    fn evicting_a_favourite_or_a_running_session_changes_nothing() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let fav = ended_session(&s, SESSION_MODE_SESSION, "/sessions/session-1", None);
        s.set_session_favourite(fav, true).unwrap();
        assert_eq!(s.evict_session(fav).unwrap(), Eviction::Protected);

        let running = s
            .start_session(None, SESSION_MODE_SESSION, 5, "/sessions/session-2", 0)
            .unwrap();
        assert!(
            s.evict_session(running).is_err(),
            "a running session is refused, loudly"
        );
        assert_eq!(s.list_sessions().unwrap().len(), 2);
        assert_eq!(s.evict_session(999).unwrap(), Eviction::Gone);
        assert!(s.pending_deletes().unwrap().is_empty());
    }

    fn clip_paths(s: &Store) -> Vec<String> {
        let mut paths: Vec<String> = s
            .list_clips()
            .unwrap()
            .into_iter()
            .map(|c| c.path.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        paths
    }

    /// A path that is absolute on the host: Windows wants a drive, and a store that refuses
    /// relative destinations would refuse the bare `/old/a.mp4` there.
    fn p(unix: &str) -> String {
        if cfg!(windows) {
            format!("C:{unix}")
        } else {
            unix.to_string()
        }
    }

    fn rewrite(id: i64, from: &str, to: &str) -> PathRewrite {
        (id, from.to_string(), to.to_string())
    }

    #[test]
    fn rewrite_paths_moves_exactly_the_rows_whose_value_still_matches() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let a = s.insert_clip(&new_clip(&p("/old/clips/a.mp4"))).unwrap();
        let b = s.insert_clip(&new_clip(&p("/old/clips/b.mp4"))).unwrap();
        s.insert_clip(&new_clip(&p("/elsewhere/c.mp4"))).unwrap();
        let session = ended_session(
            &s,
            SESSION_MODE_SESSION,
            &p("/old/sessions/session-1"),
            Some(p("/old/sessions/session-1.mp4").as_str()),
        );

        let changed = s
            .rewrite_paths(
                &[
                    rewrite(a, &p("/old/clips/a.mp4"), &p("/new/clips/a.mp4")),
                    // b was edited since the caller read it: the expected value is stale.
                    rewrite(b, &p("/old/clips/gone.mp4"), &p("/new/clips/b.mp4")),
                ],
                &[rewrite(
                    session,
                    &p("/old/sessions/session-1.mp4"),
                    &p("/new/sessions/session-1.mp4"),
                )],
                &[rewrite(
                    session,
                    &p("/old/sessions/session-1"),
                    &p("/new/sessions/session-1"),
                )],
            )
            .unwrap();

        assert_eq!(changed, 3, "a, and the session's two columns; not b");
        assert_eq!(
            clip_paths(&s),
            vec![
                p("/elsewhere/c.mp4"),
                p("/new/clips/a.mp4"),
                p("/old/clips/b.mp4")
            ],
            "b kept its value, because the row no longer held the expected one"
        );
        let moved = s.get_session(session).unwrap().unwrap();
        assert_eq!(moved.scratch_dir, p("/new/sessions/session-1"));
        assert_eq!(
            moved.final_path.as_deref(),
            Some(p("/new/sessions/session-1.mp4").as_str())
        );
    }

    #[test]
    fn rewrite_paths_cannot_overwrite_a_newer_value_or_a_missing_final_path() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let a = s.insert_clip(&new_clip(&p("/old/a.mp4"))).unwrap();
        let open = s
            .start_session(
                None,
                SESSION_MODE_SESSION,
                1_000,
                &p("/old/sessions/session-2"),
                0,
            )
            .unwrap();

        // The first pass moves it; a second pass from the same (now stale) read changes nothing.
        let plan = [rewrite(a, &p("/old/a.mp4"), &p("/new/a.mp4"))];
        assert_eq!(s.rewrite_paths(&plan, &[], &[]).unwrap(), 1);
        assert_eq!(s.rewrite_paths(&plan, &[], &[]).unwrap(), 0);
        // A session that has no final path yet cannot be matched by an expected value.
        assert_eq!(
            s.rewrite_paths(&[], &[rewrite(open, "", &p("/new/s.mp4"))], &[])
                .unwrap(),
            0
        );
        assert_eq!(s.get_session(open).unwrap().unwrap().final_path, None);
        assert_eq!(clip_paths(&s), vec![p("/new/a.mp4")]);
    }

    #[test]
    fn rewrite_paths_refuses_relative_destinations_and_rolls_back_on_a_collision() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let b = s.insert_clip(&new_clip(&p("/old/b.mp4"))).unwrap();
        let a = s.insert_clip(&new_clip(&p("/old/a.mp4"))).unwrap();
        s.insert_clip(&new_clip(&p("/new/a.mp4"))).unwrap();

        assert!(s
            .rewrite_paths(&[rewrite(b, &p("/old/b.mp4"), "relative/b.mp4")], &[], &[])
            .is_err());
        assert!(s
            .rewrite_paths(&[rewrite(b, &p("/old/b.mp4"), &p("/x/../b.mp4"))], &[], &[])
            .is_err());

        assert!(
            s.rewrite_paths(
                &[
                    rewrite(b, &p("/old/b.mp4"), &p("/new/b.mp4")),
                    rewrite(a, &p("/old/a.mp4"), &p("/new/a.mp4")),
                ],
                &[],
                &[],
            )
            .is_err(),
            "/old/a.mp4 would land on the row already at /new/a.mp4"
        );
        assert_eq!(
            clip_paths(&s),
            vec![p("/new/a.mp4"), p("/old/a.mp4"), p("/old/b.mp4")],
            "nothing moved: b was rewritten before the collision and was rolled back with it"
        );
    }

    #[test]
    fn the_v5_step_is_safe_to_run_twice() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        upgrade_to_v5(&s.conn).expect("columns and table already exist");
    }

    #[test]
    fn titles_games_and_tags_round_trip_through_list_clips() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s.insert_clip(&new_clip("/clips/a.mp4")).unwrap();

        assert!(s.set_clip_title(id, "  Pentakill  ").unwrap());
        assert!(s.set_clip_game(id, Some("Valorant")).unwrap());
        assert!(s
            .set_clip_tags(id, &["Ace".into(), " clutch  play ".into(), "ace".into()])
            .unwrap());

        let c = &s.list_clips().unwrap()[0];
        assert_eq!(c.title.as_deref(), Some("Pentakill"));
        assert_eq!(c.game.as_deref(), Some("Valorant"));
        assert_eq!(
            c.tags,
            vec!["ace".to_string(), "clutch play".to_string()],
            "normalized, de-duplicated, sorted"
        );

        // Blank clears.
        s.set_clip_title(id, "   ").unwrap();
        s.set_clip_game(id, None).unwrap();
        s.set_clip_tags(id, &[]).unwrap();
        let c = &s.list_clips().unwrap()[0];
        assert_eq!(
            (c.title.clone(), c.game.clone(), c.tags.clone()),
            (None, None, vec![])
        );
    }

    #[test]
    fn unknown_ids_are_reported_and_invalid_input_is_refused_without_changing_anything() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        assert!(!s.set_clip_title(99, "x").unwrap());
        assert!(!s.set_clip_game(99, Some("x")).unwrap());
        assert!(!s.set_clip_tags(99, &["x".into()]).unwrap());

        let id = s.insert_clip(&new_clip("/clips/a.mp4")).unwrap();
        s.set_clip_tags(id, &["keep".into()]).unwrap();
        assert!(s.set_clip_tags(id, &["ok".into(), "   ".into()]).is_err());
        assert!(s
            .set_clip_tags(id, &["x".repeat(MAX_TAG_CHARS + 1)])
            .is_err());
        let many: Vec<String> = (0..=MAX_TAGS_PER_CLIP).map(|i| format!("t{i}")).collect();
        assert!(s.set_clip_tags(id, &many).is_err());
        assert_eq!(
            s.list_clips().unwrap()[0].tags,
            vec!["keep".to_string()],
            "a refused set changes nothing"
        );

        assert!(s
            .set_clip_title(id, &"x".repeat(MAX_TITLE_CHARS + 1))
            .is_err());
        assert!(s.set_clip_title(id, "bad\ntitle").is_err());
    }

    #[test]
    fn deleting_a_clip_takes_its_tags_with_it() {
        let s = Store::open_in_memory().unwrap();
        s.migrate().unwrap();
        let id = s.insert_clip(&new_clip("/clips/a.mp4")).unwrap();
        s.set_clip_tags(id, &["a".into()]).unwrap();
        s.delete_clip_returning_path(id).unwrap();
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM clip_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
