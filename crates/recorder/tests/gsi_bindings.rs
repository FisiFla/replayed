//! The GSI listener's bindings: the port the game is told about, and the token it carries.
//!
//! Integration tests, not unit tests: everything here is what the desktop shell sees — the
//! public API of the recorder crate, the files it writes, and what those files say.
//!
//! The point of every test is the same disagreement: the file the game is given must name
//! the port the listener actually bound, and the token in it must be that game's own.

use std::net::TcpListener;
use std::path::{Path, PathBuf};

use replayed_recorder::event_driver::{
    start_event_sources, EventSources, EventsSection, GameClipSection, SourceState, GSI_CFG_NAME,
};

/// The game ids the listener serves.
const CS2: &str = "cs2";
const DOTA2: &str = "dota2";

/// Valve's app ids.
const CS2_APP_ID: u32 = 730;
const DOTA2_APP_ID: u32 = 570;

/// A configuration with the listener on and the League poller off (there is no game here).
fn events(port: u16, ephemeral: bool) -> EventsSection {
    EventsSection {
        lol_poll_enabled: false,
        gsi_port: port,
        gsi_ephemeral_port: ephemeral,
        mute_clips: Vec::new(),
        lol: GameClipSection::default(),
        gsi: GameClipSection::default(),
    }
}

/// A port nothing is listening on.
///
/// Bound and released, so the port is free: the recorder binds it a moment later. The race
/// with another process taking it is the same one any fixed port has.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free loopback port");
    listener.local_addr().expect("the bound address").port()
}

/// The `.cfg` this crate wrote for a game.
fn cfg(dir: &Path, game: &str) -> String {
    let path = dir.join("gsi").join(game).join(GSI_CFG_NAME);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}

/// The token this crate stored for a game.
fn token(dir: &Path, game: &str) -> String {
    let path = dir.join(format!("gsi-token-{game}.txt"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
        .trim()
        .to_string()
}

/// The report of the GSI source.
fn gsi_report(sources: &EventSources) -> &replayed_recorder::event_driver::SourceReport {
    sources
        .reports
        .iter()
        .find(|report| report.source == "CS2 / Dota 2")
        .expect("the GSI source is reported")
}

/// The port the `.cfg` names: the one in the `uri` line.
fn port_named_by(cfg: &str) -> u16 {
    let marker = "http://127.0.0.1:";
    let start = cfg
        .find(marker)
        .unwrap_or_else(|| panic!("the cfg names an endpoint:\n{cfg}"))
        + marker.len();
    let rest = &cfg[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end]
        .parse()
        .unwrap_or_else(|_| panic!("the cfg names a port:\n{cfg}"))
}

/// The path the `.cfg` names: the one after the port.
fn path_named_by(cfg: &str) -> String {
    let marker = "http://127.0.0.1:";
    let start = cfg
        .find(marker)
        .unwrap_or_else(|| panic!("the cfg names an endpoint:\n{cfg}"))
        + marker.len();
    let rest = &cfg[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let path = &rest[end..];
    let quote = path
        .find('"')
        .unwrap_or_else(|| panic!("the path is quoted:\n{cfg}"));
    path[..quote].to_string()
}

#[test]
fn the_cfg_names_the_port_the_listener_bound_and_not_the_configured_one() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let configured = free_port();
    let sources = start_event_sources(dir.path(), &events(configured, true));

    let live = gsi_report(&sources)
        .gsi_port
        .expect("the listener reports the port it bound");
    assert_ne!(live, configured, "the OS picked a port of its own");
    assert_ne!(live, 0, "and it is a real one");

    for game in [CS2, DOTA2] {
        let text = cfg(dir.path(), game);
        assert_eq!(
            port_named_by(&text),
            live,
            "the {game} cfg must name the port the listener bound:\n{text}"
        );
        assert_ne!(
            port_named_by(&text),
            configured,
            "and not the configured one"
        );
    }
}

#[test]
fn the_two_games_carry_different_tokens_and_their_own_paths() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let sources = start_event_sources(dir.path(), &events(free_port(), true));
    let live = gsi_report(&sources)
        .gsi_port
        .expect("the listener reports the port it bound");

    let cs2 = cfg(dir.path(), CS2);
    let dota2 = cfg(dir.path(), DOTA2);

    // Each game posts to a path of its own: that is what scopes the token.
    assert_eq!(path_named_by(&cs2), "/gsi/cs2", "the cs2 cfg:\n{cs2}");
    assert_eq!(
        path_named_by(&dota2),
        "/gsi/dota2",
        "the dota2 cfg:\n{dota2}"
    );

    // One listener, so one port: both files name the same live port.
    assert_eq!(port_named_by(&cs2), live);
    assert_eq!(port_named_by(&dota2), live);

    // A token of its own for each game: the one lifted from the other game's file must not
    // be in this game's file.
    let cs2_token = token(dir.path(), CS2);
    let dota2_token = token(dir.path(), DOTA2);
    assert_ne!(cs2_token, dota2_token, "one token per game");
    assert!(
        cs2.contains(&cs2_token),
        "the cs2 cfg carries its own token"
    );
    assert!(
        dota2.contains(&dota2_token),
        "the dota2 cfg carries its own token"
    );
    assert!(
        !cs2.contains(&dota2_token),
        "the cs2 cfg must not carry dota's token"
    );
    assert!(
        !dota2.contains(&cs2_token),
        "the dota2 cfg must not carry cs2's token"
    );
}

#[test]
fn the_ephemeral_port_binds_a_port_that_is_not_the_configured_one() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let configured = free_port();
    let sources = start_event_sources(dir.path(), &events(configured, true));

    let live = gsi_report(&sources)
        .gsi_port
        .expect("the listener reports the port it bound");
    assert_ne!(live, configured, "the configured port is not used");
    assert_ne!(live, 0, "the OS picked a real one");
    assert_eq!(
        port_named_by(&cfg(dir.path(), CS2)),
        live,
        "and the cfg names it"
    );
}

