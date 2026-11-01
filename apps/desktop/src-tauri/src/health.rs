//! The health page's data, and the GSI installer behind the settings button.
//!
//! Everything here is a plain function over explicit paths and values, so tests drive it
//! against temporary directories and hand-built statuses with no window or Tauri runtime.
//!
//! # The GSI installer
//!
//! The application does not write into game installation directories on its own. The installer
//! runs only when the user presses the button, writes exactly one file
//! (`gamestate_integration_replayed.cfg`), and never takes the destination from the webview:
//! the command receives a game (`cs2` or `dota2`), the install directory is found here from
//! Steam's own library list, and the final path goes through
//! [`crate::paths::ensure_new_file_within`] against the Steam root before a byte is written, so
//! a symlink or `..` cannot walk the write out of the game's folder.
//!
//! # The file carries the *live* binding
//!
//! The listener reads its token once, when it starts, and the port it binds may be an
//! ephemeral one the operating system picked. So the installer never re-reads the token file:
//! it is handed the binding the running listener serves ([`GsiBinding`]) and writes that
//! binding's own text ([`GsiBinding::cfg`]). A file naming another token or another port is a
//! game whose every POST is answered `403`, and the log's advice ("install the generated cfg
//! and restart the game") cannot fix a file the application wrote itself.
//!
//! # The root the write is contained by
//!
//! The install folder is found through `libraryfolders.vdf`, a file in the user's own Steam
//! directory. Those libraries are **trusted as the user's configuration**, not checked against a
//! list of operating-system roots: another unprivileged account cannot edit that file, anything
//! that can edit it already runs with the user's rights, and refusing the libraries it names would
//! break a game on a second drive for no gain. What is enforced is the shape and the boundary:
//! the folder has to be exactly a library's `steamapps/common/<game folder>` (see
//! [`trusted_install`]), resolved through symlinks, and the write is contained by that *library*,
//! so a symlink inside the game folder that points out of the library is refused. The roots the
//! libraries are read from ([`steam_roots`]) are Steam's registered install path and the default
//! locations; they only say where to *look*.
//!
//! # The file is written atomically
//!
//! The write goes to a uniquely-named temporary file in the same directory, is flushed to
//! disk, and is then renamed over the destination. The game therefore reads either the old
//! file or the new one, never a half-written mixture, and a crash (or a full disk) cannot
//! leave a truncated cfg behind.

use crate::commands::{CommandError, ErrorCode};
use replayed_events::gsi;
use replayed_recorder::event_driver::{SourceReport, SourceState, GSI_GAME_CS2, GSI_GAME_DOTA2};
use replayed_recorder::{GsiBinding, RecorderStatus};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file name both games read.
///
/// The recorder's own name for it, so the two halves cannot drift apart.
pub const GSI_CFG_NAME: &str = replayed_recorder::event_driver::GSI_CFG_NAME;

// ---------------------------------------------------------------------------------------
// Game directories
// ---------------------------------------------------------------------------------------

/// The two games with a Game State Integration feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GsiGame {
    Cs2,
    Dota2,
}

impl GsiGame {
    pub const ALL: [GsiGame; 2] = [GsiGame::Cs2, GsiGame::Dota2];

    pub fn label(self) -> &'static str {
        match self {
            GsiGame::Cs2 => "Counter-Strike 2",
            GsiGame::Dota2 => "Dota 2",
        }
    }

    /// The folder under `steamapps/common`.
    fn folder(self) -> &'static str {
        match self {
            GsiGame::Cs2 => "Counter-Strike Global Offensive",
            GsiGame::Dota2 => "dota 2 beta",
        }
    }

    /// Where the cfg goes, relative to the install folder.
    fn cfg_dir(self) -> &'static [&'static str] {
        match self {
            GsiGame::Cs2 => &["game", "csgo", "cfg"],
            GsiGame::Dota2 => &["game", "dota", "cfg", "gamestate_integration"],
        }
    }

    /// The recorder's id for this game: what a live binding is named.
    ///
    /// The two halves have to agree on the name, or the shell looks for a binding that is
    /// not there and installs nothing.
    pub fn game_id(self) -> &'static str {
        match self {
            GsiGame::Cs2 => GSI_GAME_CS2,
            GsiGame::Dota2 => GSI_GAME_DOTA2,
        }
    }

    /// Valve's app id for this game: what the listener pins the game's payloads to.
    pub fn app_id(self) -> u32 {
        match self {
            GsiGame::Cs2 => gsi::CS2_APP_ID,
            GsiGame::Dota2 => gsi::DOTA2_APP_ID,
        }
    }
}

/// Steam's own roots: the install path Steam registered, then the default locations.
///
/// The registry is first because it is the only answer for a Steam installed anywhere but
/// `Program Files` (`D:\Steam` is common), which the fixed paths below can never find. Whatever
/// does not exist is filtered later.
pub fn steam_roots() -> Vec<PathBuf> {
    steam_roots_from(registered_steam_path())
}

/// [`steam_roots`], with the registry's answer supplied so a test can drive it.
fn steam_roots_from(registered: Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.extend(registered);
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base) = std::env::var_os(var) {
            roots.push(PathBuf::from(base).join("Steam"));
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        roots.push(home.join("Library/Application Support/Steam"));
        roots.push(home.join(".steam/steam"));
        roots.push(home.join(".local/share/Steam"));
    }
    roots
}

/// The Steam install path Steam keeps under the current user's registry key.
///
/// `HKCU\Software\Valve\Steam`, value `SteamPath`: a read of the user's own hive through
/// `reg.exe`, the same tool the autostart sync already uses, so no new API and no new dependency.
/// Nothing is written and nothing about another process is touched. A missing key (Steam not
/// installed, or installed for another user) and a `reg.exe` that cannot run are both "no answer":
/// the default locations still apply, so this can only add a root, never remove one.
#[cfg(windows)]
fn registered_steam_path() -> Option<PathBuf> {
    let output = replayed_media::sidecar_command("reg")
        .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_steam_path(&String::from_utf8_lossy(&output.stdout))
}

