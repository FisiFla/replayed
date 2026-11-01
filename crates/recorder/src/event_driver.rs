//! Starting the game-event integrations and acting on what they derive.
//!
//! Both front-ends run the League poller and the CS2 / Dota 2 listener the `[events]` section asked
//! for, and turn each derived event into a clip (a highlight) or a marker. It lives beside the
//! engine it drives so the two front-ends cannot drift apart.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use replayed_events::gsi::integration::{game_path, integration_cfg_for, CS2_APP_ID, DOTA2_APP_ID};
use replayed_events::gsi::{self, GsiConfig, GsiGameBinding, GsiListener};
use replayed_events::lol::{self, LolConfig, LolHandle};
use replayed_events::{EventKind, GameEvent};
use serde::Deserialize;

use crate::{ClipReason, ClipWindow, RecordedClip, Recorder};

/// `[events]`: which game-event sources to run.
///
/// The League endpoint and the GSI path are fixed; the GSI token is generated into the application
/// data directory rather than a user-edited file (see `gsi::load_or_create_token`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventsSection {
    /// Poll the League Live Client Data API. Harmless with no game running: a refused loopback
    /// connection is the normal state.
    ///
    /// Defaulted ([`DEFAULT_GSI_PORT`] and `true` come from [`EventsSection::default`]) so a
    /// `[events]` table that leaves a key out parses, as `config.example.toml` says it may.
    #[serde(default = "default_lol_poll_enabled")]
    pub lol_poll_enabled: bool,
    /// The port the CS2 / Dota 2 GSI listener binds on loopback, or `0` to leave it off.
    #[serde(default = "default_gsi_port")]
    pub gsi_port: u16,
    /// Let the operating system pick the GSI port instead of binding `gsi_port`.
    ///
    /// The generated `.cfg` names the port the listener actually bound either way, but a
    /// fixed port can be squatted on: another local process binds it before this one starts,
    /// and the game then posts to that process — the token with it. The ephemeral mode is the
    /// answer for anyone who does not need a fixed port.
    ///
    /// `events.gsi_port = 0` still means "the listener is off", in both modes: this key
    /// chooses *which* port is bound, not whether it is. The configured `gsi_port` is ignored
    /// while it is on — the OS picks the port, and the `.cfg` names the one it picked.
    #[serde(default)]
    pub gsi_ephemeral_port: bool,
    /// Highlight kinds that must not take a clip (tags from `EventKind::as_tag`, e.g.
    /// `["objective", "assist"]`). A muted kind is still recorded as a marker. Unknown tags are
    /// rejected at load, so a typo cannot silently mute nothing.
    #[serde(default)]
    pub mute_clips: Vec<String>,
    /// `[events.lol]`: clip length for League events; unset keys use `[buffer]`.
    #[serde(default)]
    pub lol: GameClipSection,
    /// `[events.gsi]`: clip length for CS2 / Dota 2 events (one listener serves both).
    #[serde(default)]
    pub gsi: GameClipSection,
}

/// A game's own clip length: `pre_seconds` before the event and `post_seconds` after.
///
/// Either key may be omitted (that half uses `[buffer]`); values above `[buffer] max_pre_seconds` /
/// `max_post_seconds` are clamped, not refused (see [`ClipWindow`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct GameClipSection {
    pub pre_seconds: Option<u64>,
    pub post_seconds: Option<u64>,
}

impl GameClipSection {
    pub fn window(self) -> ClipWindow {
        ClipWindow {
            pre_seconds: self.pre_seconds,
            post_seconds: self.post_seconds,
        }
    }
}

/// The GSI port a file that does not name one gets: above the ephemeral range, so a listener bound
/// elsewhere at startup cannot shadow it.
pub const DEFAULT_GSI_PORT: u16 = 45671;

fn default_lol_poll_enabled() -> bool {
    true
}

fn default_gsi_port() -> u16 {
    DEFAULT_GSI_PORT
}

impl EventsSection {
    /// Refuse a GSI port that cannot be bound as written.
    ///
    /// `0` disables the listener. Anything else must be a real, non-privileged port: the default sits
    /// above the ephemeral range so a temporarily bound listener elsewhere cannot shadow it. Moved here
    /// from the CLI's loader so both front-ends share it.
    pub fn validate(&self) -> Result<()> {
        if self.gsi_port != 0 && self.gsi_port < 1024 {
            anyhow::bail!(
                "events.gsi_port must be 0 (disabled) or between 1024 and 65535, got {}",
                self.gsi_port
            );
        }
        Ok(())
    }

    /// The muted kinds, or the first tag that is not one.
    pub fn muted_kinds(&self) -> Result<Vec<EventKind>> {
        self.mute_clips
            .iter()
            .map(|tag| {
                EventKind::from_tag(tag).with_context(|| {
                    format!("events.mute_clips: `{tag}` is not an event kind (kill, death, assist, objective, dragon, baron, herald, turret, inhibitor, bomb_planted, bomb_defused, game_end, ...)")
                })
            })
            .collect()
    }
}