#[test]
fn the_default_binds_the_configured_port() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let configured = free_port();
    let sources = start_event_sources(dir.path(), &events(configured, false));

    let live = gsi_report(&sources)
        .gsi_port
        .expect("the listener reports the port it bound");
    assert_eq!(live, configured, "the configured port is bound as it was");
    assert_eq!(port_named_by(&cfg(dir.path(), CS2)), configured);
    assert_eq!(port_named_by(&cfg(dir.path(), DOTA2)), configured);
}

#[test]
fn a_config_without_the_ephemeral_key_still_parses() {
    // A file written before the key existed must keep working: the key defaults to off, so
    // the configured port is bound as it always was.
    let events: EventsSection =
        toml::from_str("lol_poll_enabled = false\ngsi_port = 45671\n").expect("an old file parses");
    assert!(!events.gsi_ephemeral_port, "absent means off");
    assert_eq!(events.gsi_port, 45671, "and the port is untouched");

    let on: EventsSection =
        toml::from_str("lol_poll_enabled = false\ngsi_port = 45671\ngsi_ephemeral_port = true\n")
            .expect("the key parses");
    assert!(on.gsi_ephemeral_port, "and it can be turned on");
}

#[test]
fn the_source_report_carries_the_live_port() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let configured = free_port();
    let sources = start_event_sources(dir.path(), &events(configured, true));

    let report = gsi_report(&sources);
    assert_eq!(report.state, SourceState::Running);
    let live = report.gsi_port.expect("the live port is reported");
    assert_ne!(live, configured, "the live port, not the configured one");
    assert!(
        report.detail.contains(&live.to_string()),
        "and the detail agrees: {}",
        report.detail
    );

    // The games are named, with their paths and app ids.
    let games = &report.gsi_games;
    assert_eq!(games.len(), 2, "both games are reported: {games:?}");
    let cs2 = games
        .iter()
        .find(|game| game.game == CS2)
        .expect("cs2 is reported");
    assert_eq!(cs2.path, "/gsi/cs2");
    assert_eq!(cs2.app_id, CS2_APP_ID);
    let dota2 = games
        .iter()
        .find(|game| game.game == DOTA2)
        .expect("dota2 is reported");
    assert_eq!(dota2.path, "/gsi/dota2");
    assert_eq!(dota2.app_id, DOTA2_APP_ID);
}

#[test]
fn the_bindings_are_empty_when_the_listener_is_off() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let sources = start_event_sources(dir.path(), &events(0, false));

    assert!(
        sources.gsi_bindings().is_empty(),
        "there is nothing serving, so there is nothing to install a cfg for"
    );
    assert_eq!(gsi_report(&sources).state, SourceState::Disabled);
    assert!(gsi_report(&sources).gsi_port.is_none());
    assert!(!dir.path().join("gsi").exists(), "and no file was written");
}

#[test]
fn the_bindings_carry_the_live_port_the_path_the_app_id_and_a_token() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let configured = free_port();
    let sources = start_event_sources(dir.path(), &events(configured, true));

    let bindings = sources.gsi_bindings();
    assert_eq!(bindings.len(), 2, "one entry per game: {bindings:?}");

    let live = gsi_report(&sources)
        .gsi_port
        .expect("the listener reports the port it bound");
    for binding in &bindings {
        assert_eq!(binding.port, live, "the live port");
        assert!(!binding.token.is_empty(), "the token is there");
        assert_eq!(binding.token, token(dir.path(), &binding.game));
    }

    let cs2 = bindings.iter().find(|b| b.game == CS2).expect("cs2");
    assert_eq!(cs2.path, "/gsi/cs2");
    assert_eq!(cs2.app_id, CS2_APP_ID);
    let dota2 = bindings.iter().find(|b| b.game == DOTA2).expect("dota2");
    assert_eq!(dota2.path, "/gsi/dota2");
    assert_eq!(dota2.app_id, DOTA2_APP_ID);
}

