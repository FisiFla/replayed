//! The directories an unattended pass may delete from.
//!
//! The executors ([`crate::cleanup::execute_cleanup`], [`crate::retention::execute_retention`]) run
//! at startup and every few minutes with nobody watching, and the paths they unlink come out of a
//! SQLite file the user can edit and any bug can fill. Until now the only check was syntactic
//! (absolute, no `..`), so a row naming `C:\Users\me\Documents\thesis.docx`, or a replay-buffer
//! session row naming the user's shared `[buffer].scratch_dir`, cost the user that file or that
//! whole tree. The desktop's interactive delete already holds the line that "a row pointing outside
//! the library must cost the user a row, never a file"; this is the same line for the automatic
//! path, with the roots handed in by the caller that knows them.
//!
//! A [`LibraryRoots`] is two directories, and each class of artefact has exactly one place it may
//! be removed from:
//!
//! | what a row names                  | must be                                                    |
//! |-----------------------------------|------------------------------------------------------------|
//! | a clip's file                     | inside `clips_dir`                                         |
//! | a session's concatenated file     | inside `sessions_dir`                                      |
//! | a session's scratch directory     | a *direct child* of `sessions_dir` named `session-<digits>[-<digits>]` (what the recorder creates), and never `clips_dir` or one of its ancestors |
//!
//! "Inside" is decided on **canonical** paths: the parent directory is resolved through every
//! symlink and junction before it is compared with the canonical root, so a link planted in the
//! library cannot lead a removal out of it. (The final component is deliberately not resolved: a
//! symlink there is unlinked as a link, which cannot reach its target.)
//!
//! Anything else is refused: the row is already gone, the path is left exactly as it is, and the
//! executor reports it in `refused`. That fails closed. The price is that rows written under a
//! previous `clips_dir` or `sessions_dir` are no longer cleaned up automatically after the setting
//! changes; [`crate::Store::rewrite_paths`] moves such rows to the new location, one expected value at a
//! time.

use std::io;
use std::path::{Path, PathBuf};

/// Why a path was not handed to the remover.
#[derive(Debug)]
pub(crate) enum Gate {
    /// Its parent directory does not exist, so the path cannot either.
    Missing,
    /// It is not somewhere this pass may delete from. The path stays untouched.
    Refused(&'static str),
    /// The check itself failed (permissions, I/O). Nothing was touched; the caller may retry later.
    Failed(io::Error),
}

const CLIPS_MISSING: &str = "the clips directory does not exist";
const SESSIONS_MISSING: &str = "the sessions directory does not exist";

/// What a path is, which decides the root that has to vouch for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// A clip's file: inside the clips directory.
    ClipFile,
    /// A session's concatenated file: inside the sessions directory.
    SessionFile,
    /// A session's scratch directory, removed as a tree.
    SessionDir,
}

/// The clips directory and the sessions directory: the only places an unattended pass deletes from.
///
/// Built by the process that owns the configuration (the recorder's `cleanup_pass`, the CLI) from
/// the same two paths it records into. They are canonicalised when a path is checked, not here, so
/// a directory that does not exist *yet* is not an error to construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRoots {
    clips_dir: PathBuf,
    sessions_dir: PathBuf,
}

impl Target {
    /// The text stored in `pending_deletes.target`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Target::ClipFile => "clip_file",
            Target::SessionFile => "session_file",
            Target::SessionDir => "session_dir",
        }
    }

    /// The inverse of [`Target::as_str`].
    pub(crate) fn parse(text: &str) -> Option<Self> {
        [Target::ClipFile, Target::SessionFile, Target::SessionDir]
            .into_iter()
            .find(|target| target.as_str() == text)
    }
}

impl LibraryRoots {
    /// The two roots. Neither is created or touched.
    pub fn new(clips_dir: impl Into<PathBuf>, sessions_dir: impl Into<PathBuf>) -> Self {
        Self {
            clips_dir: clips_dir.into(),
            sessions_dir: sessions_dir.into(),
        }
    }

    /// The clips directory.
    pub fn clips_dir(&self) -> &Path {
        &self.clips_dir
    }

    /// The sessions directory.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Whether `path` may be removed as `target`; the one decision every removal passes through.
    pub(crate) fn admit(&self, target: Target, path: &Path) -> Result<(), Gate> {
        if !crate::cleanup::is_absolute_and_clean(path) {
            return Err(Gate::Refused(
                "it is relative or contains `..`, so it names no fixed place",
            ));
        }
        let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
            return Err(Gate::Refused(
                "no final path component: it names a filesystem root, '.' or '..'",
            ));
        };
        let parent = match std::fs::canonicalize(parent) {
            Ok(parent) => parent,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(Gate::Missing),
            Err(err) => return Err(Gate::Failed(err)),
        };
        match target {
            Target::ClipFile => within(
                &self.clips_dir,
                &parent,
                CLIPS_MISSING,
                "it is outside the clips directory",
            ),
            Target::SessionFile => within(
                &self.sessions_dir,
                &parent,
                SESSIONS_MISSING,
                "it is outside the sessions directory",
            ),
            Target::SessionDir => {
                if !is_session_dir_name(&name.to_string_lossy()) {
                    return Err(Gate::Refused(
                        "its name is not `session-<digits>[-<digits>]`, so this application did \
                         not create it",
                    ));
                }
                let root = canonical_root(&self.sessions_dir, SESSIONS_MISSING)?;
                if parent != root {
                    return Err(Gate::Refused(
                        "it is not a direct child of the sessions directory",
                    ));
                }
                // Defence in depth: the clips directory must never sit inside what is removed.
                let dir = parent.join(name);
                if let Ok(clips) = std::fs::canonicalize(&self.clips_dir) {
                    if clips.starts_with(&dir) {
                        return Err(Gate::Refused("it is, or contains, the clips directory"));
                    }
                }
                Ok(())
            }
        }
    }
}