/// What the driver does with events: how bursts coalesce, and which highlights stay markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub coalesce: Coalesce,
    pub muted: Vec<EventKind>,
    /// Per-game clip lengths (`[events.lol]`, `[events.gsi]`).
    pub lol: ClipWindow,
    pub gsi: ClipWindow,
}

impl Policy {
    pub fn new(events: &EventsSection, pre_seconds: u64) -> Result<Self> {
        Ok(Self {
            coalesce: Coalesce::for_pre_roll(pre_seconds),
            muted: events.muted_kinds()?,
            lol: events.lol.window(),
            gsi: events.gsi.window(),
        })
    }

    /// Whether this event takes a clip: a highlight that is not muted. Muting `objective` mutes every
    /// objective sub-kind; muting a sub-kind mutes only that one.
    pub fn clips(&self, event: &GameEvent) -> bool {
        event.is_highlight()
            && !self.muted.contains(&event.kind)
            && !(event.kind.is_objective() && self.muted.contains(&EventKind::Objective))
    }

    /// The clip length for an event from `source`: the game's own, else the buffer's.
    pub fn window_for(&self, source: replayed_events::Source) -> ClipWindow {
        match source {
            replayed_events::Source::Lol => self.lol,
            replayed_events::Source::Gsi => self.gsi,
            replayed_events::Source::Manual => ClipWindow::default(),
        }
    }
}

impl Default for EventsSection {
    /// The values `config.example.toml` ships, so a file with no `[events]` section behaves as the example does.
    fn default() -> Self {
        Self {
            lol_poll_enabled: true,
            gsi_port: DEFAULT_GSI_PORT,
            gsi_ephemeral_port: false,
            mute_clips: Vec::new(),
            lol: GameClipSection::default(),
            gsi: GameClipSection::default(),
        }
    }
}

/// The event sources a front-end was configured to run, and the channel they report on.
///
/// Dropping this stops the sources: each handle joins its own thread on `Drop`, so they are held
/// here so nothing keeps polling a game or a port after the driver loop ends.
pub struct EventSources {
    /// Derived events, in the order the sources produced them.
    pub events: Receiver<GameEvent>,
    /// Kept alive (not read): dropping it stops the poller.
    pub _lol: Option<LolHandle>,
    /// The same for the GSI listener, together with the bindings it serves.
    pub _gsi: Option<GsiHandle>,
    /// What starting each source concluded, for a health readout; empty when assembled by hand.
    pub reports: Vec<SourceReport>,
}

impl EventSources {
    /// The games the GSI listener serves right now, with the live port and token.
    ///
    /// Empty when the listener is off (`events.gsi_port = 0`) or failed to start: there is
    /// then nothing serving, so there is nothing to install a `.cfg` for.
    pub fn gsi_bindings(&self) -> Vec<GsiBinding> {
        self._gsi
            .as_ref()
            .map(|handle| handle.bindings().to_vec())
            .unwrap_or_default()
    }
}

/// The running GSI listener and the bindings it serves.
///
/// The listener holds the tokens and does not hand them back out (it only answers whether a
/// candidate matches), but a caller that writes the game's own `.cfg` needs the token itself.
/// Keeping the two together is what stops the file and the listener from disagreeing.
pub struct GsiHandle {
    listener: GsiListener,
    bindings: Vec<GsiBinding>,
}

impl GsiHandle {
    /// The listener itself, for a caller that wants to stop it or ask which token it accepts.
    pub fn listener(&self) -> &GsiListener {
        &self.listener
    }

    /// The games the listener serves, with the live port and the token in force.
    pub fn bindings(&self) -> &[GsiBinding] {
        &self.bindings
    }
}

/// A game the GSI listener serves, with everything a caller needs to write the game's cfg.
///
/// The token is the one the listener accepts, and this crate writes it to disk anyway (the
/// `.cfg` in the application's own data directory carries it), so handing it to a caller that
/// writes the game's own copy is no new exposure. It is never logged, and this struct's
/// `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct GsiBinding {
    /// "cs2" or "dota2" — the game id.
    pub game: String,
    /// The path the game posts to.
    pub path: String,
    /// The port the listener bound; the one the `.cfg` must name.
    pub port: u16,
    /// Valve's app id.
    pub app_id: u32,
    /// The token the listener accepts for this game.
    pub token: String,
}

impl GsiBinding {
    /// The game's `.cfg`, ready to install into the game's own directory.
    ///
    /// Built from the live port, the game's own path and the token in force, so the file a
    /// caller installs cannot disagree with the running listener.
    pub fn cfg(&self) -> String {
        integration_cfg_for(
            self.port,
            &self.path,
            &self.token,
            self.game == GSI_GAME_DOTA2,
        )
    }
}