#[test]
fn a_process_that_holds_the_port_leaves_no_cfg_behind() {
    // The port is taken by another process before the recorder starts: that process would
    // receive the game's POSTs, and with them the token. So the listener cannot start, and
    // no file may be written — a cfg naming that port would point the game straight at the
    // other process.
    let dir = tempfile::tempdir().expect("a temp dir");
    let squatter = TcpListener::bind("127.0.0.1:0").expect("the squatter's socket");
    let port = squatter.local_addr().expect("its address").port();

    let sources = start_event_sources(dir.path(), &events(port, false));

    assert_eq!(gsi_report(&sources).state, SourceState::Failed);
    assert!(gsi_report(&sources).gsi_port.is_none());
    assert!(
        sources.gsi_bindings().is_empty(),
        "nothing is serving, so there is nothing to install"
    );
    for game in [CS2, DOTA2] {
        assert!(
            !dir.path()
                .join("gsi")
                .join(game)
                .join(GSI_CFG_NAME)
                .exists(),
            "no cfg was written for {game}: the file would name a port the other process holds"
        );
    }
}

#[test]
fn the_binding_never_prints_its_token() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let sources = start_event_sources(dir.path(), &events(free_port(), true));
    let binding = sources
        .gsi_bindings()
        .into_iter()
        .next()
        .expect("a binding");
    let printed = format!("{binding:?}");
    assert!(
        !printed.contains(&binding.token),
        "the token must not be printed: {printed}"
    );
    assert!(printed.contains("<redacted>"), "and it says so: {printed}");
}

#[test]
fn the_binding_renders_the_cfg_the_game_must_be_given() {
    // The desktop shell writes the game's own file from the binding: this is the text it
    // writes, and it must name the live port and carry the token in force.
    let dir = tempfile::tempdir().expect("a temp dir");
    let sources = start_event_sources(dir.path(), &events(free_port(), true));
    let live = gsi_report(&sources)
        .gsi_port
        .expect("the listener reports the port it bound");

    for binding in sources.gsi_bindings() {
        let rendered = binding.cfg();
        assert_eq!(port_named_by(&rendered), live, "the live port");
        assert_eq!(
            path_named_by(&rendered),
            binding.path,
            "the game's own path"
        );
        assert!(rendered.contains(&binding.token), "and the token in force");
        assert_eq!(
            rendered,
            cfg(dir.path(), &binding.game),
            "and it is the file this crate wrote"
        );
    }
}

/// The old single `gsi-token.txt` used to be copied into each game's own file, so both games ended up
/// holding the same token: the thing per-game tokens exist to prevent. It is no longer carried over.
#[test]
fn a_legacy_shared_token_is_not_handed_to_both_games() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let legacy = replayed_events::gsi::load_or_create_token(&dir.path().join("gsi-token.txt"))
        .expect("the legacy token");
    let sources = start_event_sources(dir.path(), &events(free_port(), true));
    assert_eq!(
        gsi_report(&sources).state,
        SourceState::Running,
        "the install still starts"
    );
    let cs2 = token(dir.path(), CS2);
    let dota2 = token(dir.path(), DOTA2);
    assert_ne!(cs2, dota2, "each game has a token of its own");
    assert_ne!(cs2, legacy, "the shared token is not reused for cs2");
    assert_ne!(dota2, legacy, "nor for dota2");
    assert!(
        dir.path().join("gsi-token.txt").is_file(),
        "the old file is left where it was"
    );
}

#[cfg(unix)]
#[test]
fn the_token_files_are_owner_only_even_when_a_legacy_file_exists() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("a temp dir");
    // A legacy file is no longer copied, so the new files get the mode the token writer gives them.
    replayed_events::gsi::load_or_create_token(&dir.path().join("gsi-token.txt"))
        .expect("the legacy token");
    start_event_sources(dir.path(), &events(free_port(), true));
    for game in [CS2, DOTA2] {
        let path = dir.path().join(format!("gsi-token-{game}.txt"));
        let mode = std::fs::metadata(&path)
            .expect("the token file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the token file is owner-only");
    }
}

#[test]
fn the_token_files_are_the_ones_the_games_get() {
    // The desktop shell writes the game's own cfg from the binding, so the token it needs is
    // the one in force. These are the files that hold it.
    let dir = tempfile::tempdir().expect("a temp dir");
    start_event_sources(dir.path(), &events(free_port(), true));

    for game in [CS2, DOTA2] {
        let path: PathBuf = dir.path().join(format!("gsi-token-{game}.txt"));
        assert!(path.is_file(), "{} is there", path.display());
        let stored = token(dir.path(), game);
        assert_eq!(stored.len(), 64, "a token is 256 bits of hex");
        assert!(stored.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