/// There is no registry off Windows; the default locations are the whole answer.
#[cfg(not(windows))]
fn registered_steam_path() -> Option<PathBuf> {
    None
}

/// The `SteamPath` data in `reg query` output.
///
/// `reg query` prints `    SteamPath    REG_SZ    c:/program files (x86)/steam`: the value's
/// name, its type, then the data, which may itself contain spaces and is kept whole. Steam writes
/// it with forward slashes; Windows accepts those, so the path is used as written.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_steam_path(output: &str) -> Option<PathBuf> {
    output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("SteamPath")?;
        // The name is a whole word: `SteamPathX` is some other value.
        let rest = rest.strip_prefix(char::is_whitespace)?.trim_start();
        let data = rest
            .strip_prefix("REG_SZ")
            .or_else(|| rest.strip_prefix("REG_EXPAND_SZ"))?
            .trim();
        (!data.is_empty()).then(|| PathBuf::from(data))
    })
}

/// Every Steam library: the roots themselves plus the `"path"` entries of each root's
/// `steamapps/libraryfolders.vdf`.
pub fn steam_libraries(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut libs: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        if p.is_dir() && !libs.contains(&p) {
            libs.push(p);
        }
    };
    for root in roots {
        add(root.clone());
        if let Ok(text) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            for path in parse_library_paths(&text) {
                add(PathBuf::from(path));
            }
        }
    }
    libs
}

/// The `"path"  "..."` values of a `libraryfolders.vdf`, with VDF's `\\` unescaped.
pub fn parse_library_paths(vdf: &str) -> Vec<String> {
    vdf.lines()
        .filter_map(|line| {
            let mut quoted = line.split('"').skip(1).step_by(2);
            (quoted.next()? == "path").then(|| quoted.next().map(|v| v.replace("\\\\", "\\")))?
        })
        .collect()
}

/// The game's install folder in the first library that has it.
pub fn find_install(game: GsiGame, libraries: &[PathBuf]) -> Option<PathBuf> {
    libraries
        .iter()
        .map(|lib| lib.join("steamapps").join("common").join(game.folder()))
        .find(|dir| dir.is_dir())
}

/// What the settings panel shows for one game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GsiTarget {
    pub game: GsiGame,
    pub label: String,
    /// The install folder, or `None` when no Steam library has the game.
    pub install_dir: Option<String>,
    /// Whether the cfg is already in place.
    pub installed: bool,
}

pub fn gsi_targets(libraries: &[PathBuf]) -> Vec<GsiTarget> {
    GsiGame::ALL
        .iter()
        .map(|&game| {
            let install = find_install(game, libraries);
            let installed = install
                .as_ref()
                .is_some_and(|dir| cfg_path(game, dir).is_file());
            GsiTarget {
                game,
                label: game.label().to_string(),
                install_dir: install.map(|p| p.to_string_lossy().into_owned()),
                installed,
            }
        })
        .collect()
}

fn cfg_path(game: GsiGame, install: &Path) -> PathBuf {
    let mut dir = install.to_path_buf();
    dir.extend(game.cfg_dir());
    dir.join(GSI_CFG_NAME)
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GsiInstallOutcome {
    pub game: GsiGame,
    pub path: String,
    /// An earlier copy was overwritten.
    pub replaced: bool,
}

/// The GSI listener, as the installer sees it.
///
/// The installer writes the file the game posts to, so it can only do that while the listener
/// is up: a file naming a port nobody listens on, or a token the listener does not accept, is
/// a game whose every POST is answered `403`.
pub enum GsiListener<'a> {
    /// The listener is up and serves this game with this binding: the live port and token.
    Running(&'a GsiBinding),
    /// The listener is switched off in the configuration (`events.gsi_port = 0`).
    SwitchedOff,
    /// Nothing is listening: no recording is running, or the listener failed to start.
    NotRunning,
}

/// Write the cfg for `game` into `install` (the game's install folder).
///
/// The file is written from the *live* binding the listener serves: the port it bound and the
/// token it accepts. Nothing is re-read from the token file, because the listener read that
/// file once, when it started, and a file replaced since would leave the game posting a token
/// the listener refuses.
///
/// `install` has to be exactly a Steam library's `steamapps/common/<game folder>`: the libraries
/// are the user's own configuration (see [`trusted_install`]), and the write is contained by the
/// library, with symlinks resolved, so it cannot leave it.
///
/// With no listener running there is nothing for the game to post to, so the install is
/// refused with the reason rather than writing a file that cannot work.
pub fn install_gsi_cfg(
    game: GsiGame,
    install: &Path,
    listener: GsiListener<'_>,
    libraries: &[PathBuf],
) -> Result<GsiInstallOutcome, CommandError> {
    let binding = match listener {
        GsiListener::Running(binding) => binding,
        GsiListener::SwitchedOff => {
            return Err(CommandError::new(
                ErrorCode::InvalidInput,
                "the CS2 / Dota 2 listener is switched off (events.gsi_port = 0), so there is \
                 nothing for the game to send to; set a port in config.toml first",
            ))
        }
        GsiListener::NotRunning => {
            return Err(CommandError::new(
                ErrorCode::InvalidInput,
                "the CS2 / Dota 2 listener is not running, so there is nothing for the game \
                 to post to; start a recording first (or check events.gsi_port in config.toml)",
            ))
        }
    };
    let outcome = install_with_binding(game, install, binding, libraries)?;
    tracing::info!("installed the {} GSI cfg at {}", game.label(), outcome.path);
    Ok(outcome)
}

/// The install itself: the folder is checked against the Steam libraries, and the file is
/// written atomically.
///
/// Shared with the refresh ([`refresh_installed_gsi`]): an installed file that has to be
/// rewritten goes through the same checks as the first install, because the folder it lives
/// in may have changed since.
fn install_with_binding(
    game: GsiGame,
    install: &Path,
    binding: &GsiBinding,
    libraries: &[PathBuf],
) -> Result<GsiInstallOutcome, CommandError> {
    let (root, install) = trusted_install(game, install, libraries)?;
    // Only the cfg directory of a game that is really installed: a half-deleted CS2 with no
    // `game/csgo` is not somewhere to start creating folders.
    let target = cfg_path(game, &install);
    let dest = crate::paths::ensure_new_file_within(&root, &target).map_err(|why| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("{}: {why}", target.display()),
        )
    })?;
    let cfg_dir = dest.parent().unwrap_or(&install);
    if game == GsiGame::Cs2 && !cfg_dir.is_dir() {
        return Err(CommandError::new(
            ErrorCode::InvalidInput,
            format!(
                "{} does not exist; is Counter-Strike 2 fully installed?",
                cfg_dir.display()
            ),
        ));
    }
    std::fs::create_dir_all(cfg_dir).map_err(|e| CommandError::io("creating", cfg_dir, e))?;
    let replaced = dest.is_file();
    write_cfg_atomically(&dest, &binding.cfg())
        .map_err(|e| CommandError::io("writing", &dest, e))?;
    Ok(GsiInstallOutcome {
        game,
        path: dest.to_string_lossy().into_owned(),
        replaced,
    })
}