impl std::fmt::Debug for GsiBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GsiBinding")
            .field("game", &self.game)
            .field("path", &self.path)
            .field("port", &self.port)
            .field("app_id", &self.app_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// One game the GSI listener serves, as a health readout shows it.
///
/// The port the game must post to is on the report itself
/// ([`SourceReport::gsi_port`]); this is the per-game half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsiGameInfo {
    /// "cs2" or "dota2" — the game id, NOT the events crate's name.
    pub game: String,
    /// The path the game posts to.
    pub path: String,
    /// Valve's app id.
    pub app_id: u32,
}

/// How a source came out of startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    Running,
    Disabled,
    Failed,
}

/// One source's startup outcome. `detail` is user-facing and never carries the GSI token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReport {
    pub source: &'static str,
    pub state: SourceState,
    pub detail: String,
    /// The port the GSI listener actually bound, when the source is running.
    ///
    /// The live port, not the configured one: an ephemeral bind, or a configured port that
    /// could not be had, are both ways for the game and the listener to disagree.
    pub gsi_port: Option<u16>,
    /// The games the listener serves, with the port and path the game must post to.
    pub gsi_games: Vec<GsiGameInfo>,
}

/// Start whatever `[events]` asked for, best effort.
///
/// Sources are independent and none is essential (no League install, or a GSI port or token file
/// that fails). A failure is a warning naming the setting, never a reason to refuse to record;
/// the hotkey is the fallback.
pub fn start_event_sources(app_dir: &Path, events: &EventsSection) -> EventSources {
    let (sink, events_rx) = mpsc::channel::<GameEvent>();

    let mut reports = Vec::new();
    let lol = if events.lol_poll_enabled {
        match lol::spawn(LolConfig::live_client(), sink.clone()) {
            Ok(handle) => {
                tracing::info!(
                    "League of Legends: polling {} for game events (set \
                     events.lol_poll_enabled = false to stop)",
                    handle.endpoint().url()
                );
                reports.push(SourceReport {
                    source: "League of Legends",
                    state: SourceState::Running,
                    detail: format!("polling {}", handle.endpoint().url()),
                    gsi_port: None,
                    gsi_games: Vec::new(),
                });
                Some(handle)
            }
            Err(err) => {
                tracing::warn!(
                    "events.lol_poll_enabled is set but polling could not start: {err:#}"
                );
                reports.push(SourceReport {
                    source: "League of Legends",
                    state: SourceState::Failed,
                    detail: format!("{err:#}"),
                    gsi_port: None,
                    gsi_games: Vec::new(),
                });
                None
            }
        }
    } else {
        tracing::info!("League of Legends polling is disabled (events.lol_poll_enabled = false)");
        reports.push(SourceReport {
            source: "League of Legends",
            state: SourceState::Disabled,
            detail: "events.lol_poll_enabled = false".into(),
            gsi_port: None,
            gsi_games: Vec::new(),
        });
        None
    };

    let gsi = if events.gsi_port == 0 {
        tracing::info!("the CS2 / Dota 2 listener is disabled (events.gsi_port = 0)");
        reports.push(SourceReport {
            source: "CS2 / Dota 2",
            state: SourceState::Disabled,
            detail: "events.gsi_port = 0".into(),
            gsi_port: None,
            gsi_games: Vec::new(),
        });
        None
    } else {
        match start_gsi(
            app_dir,
            events.gsi_port,
            events.gsi_ephemeral_port,
            sink.clone(),
        ) {
            Ok(handle) => {
                // The port the listener bound, which is the port the game must be told about.
                let port = handle.listener().port();
                reports.push(SourceReport {
                    source: "CS2 / Dota 2",
                    state: SourceState::Running,
                    detail: format!("listening on 127.0.0.1:{port}"),
                    gsi_port: Some(port),
                    gsi_games: handle
                        .bindings()
                        .iter()
                        .map(|binding| GsiGameInfo {
                            game: binding.game.clone(),
                            path: binding.path.clone(),
                            app_id: binding.app_id,
                        })
                        .collect(),
                });
                Some(handle)
            }
            Err(err) => {
                tracing::warn!("events.gsi_port is set but the listener could not start: {err:#}");
                reports.push(SourceReport {
                    source: "CS2 / Dota 2",
                    state: SourceState::Failed,
                    detail: format!("{err:#}"),
                    gsi_port: None,
                    gsi_games: Vec::new(),
                });
                None
            }
        }
    };

    EventSources {
        events: events_rx,
        _lol: lol,
        _gsi: gsi,
        reports,
    }
}

/// The name of the file both games read, and the one this crate writes into its own data
/// directory for the user to install.
pub const GSI_CFG_NAME: &str = "gamestate_integration_replayed.cfg";

/// The game id of Counter-Strike 2, as the bindings and the reports name it.
pub const GSI_GAME_CS2: &str = "cs2";

/// The game id of Dota 2, as the bindings and the reports name it.
pub const GSI_GAME_DOTA2: &str = "dota2";

/// The token file of the installs that predate per-game tokens: one token for both games.
const LEGACY_TOKEN_FILE: &str = "gsi-token.txt";