/// `root`, canonicalised; a root that is missing makes everything refused, because nothing can be
/// proven to be inside a directory that is not there.
fn canonical_root(root: &Path, missing: &'static str) -> Result<PathBuf, Gate> {
    match std::fs::canonicalize(root) {
        Ok(root) => Ok(root),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(Gate::Refused(missing)),
        Err(err) => Err(Gate::Failed(err)),
    }
}

/// `parent` (already canonical) is `root` or below it.
fn within(
    root: &Path,
    parent: &Path,
    missing: &'static str,
    outside: &'static str,
) -> Result<(), Gate> {
    if parent.starts_with(canonical_root(root, missing)?) {
        Ok(())
    } else {
        Err(Gate::Refused(outside))
    }
}

/// `session-<digits>` or `session-<digits>-<digits>`: the names `create_session_dir` produces, and
/// the shape the desktop's own `is_session_name` accepts.
fn is_session_dir_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("session-") else {
        return false;
    };
    let mut parts = rest.split('-');
    let digits = |part: Option<&str>| {
        part.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    digits(parts.next()) && parts.next().is_none_or(|p| digits(Some(p))) && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> (tempfile::TempDir, LibraryRoots) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clips = dir.path().join("clips");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&clips).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        (dir, LibraryRoots::new(clips, sessions))
    }

    fn refused(result: Result<(), Gate>) -> bool {
        matches!(result, Err(Gate::Refused(_)))
    }

    #[test]
    fn a_file_in_its_own_root_is_admitted_and_one_elsewhere_is_refused() {
        let (dir, roots) = layout();
        let inside = roots.clips_dir().join("kill.mp4");
        let nested = roots.clips_dir().join("2026").join("kill.mp4");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        assert!(roots.admit(Target::ClipFile, &inside).is_ok());
        assert!(roots.admit(Target::ClipFile, &nested).is_ok());

        // Outside the library entirely, and inside the *other* root: both are refused.
        let thesis = dir.path().join("thesis.docx");
        assert!(refused(roots.admit(Target::ClipFile, &thesis)));
        let session_file = roots.sessions_dir().join("session-1.mp4");
        assert!(refused(roots.admit(Target::ClipFile, &session_file)));
        assert!(roots.admit(Target::SessionFile, &session_file).is_ok());
        assert!(refused(roots.admit(Target::SessionFile, &inside)));
    }

    #[test]
    fn a_sibling_whose_name_merely_starts_like_the_root_is_outside_it() {
        let (dir, roots) = layout();
        let lookalike = dir.path().join("clips-backup");
        std::fs::create_dir_all(&lookalike).unwrap();
        assert!(
            refused(roots.admit(Target::ClipFile, &lookalike.join("a.mp4"))),
            "containment is by path component, not by string prefix"
        );
    }

    #[test]
    fn a_missing_parent_is_missing_not_refused() {
        let (_dir, roots) = layout();
        let gone = roots.clips_dir().join("never-created").join("a.mp4");
        assert!(matches!(
            roots.admit(Target::ClipFile, &gone),
            Err(Gate::Missing)
        ));
    }

    #[test]
    fn a_missing_root_admits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let roots = LibraryRoots::new(dir.path().join("no-clips"), dir.path().join("no-sessions"));
        assert!(refused(roots.admit(Target::ClipFile, dir.path())));
        let elsewhere = dir.path().join("x.mp4");
        assert!(refused(roots.admit(Target::ClipFile, &elsewhere)));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_cannot_lead_out_of_the_library() {
        let (dir, roots) = layout();
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, roots.clips_dir().join("link")).unwrap();
        assert!(
            refused(roots.admit(
                Target::ClipFile,
                &roots.clips_dir().join("link").join("a.mp4")
            )),
            "the parent resolves outside the clips directory"
        );
    }

    #[test]
    fn a_scratch_directory_must_be_a_session_shaped_child_of_the_sessions_directory() {
        let (dir, roots) = layout();
        let child = roots.sessions_dir().join("session-1700000000");
        let retry = roots.sessions_dir().join("session-1700000000-2");
        assert!(roots.admit(Target::SessionDir, &child).is_ok());
        assert!(roots.admit(Target::SessionDir, &retry).is_ok());

        // The sessions directory itself, an unrelated name, a nested directory, and a directory
        // outside the library all fail, even though each is "an absolute, clean directory".
        assert!(refused(
            roots.admit(Target::SessionDir, roots.sessions_dir())
        ));
        for name in [
            "scratch",
            "session-",
            "session-x",
            "session-1-2-3",
            "session-1-",
        ] {
            assert!(
                refused(roots.admit(Target::SessionDir, &roots.sessions_dir().join(name))),
                "{name}"
            );
        }
        let nested = child.join("session-5");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(refused(roots.admit(Target::SessionDir, &nested)));
        let shared = dir.path().join("session-1700000000");
        assert!(refused(roots.admit(Target::SessionDir, &shared)));
    }

    #[test]
    fn a_scratch_directory_that_holds_the_clips_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session = sessions.join("session-1");
        let clips = session.join("clips");
        std::fs::create_dir_all(&clips).unwrap();
        let roots = LibraryRoots::new(&clips, &sessions);
        assert!(refused(roots.admit(Target::SessionDir, &session)));
    }
}