/// The install folder, checked against the Steam libraries.
///
/// The folder is found through `libraryfolders.vdf`, which is the user's own Steam
/// configuration: it lives in the user's Steam directory, so no *other* unprivileged account
/// can edit it, and the application writes with that user's own permissions either way.
/// Refusing the libraries it names would break a game on a second drive for no security
/// gain, so they are trusted as configuration.
///
/// What is checked is the SHAPE and the boundary: the folder has to be exactly a library's
/// `steamapps/common/<game folder>`, resolved through symlinks like everything else here,
/// and the write is contained by the *library* rather than by the folder. A symlink planted
/// inside the game folder that points out of the library is therefore refused, while a
/// secondary library is not.
///
/// Returns the library the write is contained by, and the resolved folder.
fn trusted_install(
    game: GsiGame,
    install: &Path,
    libraries: &[PathBuf],
) -> Result<(PathBuf, PathBuf), CommandError> {
    let resolved = install.canonicalize().map_err(|err| {
        CommandError::new(
            ErrorCode::InvalidInput,
            format!("{} could not be resolved: {err}", install.display()),
        )
    })?;
    for library in libraries {
        let Ok(library) = library.canonicalize() else {
            continue;
        };
        let expected = library.join("steamapps").join("common").join(game.folder());
        if expected
            .canonicalize()
            .is_ok_and(|expected| expected == resolved)
        {
            return Ok((library, resolved));
        }
    }
    Err(CommandError::new(
        ErrorCode::InvalidInput,
        format!(
            "{} is not a Steam library's steamapps/common/{}, so nothing was written",
            install.display(),
            game.folder()
        ),
    ))
}

/// Write `text` to `dest` atomically.
///
/// A uniquely-named temp file in the same directory, written, flushed to disk, then renamed
/// over the destination: the file the game reads is either the old one or the new one, never
/// a half-written mixture, and a crash (or a full disk) cannot leave one behind.
///
/// The rename *replaces* the destination rather than writing through it, so the file is
/// never truncated in place: a reader either sees the old file or the new one.
fn write_cfg_atomically(dest: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    write_cfg_atomically_with(dest, text, |file, bytes| file.write_all(bytes))
}

/// [`write_cfg_atomically`], with the write itself supplied.
///
/// The write is a parameter so a test can fail it *after* the temporary file exists: that is
/// the failure the atomic write exists for, and the one a write in place cannot survive.
fn write_cfg_atomically_with(
    dest: &Path,
    text: &str,
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    // The name carries the pid and a counter so two installs cannot share it.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{GSI_CFG_NAME}.{}.{seq}.tmp", std::process::id()));
    let attempt = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        write(&mut file, text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, dest)
    };
    if let Err(err) = attempt() {
        // The temp file goes, whatever the failure was: a leftover would be a half-written
        // cfg sitting in the game's directory under a name nothing reads.
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// A `.cfg` the shell installed into a game's own directory.
///
/// The record is what keeps the installed file *current*: the port the listener binds can
/// change between runs (it is configurable, and it may be an ephemeral one), and a file that
/// names an old port is a game posting into the void.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledGsi {
    /// The game it was installed for.
    pub game: GsiGame,
    /// The install folder it went into.
    pub install: PathBuf,
    /// The file that was written.
    pub path: PathBuf,
    /// The binding it was written from: the port, path and token in force then.
    pub binding: GsiBinding,
}

/// Record an installation, replacing the record of the same game.
///
/// One record per game: a second install of the same game replaces the first, so the refresh
/// does not write the same file twice.
pub fn remember_install(
    installed: &mut Vec<InstalledGsi>,
    game: GsiGame,
    install: &Path,
    path: &Path,
    binding: &GsiBinding,
) {
    installed.retain(|old| old.game != game);
    installed.push(InstalledGsi {
        game,
        install: install.to_path_buf(),
        path: path.to_path_buf(),
        binding: binding.clone(),
    });
}

/// Whether the file an installation points at lacks a data block this build subscribes to.
///
/// The record says which binding the file was written from, not what is in it now: a file put
/// back by hand from an older guide, or by an older build sharing the folder, has the right port
/// and token and is still missing `player_id`, so the game posts rounds with no identity in them
/// and the spectator rules cannot tell the user from the player they are watching. Only
/// Counter-Strike's file has that check ([`gsi::cs2_cfg_is_current`]); a file that cannot be read
/// is not called stale, because the rewrite that would follow reports that failure properly.
fn on_disk_cfg_is_stale(entry: &InstalledGsi) -> bool {
    entry.game == GsiGame::Cs2
        && std::fs::read_to_string(&entry.path).is_ok_and(|text| !gsi::cs2_cfg_is_current(&text))
}