/// One game the GSI listener serves.
struct GsiGame {
    /// The game id: the folder under `gsi/`, and the id the path is derived from.
    id: &'static str,
    /// The name the log line uses.
    name: &'static str,
    /// Valve's app id; the listener refuses a payload that says it is another game.
    app_id: u32,
    /// Whether the `.cfg` is Dota's rather than Counter-Strike's.
    dota: bool,
}

/// The two games, each with its own token and its own path.
const GSI_GAMES: [GsiGame; 2] = [
    GsiGame {
        id: GSI_GAME_CS2,
        name: "Counter-Strike 2 / CS:GO",
        app_id: CS2_APP_ID,
        dota: false,
    },
    GsiGame {
        id: GSI_GAME_DOTA2,
        name: "Dota 2",
        app_id: DOTA2_APP_ID,
        dota: true,
    },
];

/// One game, ready to be served: its path, its token and its id.
struct GameSetup {
    id: &'static str,
    name: &'static str,
    app_id: u32,
    dota: bool,
    path: String,
    token: String,
}

/// Bring up the GSI listener and write out the files the user has to install.
///
/// Never writes into a game's installation directory: it generates the files into its own data
/// directory and says where to copy them (the desktop settings button is the one user-triggered
/// installer). The tokens are persisted so the installed files keep working across runs, and
/// are never logged.
///
/// The order is the point:
///
/// 1. each game's token is read (or generated) into a file of its own;
/// 2. the listener is bound, and the port it bound is the port the game is told about;
/// 3. only then are the `.cfg` files written, each naming that live port, its own path and
///    its own token.
///
/// A `.cfg` written before the bind names a port nobody may be listening on — a taken port, or
/// an ephemeral one the OS picked — and the game posts into the void. Writing the files after
/// the bind is what makes the file and the listener agree.
fn start_gsi(
    app_dir: &Path,
    configured_port: u16,
    ephemeral: bool,
    sink: replayed_events::EventSink,
) -> Result<GsiHandle> {
    let games: Vec<GameSetup> = GSI_GAMES
        .iter()
        .map(|game| {
            Ok(GameSetup {
                id: game.id,
                name: game.name,
                app_id: game.app_id,
                dota: game.dota,
                path: game_path(game.id),
                token: per_game_token(app_dir, game.id)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Bind FIRST. The port the listener got is the port the game is told about, whether it was
    // configured, picked by the OS, or the only one left.
    let bind_port = if ephemeral { 0 } else { configured_port };
    let listener = gsi::spawn(gsi_config(bind_port, &games), sink)?;
    let live_port = listener.port();

    // Then the files the user installs: one per game, each with its own token and its own path.
    let mut bindings = Vec::new();
    for game in &games {
        let dir = app_dir.join("gsi").join(game.id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(GSI_CFG_NAME);
        let text = integration_cfg_for(live_port, &game.path, &game.token, game.dota);
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(
            "{}: copy {} into the game's cfg directory and restart the game",
            game.name,
            path.display()
        );
        bindings.push(GsiBinding {
            game: game.id.to_string(),
            path: game.path.clone(),
            port: live_port,
            app_id: game.app_id,
            token: game.token.clone(),
        });
    }

    tracing::info!(
        "the CS2 / Dota 2 listener is up on 127.0.0.1:{live_port}; the tokens the games must \
         carry are in {}/gsi-token-<game>.txt (never logged)",
        app_dir.display()
    );
    Ok(GsiHandle { listener, bindings })
}

/// The listener's configuration: one binding per game, each with its own token and path.
///
/// The binding is what scopes a token, so a token lifted from one game's installed `.cfg` is
/// good for that game's path only.
fn gsi_config(port: u16, games: &[GameSetup]) -> GsiConfig {
    GsiConfig {
        port,
        games: games
            .iter()
            .map(|game| {
                GsiGameBinding::new(game.path.clone(), game.token.clone(), Some(game.app_id))
            })
            .collect(),
    }
}

/// The token of one game, in a file of its own.
///
/// One token per game: a token lifted from one game's installed `.cfg` must not be able to
/// forge the other game's events. The old single `gsi-token.txt` of the installs that predate this
/// is deliberately **not** carried over. Copying it into each game's file made both games hold
/// the same token, which is the exact property the split exists to remove, and a compatibility
/// path that quietly keeps a shared secret alive is a way back to the weakness. Each game gets a
/// fresh token the first time; the games' installed `.cfg` files carry the old one, so the
/// integration has to be installed again (the desktop's "Install integration"), and this logs
/// that. The old file is left where it is, unread: an older build still finds its token there.
fn per_game_token(app_dir: &Path, game: &str) -> Result<String> {
    let path = app_dir.join(format!("gsi-token-{game}.txt"));
    let legacy = app_dir.join(LEGACY_TOKEN_FILE);
    if !path.exists() && legacy.is_file() {
        tracing::warn!(
            "{game}: a new GSI token was generated instead of reusing the shared one in {}; \
             install the {game} integration again so the game carries it",
            legacy.display()
        );
    }
    gsi::load_or_create_token(&path)
}

/// Do what a derived event asks for. The only decision, highlight versus marker, is the
/// vocabulary's ([`replayed_events::EventKind::is_highlight`]).
pub fn act_on_event(recorder: &Recorder, event: GameEvent) -> Result<Option<RecordedClip>> {
    act_on_event_in(recorder, event, ClipWindow::default())
}

/// [`act_on_event`] with the clip length the event's game asked for.
pub fn act_on_event_in(
    recorder: &Recorder,
    event: GameEvent,
    window: ClipWindow,
) -> Result<Option<RecordedClip>> {
    act_on_event_at(
        recorder,
        TimedEvent {
            event,
            at_ms: recorder.media_now_ms(),
        },
        window,
    )
}

/// A derived event and the media time it arrived at.
///
/// The time is sampled when the event *arrives*, not when it is acted on. A burst is coalesced
/// before its clip is cut — up to 60% of the pre-roll, which is 18 seconds at the default
/// `pre_seconds` — and a marker stamped at the end of that wait lands that long after the
/// moment it describes. The arrival time is the only honest answer, and it is the one the
/// clip's chapter and the `events` row are written with.
#[derive(Debug, Clone)]
pub struct TimedEvent {
    /// The event itself.
    pub event: GameEvent,
    /// The recorder's media timeline position when the event arrived.
    ///
    /// The media timeline, not the wall clock: the footage is measured on it, so a marker
    /// stamped with it lands next to the clip that covers the moment.
    pub at_ms: u64,
}

/// [`act_on_event_in`] with the moment the event happened.
pub fn act_on_event_at(
    recorder: &Recorder,
    timed: TimedEvent,
    window: ClipWindow,
) -> Result<Option<RecordedClip>> {
    if timed.event.is_highlight() {
        let described = format!("{} {}", timed.event.source, timed.event.kind);
        recorder
            .clip_now_in_at(
                ClipReason::GameEvent(timed.event),
                window,
                Some(timed.at_ms),
            )
            .map(|clip| {
                tracing::info!("the {described} clip is {}", clip.metadata.path.display());
                Some(clip)
            })
            .with_context(|| format!("taking a clip for a {described} event"))
    } else {
        let described = format!("{} {}", timed.event.source, timed.event.kind);
        recorder
            .note_event_at(timed.event, Some(timed.at_ms))
            .map(|id| {
                tracing::info!("recorded the {described} marker as event #{id}");
                None
            })
            .with_context(|| format!("recording a {described} marker"))
    }
}

/// How long a burst of highlights may grow before it becomes one clip.
///
/// A pentakill is five kills in ten seconds; one clip over the settled burst replaces five
/// overlapping files. The cut happens `quiet` after the last highlight or `max` after the first,
/// whichever is sooner, and `max` stays inside the pre-roll so the first event is still buffered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coalesce {
    /// How long without another highlight ends a burst.
    pub quiet: Duration,
    /// The longest a burst may run from its first event.
    pub max: Duration,
}

impl Coalesce {
    /// Sized from the buffer's pre-roll: a burst may run 60% of it and ends after a third of that
    /// without news (never under a second, never over six).
    pub fn for_pre_roll(pre_seconds: u64) -> Self {
        let max = Duration::from_millis(pre_seconds.saturating_mul(600));
        let quiet = (max / 3).clamp(Duration::from_secs(1), Duration::from_secs(6));
        Self {
            quiet,
            max: max.max(quiet),
        }
    }
}

/// Everything in one burst, `first` included, in arrival order. Markers arriving meanwhile are
/// collected but neither extend nor end the burst; the channel closing ends it.
///
/// Each member carries the media time it arrived at: the burst is waited out before the clip is
/// cut, and a marker stamped with the request's instant would name the end of the wait rather
/// than the moment the event happened.
///
/// `stop` ends the wait early with the burst so far. The wait can run for most of the pre-roll (18
/// seconds at the default `pre_seconds`) and the driver thread is joined when its [`EventDriver`] is
/// dropped, so a drop in the middle of a burst used to hold the application's shutdown for that long.
/// The wait is sliced into [`DRIVER_POLL`] steps so the flag is seen within one of them.
///
/// `next` is the source of events: it waits up to the given time for one and returns it already
/// stamped (see [`stamp_on_receipt`] for the caller that has only a bare channel).
fn gather_burst(
    first: TimedEvent,
    next: &dyn Fn(Duration) -> Result<TimedEvent, RecvTimeoutError>,
    policy: &Policy,
    stop: &AtomicBool,
) -> Vec<TimedEvent> {
    let c = policy.coalesce;
    let started = std::time::Instant::now();
    let mut last_highlight = started;
    let mut burst = vec![first];
    loop {
        let now = std::time::Instant::now();
        let quiet_left = c.quiet.saturating_sub(now.duration_since(last_highlight));
        let max_left = c.max.saturating_sub(now.duration_since(started));
        let wait = quiet_left.min(max_left);
        if wait.is_zero() || stop.load(Ordering::Acquire) {
            return burst;
        }
        match next(wait.min(DRIVER_POLL)) {
            Ok(timed) => {
                if policy.clips(&timed.event) {
                    last_highlight = std::time::Instant::now();
                }
                burst.push(timed);
            }
            // A slice that passed quietly: the loop re-reads the clocks and the stop flag.
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return burst,
        }
    }
}

/// An event source for [`gather_burst`] over a bare channel: each event is stamped with the media
/// position at the moment it is *received here*.
///
/// Right for a caller that reads the channel as events arrive (the CLI's loop). The driver, which can be
/// busy cutting a clip while events queue, stamps them as they arrive instead (see [`drive`]).
fn stamp_on_receipt<'a>(
    rx: &'a Receiver<GameEvent>,
    media_now: &'a dyn Fn() -> u64,
) -> impl Fn(Duration) -> Result<TimedEvent, RecvTimeoutError> + 'a {
    move |wait| {
        rx.recv_timeout(wait).map(|event| TimedEvent {
            event,
            at_ms: media_now(),
        })
    }
}

/// [`act_on_event`] with burst coalescing: a highlight waits for its burst to settle, then one clip
/// is cut (reason: the first highlight) and every other event is recorded as a marker. Returns the clip if one was taken.
pub fn act_on_burst(
    recorder: &Recorder,
    first: GameEvent,
    rx: &Receiver<GameEvent>,
    policy: &Policy,
) -> Result<Option<RecordedClip>> {
    act_on_burst_until(recorder, first, rx, policy, &AtomicBool::new(false))
}

/// [`act_on_burst`] that gives up waiting for the burst to settle once `stop` is set, and cuts the
/// clip from what has been gathered (see [`gather_burst`]).
pub fn act_on_burst_until(
    recorder: &Recorder,
    first: GameEvent,
    rx: &Receiver<GameEvent>,
    policy: &Policy,
    stop: &AtomicBool,
) -> Result<Option<RecordedClip>> {
    // The moment the first event arrived: the caller received it just before calling, and the
    // burst it belongs to may not be cut for another whole coalescing window.
    let first = TimedEvent {
        event: first,
        at_ms: recorder.media_now_ms(),
    };
    let media_now = || recorder.media_now_ms();
    let next = stamp_on_receipt(rx, &media_now);
    act_on_timed_burst(recorder, first, &next, policy, stop)
}

/// [`act_on_burst_until`] for events that were stamped when they arrived.
fn act_on_timed_burst(
    recorder: &Recorder,
    first: TimedEvent,
    next: &dyn Fn(Duration) -> Result<TimedEvent, RecvTimeoutError>,
    policy: &Policy,
    stop: &AtomicBool,
) -> Result<Option<RecordedClip>> {
    if !policy.clips(&first.event) {
        // A marker, or a highlight the player muted: recorded on the timeline, no footage.
        return recorder
            .note_event_at(first.event, Some(first.at_ms))
            .map(|_| None)
            .context("recording a game-event marker");
    }
    let mut burst = gather_burst(first, next, policy, stop).into_iter();
    let head = burst.next().expect("a burst always holds its first event");
    let window = policy.window_for(head.event.source);
    let result = act_on_event_at(recorder, head, window);
    for rest in burst {
        // Marker-only: the clip above already covers these moments.
        if let Err(err) = recorder.note_event_at(rest.event, Some(rest.at_ms)) {
            tracing::warn!("a burst event could not be recorded as a marker: {err:#}");
        }
    }
    result
}

/// Called with every clip the driver takes, on the driver's thread, so the desktop shell can notify
/// the player without the recorder knowing about notifications.
pub type ClipHook = Box<dyn Fn(&RecordedClip) + Send + 'static>;

/// The thread that turns derived events into clips while a recording runs. Dropping it stops the
/// thread and then the sources.
pub struct EventDriver {
    reports: Vec<SourceReport>,
    gsi_bindings: Vec<GsiBinding>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EventDriver {
    /// Start the sources `events` asks for and drive `recorder` from them.
    pub fn spawn(
        recorder: Arc<Recorder>,
        app_dir: &Path,
        events: &EventsSection,
        policy: Policy,
        on_clip: ClipHook,
    ) -> Result<Self> {
        let sources = start_event_sources(app_dir, events);
        let reports = sources.reports.clone();
        // Taken before the sources move onto the driver thread: the shell that installs the
        // game's `.cfg` needs the port the listener bound and the token it accepts.
        let gsi_bindings = sources.gsi_bindings();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("replayed-events".into())
            .spawn(move || drive(&recorder, sources, &flag, &policy, &on_clip))
            .context("spawning the game-event thread")?;
        Ok(Self {
            reports,
            gsi_bindings,
            stop,
            thread: Some(thread),
        })
    }
}

impl EventDriver {
    /// How each source came out of startup, for the health page.
    pub fn reports(&self) -> &[SourceReport] {
        &self.reports
    }

    /// The games the GSI listener serves, with the live port and the token in force.
    ///
    /// The desktop shell writes the game's own `.cfg` from this: the port is the one the
    /// listener bound and the token is the one it accepts, so the file it installs cannot
    /// disagree with the running listener. Empty when the listener is off or failed to start.
    pub fn gsi_bindings(&self) -> Vec<GsiBinding> {
        self.gsi_bindings.clone()
    }
}

impl Drop for EventDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const DRIVER_POLL: Duration = Duration::from_millis(250);

pub(crate) fn drive(
    recorder: &Recorder,
    sources: EventSources,
    stop: &AtomicBool,
    policy: &Policy,
    on_clip: &ClipHook,
) {
    // The events are stamped on a thread of their own, as they arrive. This thread is held for the whole
    // of a clip (the post-roll wait and the splice), and an event stamped only when it got back to the
    // channel carried the end of that wait as its moment: the marker and the chapter named a second
    // seconds after the play, and the clip it started could miss it.
    let (stamped_tx, stamped) = mpsc::channel::<TimedEvent>();
    std::thread::scope(|scope| {
        let EventSources {
            events,
            _lol,
            _gsi,
            reports,
        } = sources;
        scope.spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match events.recv_timeout(DRIVER_POLL) {
                    Ok(event) => {
                        let at_ms = recorder.media_now_ms();
                        if stamped_tx.send(TimedEvent { event, at_ms }).is_err() {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        // The handles stay here, alive for the loop below: dropping them stops the sources.
        let _keep = (_lol, _gsi, reports);
        let next = |wait: Duration| stamped.recv_timeout(wait);
        while !stop.load(Ordering::Acquire) {
            match stamped.recv_timeout(DRIVER_POLL) {
                Ok(first) => {
                    // Nothing is recording (armed for a game, or stopped): an event then is a state, not a failure.
                    if !recorder.is_running() {
                        tracing::info!(
                            "a game event arrived while nothing is being recorded; ignored"
                        );
                        continue;
                    }
                    match act_on_timed_burst(recorder, first, &next, policy, stop) {
                        Ok(Some(clip)) => on_clip(&clip),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!("the game event could not be acted on: {err:#}")
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kill() -> GameEvent {
        GameEvent::bare(
            replayed_events::Source::Lol,
            replayed_events::EventKind::Kill,
        )
    }

    /// An event with the media time it arrived at.
    fn timed(event: GameEvent, at_ms: u64) -> TimedEvent {
        TimedEvent { event, at_ms }
    }

    fn quick() -> Policy {
        Policy {
            coalesce: Coalesce {
                quiet: Duration::from_millis(60),
                max: Duration::from_millis(400),
            },
            muted: Vec::new(),
            lol: ClipWindow::default(),
            gsi: ClipWindow::default(),
        }
    }

    #[test]
    fn a_muted_highlight_does_not_take_a_clip_and_an_unknown_tag_is_an_error() {
        let events = EventsSection {
            mute_clips: vec!["objective".into()],
            ..EventsSection::default()
        };
        let policy = Policy::new(&events, 30).unwrap();
        let objective = GameEvent::bare(replayed_events::Source::Lol, EventKind::Objective);
        assert!(!policy.clips(&objective), "muted");
        assert!(policy.clips(&kill()), "an unmuted highlight still clips");
        let bad = EventsSection {
            mute_clips: vec!["kil".into()],
            ..EventsSection::default()
        };
        assert!(bad.muted_kinds().unwrap_err().to_string().contains("kil"));
    }

    #[test]
    fn muting_objective_mutes_every_sub_kind_and_a_sub_kind_mutes_only_itself() {
        let all = EventsSection {
            mute_clips: vec!["objective".into()],
            ..EventsSection::default()
        };
        let policy = Policy::new(&all, 30).unwrap();
        for kind in EventKind::ALL.into_iter().filter(|k| k.is_objective()) {
            assert!(
                !policy.clips(&GameEvent::bare(replayed_events::Source::Lol, kind)),
                "{kind}"
            );
        }
        let some = EventsSection {
            mute_clips: vec!["turret".into()],
            ..EventsSection::default()
        };
        let policy = Policy::new(&some, 30).unwrap();
        let clips = |kind| policy.clips(&GameEvent::bare(replayed_events::Source::Lol, kind));
        assert!(!clips(EventKind::Turret));
        assert!(clips(EventKind::Dragon) && clips(EventKind::Baron) && clips(EventKind::Objective));
    }

    #[test]
    fn per_game_clip_lengths_parse_and_reach_the_policy() {
        let events: EventsSection = toml::from_str(
            "lol_poll_enabled = true\ngsi_port = 0\n[lol]\npre_seconds = 25\npost_seconds = 8\n[gsi]\npre_seconds = 6\n",
        )
        .unwrap();
        let policy = Policy::new(&events, 30).unwrap();
        assert_eq!(
            policy.window_for(replayed_events::Source::Lol),
            ClipWindow::new(25, 8)
        );
        assert_eq!(
            policy.window_for(replayed_events::Source::Gsi),
            ClipWindow {
                pre_seconds: Some(6),
                post_seconds: None
            }
        );
        assert_eq!(
            policy.window_for(replayed_events::Source::Manual),
            ClipWindow::default()
        );
    }

    #[test]
    fn a_burst_of_kills_is_gathered_into_one_group() {
        let (tx, rx) = mpsc::channel();
        for _ in 0..4 {
            tx.send(kill()).unwrap();
        }
        let burst = gather_burst(
            timed(kill(), 0),
            &stamp_on_receipt(&rx, &|| 0),
            &quick(),
            &AtomicBool::new(false),
        );
        assert_eq!(
            burst.len(),
            5,
            "the first plus the four that were already waiting"
        );
    }

    #[test]
    fn a_lone_event_ends_after_the_quiet_gap_and_not_the_max() {
        // With a `max` an hour away, only the quiet gap can end the burst: returning at all is
        // the proof, so no wall-clock bound is asserted (a loaded runner would flake on one).
        let mut policy = quick();
        policy.coalesce.max = Duration::from_secs(3_600);
        let (_tx, rx) = mpsc::channel::<GameEvent>();
        assert_eq!(
            gather_burst(
                timed(kill(), 0),
                &stamp_on_receipt(&rx, &|| 0),
                &policy,
                &AtomicBool::new(false)
            )
            .len(),
            1
        );
    }

    #[test]
    fn a_burst_wait_ends_when_the_driver_is_stopped() {
        // Dropping the driver sets the flag and joins its thread; a wait of an hour must not hold
        // that join. The bound is generous (the wait is sliced at 250ms) so a loaded runner passes.
        let mut policy = quick();
        policy.coalesce = Coalesce {
            quiet: Duration::from_secs(3_600),
            max: Duration::from_secs(3_600),
        };
        let (_tx, rx) = mpsc::channel::<GameEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        let burst = gather_burst(
            timed(kill(), 0),
            &stamp_on_receipt(&rx, &|| 0),
            &policy,
            &stop,
        );
        assert_eq!(burst.len(), 1, "the burst so far is kept");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the wait did not end with the stop flag"
        );
        setter.join().unwrap();
    }

    #[test]
    fn a_continuous_stream_is_cut_at_the_max() {
        // The stream never goes quiet and never ends until the receiver is dropped, so the
        // only way `gather_burst` returns is the max cutting it. No elapsed-time bound: a
        // run-to-the-end implementation hangs here, which fails the test without a clock.
        let (tx, rx) = mpsc::channel();
        let feeder = std::thread::spawn(move || {
            while tx.send(kill()).is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let burst = gather_burst(
            timed(kill(), 0),
            &stamp_on_receipt(&rx, &|| 0),
            &quick(),
            &AtomicBool::new(false),
        );
        assert!(
            burst.len() > 1,
            "the burst gathered the stream until the cut"
        );
        drop(rx);
        feeder.join().unwrap();
    }

    #[test]
    fn the_window_scales_with_the_pre_roll_and_stays_inside_it() {
        let c = Coalesce::for_pre_roll(30);
        assert_eq!(c.max, Duration::from_secs(18));
        assert_eq!(c.quiet, Duration::from_secs(6));
        let tiny = Coalesce::for_pre_roll(1);
        assert!(tiny.quiet >= Duration::from_secs(1));
        assert!(tiny.max >= tiny.quiet);
    }

    #[test]
    fn a_missing_events_section_means_the_example_values() {
        assert_eq!(
            EventsSection::default(),
            EventsSection {
                lol_poll_enabled: true,
                gsi_port: 45671,
                gsi_ephemeral_port: false,
                mute_clips: Vec::new(),
                lol: GameClipSection::default(),
                gsi: GameClipSection::default(),
            }
        );
    }

    #[test]
    fn the_example_file_and_the_default_agree() {
        #[derive(Deserialize)]
        struct File {
            events: EventsSection,
        }
        let text = include_str!("../../../config.example.toml");
        let file: File = toml::from_str(text).expect("the example parses");
        assert_eq!(file.events, EventsSection::default());
    }

    /// `config.example.toml` says `gsi_port` may be omitted; the table used to refuse it as missing.
    #[test]
    fn an_events_table_may_omit_the_port_and_the_poll_switch() {
        #[derive(Deserialize)]
        struct File {
            events: EventsSection,
        }
        let file: File = toml::from_str("[events]\n").expect("an empty table parses");
        assert_eq!(file.events, EventsSection::default());
        let file: File = toml::from_str("[events]\ngsi_port = 0\n").unwrap();
        assert!(file.events.lol_poll_enabled, "the omitted switch is on");
        assert_eq!(file.events.gsi_port, 0, "a stated value is kept");
    }

    #[test]
    fn a_privileged_gsi_port_is_refused_and_zero_is_off() {
        let mut events = EventsSection::default();
        assert!(events.validate().is_ok());
        events.gsi_port = 0;
        assert!(events.validate().is_ok());
        events.gsi_port = 80;
        assert!(format!("{:#}", events.validate().unwrap_err()).contains("events.gsi_port"));
    }
}