/// Rewrite the installations whose live binding has moved, or whose file no longer carries the
/// data blocks this build subscribes to.
///
/// The port the listener binds can change between runs, and the token with it, so an
/// installed file that names the old ones is exactly the stale file this exists to prevent.
/// Every installation whose game the listener serves and whose binding differs from the one
/// it was written from is rewritten, and so is one whose file is out of date
/// ([`on_disk_cfg_is_stale`]) even though the binding matches.
///
/// An installation whose rewrite fails is dropped rather than retried: the game folder is
/// gone, or its permissions changed, and every later start would say the same thing.
pub fn refresh_installed_gsi(
    installed: &mut Vec<InstalledGsi>,
    bindings: &[GsiBinding],
    libraries: &[PathBuf],
) {
    installed.retain_mut(|entry| {
        let Some(binding) = bindings.iter().find(|b| b.game == entry.game.game_id()) else {
            // The listener does not serve this game right now: there is nothing to refresh
            // it to, and the installation is kept for when it does.
            return true;
        };
        if *binding == entry.binding && !on_disk_cfg_is_stale(entry) {
            return true;
        }
        match install_with_binding(entry.game, &entry.install, binding, libraries) {
            Ok(outcome) => {
                entry.path = PathBuf::from(outcome.path);
                entry.binding = binding.clone();
                tracing::info!(
                    "the {} GSI cfg at {} was rewritten for the listener's current port",
                    entry.game.label(),
                    entry.path.display()
                );
                true
            }
            Err(err) => {
                tracing::warn!(
                    "the {} GSI cfg at {} could not be rewritten and will not be tried again: {}",
                    entry.game.label(),
                    entry.path.display(),
                    err.message
                );
                false
            }
        }
    });
}

// ---------------------------------------------------------------------------------------
// The health page
// ---------------------------------------------------------------------------------------

/// How the recorder is, in one word the page can colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderState {
    Recording,
    /// Armed for a game, not recording yet.
    Waiting,
    Idle,
    /// Stopped by a failure; `error` says which.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskHealth {
    pub path: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSourceDto {
    pub source: String,
    /// `running`, `disabled` or `failed`.
    pub state: String,
    pub detail: String,
}

/// Mirrored in `src/lib/types.ts`; a test pins the key set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthDto {
    pub recorder_state: RecorderState,
    pub recorder_error: Option<String>,
    /// The encoder the latest recording ran; `None` before any recording.
    pub encoder: Option<String>,
    pub ffmpeg_available: bool,
    pub disk: Option<DiskHealth>,
    /// Empty until a recording starts: the sources live exactly as long as it does.
    pub event_sources: Vec<EventSourceDto>,
    /// Newest last.
    pub recent_errors: Vec<String>,
    pub log_path: String,
}

pub fn recorder_state(status: &RecorderStatus) -> RecorderState {
    if status.running {
        RecorderState::Recording
    } else if status.error.is_some() {
        RecorderState::Failed
    } else if status.watching_games {
        RecorderState::Waiting
    } else {
        RecorderState::Idle
    }
}

pub fn event_source_dtos(reports: &[SourceReport]) -> Vec<EventSourceDto> {
    reports
        .iter()
        .map(|r| EventSourceDto {
            source: r.source.to_string(),
            state: match r.state {
                SourceState::Running => "running",
                SourceState::Disabled => "disabled",
                SourceState::Failed => "failed",
            }
            .to_string(),
            detail: r.detail.clone(),
        })
        .collect()
}

/// Free and total bytes of the disk holding `dir` (its nearest existing ancestor, so a clips
/// directory that does not exist yet still answers).
pub fn disk_health(dir: &Path) -> Option<DiskHealth> {
    let probe = dir.ancestors().find(|p| p.exists())?;
    Some(DiskHealth {
        path: dir.to_string_lossy().into_owned(),
        free_bytes: fs4::available_space(probe).ok()?,
        total_bytes: fs4::total_space(probe).ok()?,
    })
}

/// The last `max` WARN/ERROR lines of a log, oldest first.
///
/// A log line is `<timestamp>  LEVEL target: message`; the level is matched as a whole word
/// so a message that merely contains "error" is not counted.
pub fn recent_problems(log: &str, max: usize) -> Vec<String> {
    let mut found: Vec<String> = log
        .lines()
        .filter(|l| {
            l.split_whitespace()
                .take(3)
                .any(|w| w == "ERROR" || w == "WARN")
        })
        .map(|l| l.trim().chars().take(400).collect())
        .collect();
    let drop = found.len().saturating_sub(max);
    found.drain(..drop);
    found
}

/// Read the tail of the log file (at most 256 KiB) and extract [`recent_problems`].
pub fn read_recent_problems(log_path: &Path, max: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 256 * 1024;
    let Ok(mut file) = std::fs::File::open(log_path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(TAIL)))
        .is_err()
    {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    recent_problems(&String::from_utf8_lossy(&bytes), max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use replayed_recorder::GsiGameInfo;

    /// A fixed token, not a credential: the digits of pi in hex, the same in every test.
    /// The real one is generated from the OS entropy source at run time. `gitleaks:allow`
    /// because a hex string assigned to a name containing "token" reads as a leak.
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"; // gitleaks:allow

    fn fake_steam(root: &Path, game: GsiGame) -> PathBuf {
        let install = root.join("steamapps/common").join(game.folder());
        std::fs::create_dir_all(&install).unwrap();
        install
    }

    /// The binding a running listener serves one game with.
    fn binding(game: GsiGame, port: u16, token: &str) -> GsiBinding {
        GsiBinding {
            game: game.game_id().to_string(),
            path: gsi::game_path(game.game_id()),
            port,
            app_id: game.app_id(),
            token: token.to_string(),
        }
    }

    /// The names of the files in `dir`.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_two_games_map_to_the_recorder_ids_and_app_ids() {
        // The shell and the recorder have to name the same games, or the shell looks for a
        // binding that is not there and installs nothing.
        assert_eq!(GsiGame::Cs2.game_id(), "cs2");
        assert_eq!(GsiGame::Dota2.game_id(), "dota2");
        assert_eq!(GsiGame::Cs2.app_id(), 730);
        assert_eq!(GsiGame::Dota2.app_id(), 570);
        assert_eq!(gsi::game_path(GsiGame::Cs2.game_id()), "/gsi/cs2");
        assert_eq!(gsi::game_path(GsiGame::Dota2.game_id()), "/gsi/dota2");
    }

    #[test]
    fn library_paths_are_read_from_the_vdf_and_unescaped() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Games\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(
            parse_library_paths(vdf),
            vec!["C:\\Games\\Steam", "D:\\SteamLibrary"]
        );
    }

    #[test]
    fn a_game_is_found_in_a_secondary_library_listed_by_the_vdf() {
        let steam = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(steam.path().join("steamapps")).unwrap();
        std::fs::write(
            steam.path().join("steamapps/libraryfolders.vdf"),
            format!(
                "\"x\"\n{{\n\"0\"\n{{\n\"path\" \"{}\"\n}}\n}}\n",
                second.path().display()
            ),
        )
        .unwrap();
        let install = fake_steam(second.path(), GsiGame::Dota2);
        let libs = steam_libraries(&[steam.path().to_path_buf()]);
        assert_eq!(find_install(GsiGame::Dota2, &libs), Some(install));
        assert_eq!(find_install(GsiGame::Cs2, &libs), None);
        let targets = gsi_targets(&libs);
        assert!(targets[0].install_dir.is_none() && !targets[0].installed);
        assert!(targets[1].install_dir.is_some() && !targets[1].installed);
    }

    #[test]
    fn the_install_writes_the_live_binding_not_the_configured_port() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();

        // The port the listener bound, which is not the one the configuration asked for.
        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let out = install_gsi_cfg(
            GsiGame::Cs2,
            &install,
            GsiListener::Running(&live),
            &[steam.path().to_path_buf()],
        )
        .unwrap();

        assert!(!out.replaced);
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(
            text.contains("\"uri\"       \"http://127.0.0.1:45671/gsi/cs2\""),
            "the file names the live port and the game's own path:\n{text}"
        );
        assert_eq!(
            text.matches(TOKEN).count(),
            1,
            "the file carries the listener's token, once:\n{text}"
        );
        assert!(gsi_targets(&[steam.path().to_path_buf()])[0].installed);
    }

    #[test]
    fn a_second_install_of_the_same_binding_writes_the_same_file() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();
        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let roots = [steam.path().to_path_buf()];

        let first =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &roots).unwrap();
        let again =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &roots).unwrap();

        assert!(again.replaced, "a second install overwrites and says so");
        assert_eq!(
            std::fs::read_to_string(&again.path).unwrap(),
            std::fs::read_to_string(&first.path).unwrap()
        );
    }

    #[test]
    fn an_install_without_a_running_listener_is_refused_and_nothing_is_written() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();

        let err = install_gsi_cfg(
            GsiGame::Cs2,
            &install,
            GsiListener::NotRunning,
            &[steam.path().to_path_buf()],
        )
        .unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(
            err.message.contains("not running"),
            "the message must say why there is nothing to install for: {}",
            err.message
        );
        assert!(
            !install.join("game/csgo/cfg").join(GSI_CFG_NAME).exists(),
            "a file that cannot work was not written"
        );
    }

    #[test]
    fn dota_gets_its_gamestate_integration_folder_created() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Dota2);
        let live = binding(GsiGame::Dota2, 45671, TOKEN);
        let out = install_gsi_cfg(
            GsiGame::Dota2,
            &install,
            GsiListener::Running(&live),
            &[steam.path().to_path_buf()],
        )
        .unwrap();
        assert!(Path::new(&out.path).is_file());
        assert!(out.path.contains("gamestate_integration"));
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(
            text.contains("\"uri\"       \"http://127.0.0.1:45671/gsi/dota2\""),
            "Dota posts to its own path:\n{text}"
        );
    }

    #[test]
    fn a_cs2_install_without_its_cfg_dir_is_refused_and_nothing_is_created() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let err = install_gsi_cfg(
            GsiGame::Cs2,
            &install,
            GsiListener::Running(&live),
            &[steam.path().to_path_buf()],
        )
        .unwrap_err();
        assert!(err.message.contains("fully installed"), "{}", err.message);
        assert!(!install.join("game").exists());
    }

    #[test]
    fn a_disabled_listener_refuses_to_install() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Dota2);
        let err = install_gsi_cfg(
            GsiGame::Dota2,
            &install,
            GsiListener::SwitchedOff,
            &[steam.path().to_path_buf()],
        )
        .unwrap_err();
        assert!(err.message.contains("gsi_port"));
        assert!(
            !install.join("game").exists(),
            "a listener that is off writes nothing"
        );
    }

    #[test]
    fn a_folder_that_is_not_a_steam_librarys_game_folder_is_refused() {
        let steam = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // A folder that is not `<library>/steamapps/common/<game folder>` is not somewhere
        // this writes, whatever else may be true about it.
        let install = elsewhere.path().join("somewhere/else");
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();

        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let err = install_gsi_cfg(
            GsiGame::Cs2,
            &install,
            GsiListener::Running(&live),
            &[steam.path().to_path_buf()],
        )
        .unwrap_err();
        assert!(
            err.message.contains("steamapps/common"),
            "the reason names the shape that was expected: {}",
            err.message
        );
        assert!(!install.join("game/csgo/cfg").join(GSI_CFG_NAME).exists());
    }

    /// A secondary library is a real library, and refusing it would break a game on a
    /// second drive for no security gain: the VDF is the user's own Steam configuration,
    /// and the application writes with that user's own permissions either way.
    #[test]
    fn a_game_in_a_secondary_library_is_installed() {
        let steam = tempfile::tempdir().unwrap();
        let secondary = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(steam.path().join("steamapps")).unwrap();
        std::fs::write(
            steam.path().join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\"0\"\n{{\n\"path\" \"{}\"\n}}\n}}\n",
                secondary.path().display()
            ),
        )
        .unwrap();

        // The game is installed in the secondary library, the way Steam lays one out.
        let game_dir = secondary
            .path()
            .join("steamapps")
            .join("common")
            .join(GsiGame::Cs2.folder());
        std::fs::create_dir_all(game_dir.join("game").join("csgo").join("cfg")).unwrap();

        // The detection finds it: that is what the library list is for.
        let libs = steam_libraries(&[steam.path().to_path_buf()]);
        let install = find_install(GsiGame::Cs2, &libs).unwrap();
        assert_eq!(install, game_dir);

        // And the install goes through: the folder is that library's
        // `steamapps/common/<game folder>`.
        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let outcome = install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &libs)
            .expect("a game in a secondary library is installed");
        assert!(Path::new(&outcome.path).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_the_previous_file_intact_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        let cfg_dir = install.join("game/csgo/cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let roots = [steam.path().to_path_buf()];

        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let out =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &roots).unwrap();
        let before = std::fs::read_to_string(&out.path).unwrap();

        // The directory cannot be written to any more, so the write fails where it fails
        // in the field: on the file, with the old one still in place.
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let moved = binding(GsiGame::Cs2, 45672, TOKEN);
        let err = install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&moved), &roots)
            .unwrap_err();
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(err.code, ErrorCode::Io);
        assert_eq!(
            std::fs::read_to_string(&out.path).unwrap(),
            before,
            "the old file survived the failed write"
        );
        assert_eq!(
            names(&cfg_dir),
            vec![GSI_CFG_NAME.to_string()],
            "and no temp file was left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_install_whose_destination_is_a_symlink_is_refused_and_nothing_is_written_through_it() {
        let steam = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        let cfg_dir = install.join("game/csgo/cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let precious = outside.path().join("precious.txt");
        std::fs::write(&precious, "not the token").unwrap();
        std::os::unix::fs::symlink(&precious, cfg_dir.join(GSI_CFG_NAME)).unwrap();

        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let err = install_gsi_cfg(
            GsiGame::Cs2,
            &install,
            GsiListener::Running(&live),
            &[steam.path().to_path_buf()],
        )
        .unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert_eq!(
            std::fs::read_to_string(&precious).unwrap(),
            "not the token",
            "the file the link pointed at was not written through"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_install_replaces_the_file_rather_than_rewriting_it_in_place() {
        use std::os::unix::fs::MetadataExt;
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();
        let roots = [steam.path().to_path_buf()];

        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let out =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &roots).unwrap();
        let first = std::fs::metadata(&out.path).unwrap().ino();

        let moved = binding(GsiGame::Cs2, 45699, TOKEN);
        install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&moved), &roots).unwrap();
        let second = std::fs::metadata(&out.path).unwrap().ino();

        assert_ne!(
            first, second,
            "the file was replaced, not truncated and rewritten: a reader sees the old file \
             or the new one, never half of either"
        );
    }

    #[test]
    fn a_write_that_fails_midway_leaves_the_previous_file_intact_and_leaves_no_temp_file() {
        // The failure the atomic write exists for: the write breaks after the temporary
        // file exists. A write in place would have truncated the file and left it empty.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(GSI_CFG_NAME);
        std::fs::write(&dest, "the old cfg").unwrap();

        let err = write_cfg_atomically_with(&dest, "the new cfg", |_, _| {
            Err(std::io::Error::other("the disk is full"))
        })
        .unwrap_err();

        assert!(err.to_string().contains("the disk is full"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "the old cfg",
            "the old file survived the failed write"
        );
        assert_eq!(
            names(dir.path()),
            vec![GSI_CFG_NAME.to_string()],
            "and no temp file was left behind"
        );
    }

    #[test]
    fn a_rename_that_fails_leaves_the_old_file_and_clears_the_temp_file() {
        // The destination cannot be replaced (it is a directory), so the rename fails after
        // the write succeeded: the temp file has to go, and the old file has to stay.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(GSI_CFG_NAME);
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("a file"), b"in the way").unwrap();

        let err = write_cfg_atomically(&dest, "the new cfg").unwrap_err();

        assert!(
            err.raw_os_error().is_some(),
            "the rename is what failed: {err}"
        );
        assert_eq!(
            names(dir.path()),
            vec![GSI_CFG_NAME.to_string()],
            "the temp file was cleared"
        );
        assert_eq!(
            names(&dest),
            vec!["a file".to_string()],
            "and the directory it stood for is untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_install_whose_cfg_dir_is_a_symlink_out_of_the_game_is_refused() {
        let steam = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo")).unwrap();
        std::os::unix::fs::symlink(outside.path(), install.join("game/csgo/cfg")).unwrap();
        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        assert!(install_gsi_cfg(
            GsiGame::Cs2,
            &install,
            GsiListener::Running(&live),
            &[steam.path().to_path_buf()]
        )
        .is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_install_recorded_at_one_port_is_rewritten_when_the_listener_moves() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();
        let roots = [steam.path().to_path_buf()];

        let first = binding(GsiGame::Cs2, 45671, TOKEN);
        let out =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&first), &roots).unwrap();

        let mut installed = Vec::new();
        remember_install(
            &mut installed,
            GsiGame::Cs2,
            &install,
            Path::new(&out.path),
            &first,
        );

        // The listener comes back on another port: the installed file has to follow it, or
        // the game posts into the void.
        let moved = binding(GsiGame::Cs2, 45699, TOKEN);
        refresh_installed_gsi(&mut installed, &[moved], &roots);

        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(
            text.contains("\"uri\"       \"http://127.0.0.1:45699/gsi/cs2\""),
            "the installed file names the port the listener bound:\n{text}"
        );
        assert_eq!(installed.len(), 1, "the installation is still remembered");
        assert_eq!(installed[0].binding.port, 45699);
    }

    #[test]
    fn an_install_that_is_already_current_is_not_rewritten() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();
        let roots = [steam.path().to_path_buf()];

        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let out =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &roots).unwrap();
        let before = std::fs::read_to_string(&out.path).unwrap();

        let mut installed = Vec::new();
        remember_install(
            &mut installed,
            GsiGame::Cs2,
            &install,
            Path::new(&out.path),
            &live,
        );
        refresh_installed_gsi(&mut installed, std::slice::from_ref(&live), &roots);

        assert_eq!(
            std::fs::read_to_string(&out.path).unwrap(),
            before,
            "a file that is already current is left alone"
        );
        assert_eq!(installed.len(), 1);
    }

    /// What an older build wrote: this build's file without the `player_id` block.
    fn without_player_id(text: &str) -> String {
        text.lines()
            .filter(|line| !line.contains("player_id"))
            .map(|line| format!("{line}\n"))
            .collect()
    }

    #[test]
    fn an_installed_cs2_cfg_missing_a_block_is_rewritten_even_when_the_binding_is_unchanged() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();
        let roots = [steam.path().to_path_buf()];

        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let out =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &roots).unwrap();
        let mut installed = Vec::new();
        remember_install(
            &mut installed,
            GsiGame::Cs2,
            &install,
            Path::new(&out.path),
            &live,
        );
        // The same port and token, in the file an older build wrote.
        let old = without_player_id(&std::fs::read_to_string(&out.path).unwrap());
        assert!(!gsi::cs2_cfg_is_current(&old));
        std::fs::write(&out.path, &old).unwrap();

        refresh_installed_gsi(&mut installed, std::slice::from_ref(&live), &roots);

        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(gsi::cs2_cfg_is_current(&text), "got:\n{text}");
        assert!(text.contains(TOKEN), "and it still carries the live token");
        assert_eq!(installed.len(), 1);
    }

    #[test]
    fn an_install_over_an_older_cfg_leaves_a_current_file_with_the_live_port_and_token() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        let cfg_dir = install.join("game/csgo/cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let roots = [steam.path().to_path_buf()];

        // What an earlier build installed: no `player_id`, another port and the legacy token.
        let legacy_token = "1111111111111111111111111111111111111111111111111111111111111111"; // gitleaks:allow
        let older = without_player_id(&gsi::integration_cfg(40000, legacy_token));
        std::fs::write(cfg_dir.join(GSI_CFG_NAME), &older).unwrap();

        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let out =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &roots).unwrap();

        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(out.replaced);
        assert!(gsi::cs2_cfg_is_current(&text), "got:\n{text}");
        assert!(text.contains(":45671/"), "the live port:\n{text}");
        assert!(text.contains(TOKEN), "the live token");
        assert!(!text.contains(legacy_token), "and not the legacy one");
    }

    #[test]
    fn an_installation_that_cannot_be_rewritten_is_dropped_rather_than_retried() {
        let steam = tempfile::tempdir().unwrap();
        let install = fake_steam(steam.path(), GsiGame::Cs2);
        std::fs::create_dir_all(install.join("game/csgo/cfg")).unwrap();
        let roots = [steam.path().to_path_buf()];

        let live = binding(GsiGame::Cs2, 45671, TOKEN);
        let out =
            install_gsi_cfg(GsiGame::Cs2, &install, GsiListener::Running(&live), &roots).unwrap();

        let mut installed = Vec::new();
        remember_install(
            &mut installed,
            GsiGame::Cs2,
            &install,
            Path::new(&out.path),
            &live,
        );

        // The game folder is gone.
        std::fs::remove_dir_all(&install).unwrap();

        let moved = binding(GsiGame::Cs2, 45699, TOKEN);
        refresh_installed_gsi(&mut installed, &[moved], &roots);

        assert!(
            installed.is_empty(),
            "a rewrite that cannot happen is not retried at every start"
        );
    }

    #[test]
    fn recorder_state_distinguishes_the_four_conditions() {
        let stopped = RecorderStatus::stopped();
        assert_eq!(recorder_state(&stopped), RecorderState::Idle);
        assert_eq!(
            recorder_state(&RecorderStatus {
                running: true,
                ..stopped.clone()
            }),
            RecorderState::Recording
        );
        assert_eq!(
            recorder_state(&RecorderStatus {
                watching_games: true,
                ..stopped.clone()
            }),
            RecorderState::Waiting
        );
        assert_eq!(
            recorder_state(&RecorderStatus {
                error: Some("boom".into()),
                ..stopped
            }),
            RecorderState::Failed
        );
    }

    #[test]
    fn event_sources_map_to_plain_strings() {
        let dtos = event_source_dtos(&[SourceReport {
            source: "CS2 / Dota 2",
            state: SourceState::Failed,
            detail: "address in use".into(),
            gsi_port: None,
            gsi_games: Vec::new(),
        }]);
        assert_eq!(dtos[0].state, "failed");
        assert_eq!(dtos[0].detail, "address in use");
    }

    #[test]
    fn a_live_gsi_report_names_the_port_the_listener_bound() {
        // The health page shows the port the listener actually bound: a configured one is
        // not the same thing once the port is ephemeral or was taken.
        let dtos = event_source_dtos(&[SourceReport {
            source: "CS2 / Dota 2",
            state: SourceState::Running,
            detail: "listening on 127.0.0.1:45699".into(),
            gsi_port: Some(45699),
            gsi_games: vec![GsiGameInfo {
                game: "cs2".into(),
                path: "/gsi/cs2".into(),
                app_id: 730,
            }],
        }]);
        assert_eq!(dtos[0].state, "running");
        assert!(dtos[0].detail.contains("45699"));
    }

    #[test]
    fn recent_problems_keeps_only_warn_and_error_levels_newest_last() {
        let log = "2026-10-17T10:00:00Z  INFO a: fine\n\
                   2026-10-17T10:00:01Z  WARN a: first\n\
                   2026-10-17T10:00:02Z  INFO a: this mentions ERROR in prose\n\
                   2026-10-17T10:00:03Z ERROR a: second\n\
                   2026-10-17T10:00:04Z  WARN a: third\n";
        assert_eq!(recent_problems(log, 10).len(), 3);
        let last_two = recent_problems(log, 2);
        assert!(last_two[0].contains("second") && last_two[1].contains("third"));
    }

    #[test]
    fn reading_a_missing_log_is_empty_not_an_error() {
        assert!(read_recent_problems(Path::new("/definitely/not/here.log"), 5).is_empty());
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("l.log");
        std::fs::write(&log, "t  ERROR x: broke\n").unwrap();
        assert_eq!(read_recent_problems(&log, 5).len(), 1);
    }

    /// The Windows lock bug, end to end through the health page's own reader.
    ///
    /// The log is held by the running process through [`crate::log_file::RotatingFile`]; the page
    /// reads it through a second handle in the same process. With a mandatory lock on the log
    /// itself that read failed with `ERROR_LOCK_VIOLATION` and `read_recent_problems` answered
    /// with an empty list, so the page that exists to show what went wrong showed nothing. (On
    /// Unix the lock is advisory, so only the Windows run can fail this.)
    #[test]
    fn the_problems_are_readable_while_the_running_process_holds_the_log() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replayed.log");
        let (mut log, owns) = crate::log_file::RotatingFile::open_locked(path.clone(), 4096, 1)
            .expect("the log opens");
        assert!(owns);
        log.write_all(b"2026-10-17T10:00:03Z ERROR a: held and visible\n")
            .unwrap();
        log.flush().unwrap();

        let problems = read_recent_problems(&path, 5);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("held and visible"));
    }

    #[test]
    fn steam_path_is_read_from_reg_query_output() {
        // The shape `reg query HKCU\Software\Valve\Steam /v SteamPath` printed on a real box, CRLFs
        // included. The data has spaces and is kept whole.
        let out = "\r\nHKEY_CURRENT_USER\\Software\\Valve\\Steam\r\n    SteamPath    REG_SZ    c:/program files (x86)/steam\r\n\r\n";
        assert_eq!(
            parse_steam_path(out),
            Some(PathBuf::from("c:/program files (x86)/steam"))
        );
        assert_eq!(
            parse_steam_path("    SteamPath    REG_SZ    D:/Steam"),
            Some(PathBuf::from("D:/Steam"))
        );
        // Another value, an empty one and no value at all are no answer.
        assert_eq!(parse_steam_path("    SteamPathX    REG_SZ    D:/x"), None);
        assert_eq!(parse_steam_path("    SteamPath    REG_SZ    "), None);
        assert_eq!(parse_steam_path("    SteamPath    REG_DWORD    0x1"), None);
        assert_eq!(parse_steam_path(""), None);
    }

    /// The real registry read, on a machine that has Steam. Where the key is absent (no Steam, or
    /// a CI image) the read is "no answer" and there is nothing to assert; where it is present the
    /// answer must be the directory Steam registered, and it must lead the roots.
    #[cfg(windows)]
    #[test]
    fn the_registry_read_returns_the_steam_directory_when_steam_is_installed() {
        if let Some(path) = registered_steam_path() {
            assert!(path.is_dir(), "the registered Steam path exists: {path:?}");
            assert_eq!(steam_roots().first(), Some(&path));
        }
    }

    #[test]
    fn a_steam_installed_outside_program_files_is_a_root_and_is_looked_in_first() {
        // The reported failure: Steam at `D:\Steam`, which no fixed path names.
        let steam = tempfile::tempdir().unwrap();
        let roots = steam_roots_from(Some(steam.path().to_path_buf()));
        assert_eq!(roots.first(), Some(&steam.path().to_path_buf()));

        // And a game in it is found through the library list that root yields.
        let install = steam
            .path()
            .join("steamapps/common")
            .join(GsiGame::Cs2.folder());
        std::fs::create_dir_all(&install).unwrap();
        let libs = steam_libraries(&roots);
        assert_eq!(find_install(GsiGame::Cs2, &libs), Some(install));

        // With no registry answer the default locations are still there.
        assert_eq!(
            steam_roots_from(None).len() + 1,
            roots.len(),
            "the registry adds a root and removes none"
        );
    }

    #[test]
    fn disk_health_answers_for_a_directory_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let health = disk_health(&dir.path().join("not/yet")).unwrap();
        assert!(health.total_bytes >= health.free_bytes && health.total_bytes > 0);
    }

    #[test]
    fn the_health_json_keys_are_the_typescript_interface() {
        let health = HealthDto {
            recorder_state: RecorderState::Idle,
            recorder_error: None,
            encoder: None,
            ffmpeg_available: true,
            disk: None,
            event_sources: vec![],
            recent_errors: vec![],
            log_path: String::new(),
        };
        let value = serde_json::to_value(&health).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "disk",
                "encoder",
                "event_sources",
                "ffmpeg_available",
                "log_path",
                "recent_errors",
                "recorder_error",
                "recorder_state"
            ]
        );
    }
}
