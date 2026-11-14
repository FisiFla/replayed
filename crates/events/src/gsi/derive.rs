//! GSI document to events, as a pure state machine.
//!
//! GSI is a snapshot service, not an event stream: the game POSTs the current state on change
//! and on a heartbeat, so what happened must be drawn from the difference between two
//! snapshots. This is one function of previous knowledge plus document, with no socket, clock
//! or log.
//!
//! | what changed | kind |
//! |---|---|
//! | nothing was in a game, now there is | `GameStart` |
//! | a game was in progress, now there is not (`map.phase == "gameover"`, Dota 2's `POST_GAME`, or an empty map) | `GameEnd` |
//! | `player.match_stats.kills` (Dota 2: `player.kills`) went up, for the local player, outside warmup | `Kill` |
//! | `deaths` went up | `Death` |
//! | `assists` went up | `Assist` |
//! | `map.round` went up | `RoundStart` |
//! | `round.phase` became `"over"` | `RoundEnd` |
//! | the bomb state became `"planted"` / `"defused"` | `BombPlanted` / `BombDefused` |
//! | anything else | nothing |
//!
//! Five rules make it survive real payloads:
//!
//! * A delta, not a count. Scoreboards are totals, so the tracker keeps a baseline, and it
//!   must be the same player's: `steamid` is compared, and a payload for a different player (a
//!   spectator switching view) re-baselines silently instead of reporting their kills as a jump.
//! * The local player, not whoever is on screen. Counter-Strike's `player` block describes the
//!   player being *observed*, which is the user only while alive; once dead the game follows a
//!   teammate and that teammate's totals arrive in the same block. `provider.steamid` is the
//!   client's own id, so a Counter-Strike scoreboard only counts when `player.steamid` is it, and
//!   the baseline moves silently otherwise. When the document carries no `player.steamid` at all
//!   (a `.cfg` installed before `player_id` was subscribed) the player is taken to be the local
//!   one, because the only alternative is to drop every kill; that is the one case where
//!   spectating cannot be told apart, which is why the installed file should be refreshed (see
//!   [`crate::gsi::integration`]).
//! * Warmup is not a match. Counter-Strike's `map.phase == "warmup"` is practice against bots and
//!   free respawns; its kills are neither worth a clip nor part of the match scoreboard, so the
//!   baseline follows them without reporting.
//! * A jump is one event. Two kills inside one POST window are one `Kill` with `"delta": 2`;
//!   two clips for one payload would invent events never separately observed.
//! * Transitions, not values. Round and bomb events fire on the edge, so the heartbeat that
//!   repeats the state emits nothing (else a clip every ten seconds).
//!
//! Attach policy is the League tracker's (see [`crate::lol::derive`]): the first document of a
//! game reports `GameStart` and adopts the scoreboard, round and bomb state as the baseline, so
//! a payload arriving mid-match does not produce a burst of clips for the match so far.

use crate::gsi::payload::{GsiSnapshot, Player};
use crate::{EventKind, GameEvent, Source};
use serde_json::{json, Map, Value};

/// Which game a document came from. The payloads overlap, and the derivation differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Game {
    /// Counter-Strike 2 / CS:GO (`provider.appid` 730).
    Cs2,
    /// Dota 2 (`provider.appid` 570).
    Dota2,
}

impl Game {
    /// The game a document reports, from its app id (the one field Valve guarantees), falling back
    /// to the provider's name for a client that disagrees.
    pub fn of(snapshot: &GsiSnapshot) -> Option<Game> {
        match snapshot.provider.appid {
            730 => Some(Game::Cs2),
            570 => Some(Game::Dota2),
            _ => {
                let name = snapshot.provider.name.to_ascii_lowercase();
                if name.contains("dota") {
                    Some(Game::Dota2)
                } else if name.contains("counter-strike") || name.contains("csgo") {
                    Some(Game::Cs2)
                } else {
                    None
                }
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Game::Cs2 => "cs2",
            Game::Dota2 => "dota2",
        }
    }
}

/// The scoreboard, round and bomb state a document describes: the "previous" side of the next
/// diff.
#[derive(Debug, Clone, Default, PartialEq)]
struct Baseline {
    game: Option<Game>,
    steamid: Option<String>,
    kills: Option<u32>,
    deaths: Option<u32>,
    assists: Option<u32>,
    round: u32,
    round_phase: String,
    bomb: String,
    map: String,
}

impl Baseline {
    fn of(snapshot: &GsiSnapshot, game: Game) -> Self {
        let player = snapshot.player.first();
        Self {
            game: Some(game),
            steamid: watched_steamid(snapshot, game),
            kills: player.and_then(Player::kills),
            deaths: player.and_then(Player::deaths),
            assists: player.and_then(Player::assists),
            round: snapshot.map.round,
            round_phase: snapshot.round.phase.clone(),
            bomb: bomb_state(snapshot),
            map: snapshot.map.name.clone(),
        }
    }

    /// Whether `snapshot` describes the same player as this baseline, so a scoreboard delta is
    /// meaningful. A document naming no steam id cannot be compared and is treated as a different
    /// player, not as a zero.
    fn same_player(&self, snapshot: &GsiSnapshot, game: Game) -> bool {
        match (
            self.steamid.as_deref(),
            watched_steamid(snapshot, game).as_deref(),
        ) {
            (Some(previous), Some(current)) => previous == current,
            _ => false,
        }
    }
}

fn player_steamid(player: &Player) -> Option<String> {
    player.steamid.clone().filter(|id| !id.is_empty())
}

/// The client's own steam id (`provider.steamid`): the local user, whatever the game is showing.
fn local_steamid(snapshot: &GsiSnapshot) -> Option<String> {
    snapshot
        .provider
        .steamid
        .clone()
        .filter(|id| !id.is_empty())
}

/// Whose scoreboard the `player` block carries, for comparing one document with the next.
///
/// The block's own `steamid` when it has one. Counter-Strike omits it unless `player_id` is
/// subscribed, and a `.cfg` installed before that block was added still does, so for Counter-Strike
/// the client's id stands in: with nobody else named, the local player is the best attribution and
/// the alternative is to drop every kill. Dota 2 has no such stand-in: its `player` block names
/// the player, and an observer's provider id is not any of the listed players.
fn watched_steamid(snapshot: &GsiSnapshot, game: Game) -> Option<String> {
    let named = snapshot.player.first().and_then(player_steamid);
    match game {
        Game::Cs2 => named.or_else(|| local_steamid(snapshot)),
        Game::Dota2 => named,
    }
}

/// Whether the scoreboard in `snapshot` is the local user's, as opposed to a player they are
/// spectating (Counter-Strike shows a teammate once the user is dead). A document with no client
/// id cannot be checked and is left to the same-player comparison.
fn is_local_player(snapshot: &GsiSnapshot, game: Game) -> bool {
    if game != Game::Cs2 {
        return true;
    }
    match (local_steamid(snapshot), watched_steamid(snapshot, game)) {
        (Some(local), Some(watched)) => local == watched,
        _ => true,
    }
}

/// The bomb state, preferring the detailed root `bomb` block and falling back to `round.bomb`
/// (the same thing at different resolutions).
fn bomb_state(snapshot: &GsiSnapshot) -> String {
    if snapshot.bomb.state.is_empty() {
        snapshot.round.bomb.clone()
    } else {
        snapshot.bomb.state.clone()
    }
}

/// The state machine.
#[derive(Debug, Default)]
pub struct Tracker {
    in_game: bool,
    baseline: Baseline,
    /// How many documents have been folded in; only makes tests explicit about attach versus
    /// steady state.
    observed: u64,
}

impl Tracker {
    /// Observe one POST body and return what it implies.
    ///
    /// A document that cannot be attributed to a game (no provider, an unknown app id) yields
    /// nothing and changes nothing: it is not evidence a game ended, and guessing is worse than
    /// waiting.
    pub fn observe(&mut self, snapshot: &GsiSnapshot) -> Vec<GameEvent> {
        let Some(game) = Game::of(snapshot) else {
            return Vec::new();
        };
        self.observed += 1;

        if !in_game(snapshot, game) {
            return self.leave_game(snapshot, game);
        }
        if !self.in_game || self.baseline.game != Some(game) {
            return self.enter_game(snapshot, game);
        }

        let mut out = Vec::new();
        out.extend(scoreboard_events(&self.baseline, snapshot, game));
        if game == Game::Cs2 {
            out.extend(round_events(&self.baseline, snapshot));
        }
        self.baseline = Baseline::of(snapshot, game);
        out
    }

    /// Whether a game is in progress, as of the last observation.
    pub fn in_game(&self) -> bool {
        self.in_game
    }

    /// How many documents have been folded in.
    pub fn observed(&self) -> u64 {
        self.observed
    }

    fn enter_game(&mut self, snapshot: &GsiSnapshot, game: Game) -> Vec<GameEvent> {
        self.in_game = true;
        self.baseline = Baseline::of(snapshot, game);
        vec![GameEvent::new(
            Source::Gsi,
            EventKind::GameStart,
            game_event_json(game, "GameStart")
                .with("map", json!(snapshot.map.name))
                .with("mode", json!(snapshot.map.mode))
                .with("phase", json!(map_phase(snapshot, game)))
                .with("adopted_scoreboard", json!(self.baseline.scoreboard_json()))
                .value(),
        )]
    }

    fn leave_game(&mut self, snapshot: &GsiSnapshot, game: Game) -> Vec<GameEvent> {
        if !self.in_game {
            return Vec::new();
        }
        let previous = std::mem::take(&mut self.baseline);
        self.in_game = false;
        vec![GameEvent::new(
            Source::Gsi,
            EventKind::GameEnd,
            game_event_json(game, "GameEnd")
                .with("map", json!(previous.map))
                .with("final_state", json!(map_phase(snapshot, game)))
                .with("final_scoreboard", json!(previous.scoreboard_json()))
                .value(),
        )]
    }
}

/// Whether the document describes a game in progress, per game.
fn in_game(snapshot: &GsiSnapshot, game: Game) -> bool {
    match game {
        // Counter-Strike keeps the map name set all session, so the phase separates a running match
        // from the scoreboard after one.
        Game::Cs2 => !snapshot.map.name.trim().is_empty() && snapshot.map.phase != "gameover",
        // Dota 2 reports a rules state; the states before and after a match are named.
        Game::Dota2 => {
            let state = snapshot.map.game_state.as_str();
            !state.is_empty()
                && state != "DOTA_GAMERULES_STATE_INIT"
                && state != "DOTA_GAMERULES_STATE_INVALID"
                && state != "DOTA_GAMERULES_STATE_POST_GAME"
        }
    }
}

/// The phase a payload reports, whichever game it is from.
fn map_phase(snapshot: &GsiSnapshot, game: Game) -> String {
    match game {
        Game::Cs2 => snapshot.map.phase.clone(),
        Game::Dota2 => snapshot.map.game_state.clone(),
    }
}

/// Kill / death / assist, from the scoreboard deltas.
fn scoreboard_events(baseline: &Baseline, snapshot: &GsiSnapshot, game: Game) -> Vec<GameEvent> {
    let Some(player) = snapshot.player.first() else {
        return Vec::new();
    };
    if !baseline.same_player(snapshot, game) {
        // A different player's scoreboard (or no steam id): nothing can be attributed to the watched
        // player, so nothing is reported; the baseline moves on in `observe`.
        return Vec::new();
    }
    if !is_local_player(snapshot, game) {
        // The user is dead and the game is showing a teammate. Their kills are not the user's.
        return Vec::new();
    }
    if game == Game::Cs2 && snapshot.map.phase == "warmup" {
        // Practice kills against bots: the baseline follows them, nothing is reported.
        return Vec::new();
    }

    let mut out = Vec::new();
    for (kind, previous, current, label) in [
        (EventKind::Kill, baseline.kills, player.kills(), "kills"),
        (EventKind::Death, baseline.deaths, player.deaths(), "deaths"),
        (
            EventKind::Assist,
            baseline.assists,
            player.assists(),
            "assists",
        ),
    ] {
        let (Some(previous), Some(current)) = (previous, current) else {
            continue;
        };
        if current <= previous {
            continue;
        }
        out.push(GameEvent::new(
            Source::Gsi,
            kind,
            game_event_json(game, kind.as_tag())
                .with("delta", json!(current - previous))
                .with("total", json!(current))
                .with("stat", json!(label))
                .with("round", json!(snapshot.map.round))
                .with("player", json!(player.name))
                .value(),
        ));
    }
    out
}

/// Round start and round end (Counter-Strike only; Dota 2 has no round state).
fn round_events(baseline: &Baseline, snapshot: &GsiSnapshot) -> Vec<GameEvent> {
    let mut out = Vec::new();
    if snapshot.map.round > baseline.round {
        out.push(GameEvent::new(
            Source::Gsi,
            EventKind::RoundStart,
            json!({
                "source": "gsi",
                "game": "cs2",
                "event": "round_start",
                "round": snapshot.map.round,
                "score": { "ct": snapshot.map.team_ct.score, "t": snapshot.map.team_t.score },
            }),
        ));
    }
    if snapshot.round.phase == "over" && baseline.round_phase != "over" {
        out.push(GameEvent::new(
            Source::Gsi,
            EventKind::RoundEnd,
            json!({
                "source": "gsi",
                "game": "cs2",
                "event": "round_end",
                "round": snapshot.map.round,
                "win_team": snapshot.round.win_team.clone(),
                "score": { "ct": snapshot.map.team_ct.score, "t": snapshot.map.team_t.score },
            }),
        ));
    }

    // The bomb, on the edge only: carried, planting, planted, (defusing), defused/exploded, each
    // a POST. Only the two moments worth a clip are reported; "exploded" is the round ending
    // (`RoundEnd`).
    let bomb = bomb_state(snapshot);
    if bomb != baseline.bomb {
        match bomb.as_str() {
            "planted" => out.push(GameEvent::new(
                Source::Gsi,
                EventKind::BombPlanted,
                json!({
                    "source": "gsi",
                    "game": "cs2",
                    "event": "bomb_planted",
                    "round": snapshot.map.round,
                    "bomb_state": bomb,
                    "countdown": snapshot.bomb.countdown.clone(),
                    "player": snapshot.bomb.player.clone(),
                }),
            )),
            "defused" => out.push(GameEvent::new(
                Source::Gsi,
                EventKind::BombDefused,
                json!({
                    "source": "gsi",
                    "game": "cs2",
                    "event": "bomb_defused",
                    "round": snapshot.map.round,
                    "bomb_state": bomb,
                    "player": snapshot.bomb.player.clone(),
                }),
            )),
            _ => {}
        }
    }
    out
}

/// A small builder so payloads read as a fixed envelope plus whatever this event knows.
struct Envelope(Value);

impl Envelope {
    fn with(mut self, key: &str, value: Value) -> Self {
        if let Value::Object(map) = &mut self.0 {
            map.insert(key.to_string(), value);
        }
        self
    }

    fn value(self) -> Value {
        self.0
    }
}

fn game_event_json(game: Game, event: &str) -> Envelope {
    Envelope(json!({ "source": "gsi", "game": game.as_str(), "event": event }))
}

impl Baseline {
    fn scoreboard_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("steamid".into(), json!(self.steamid));
        map.insert("kills".into(), json!(self.kills));
        map.insert("deaths".into(), json!(self.deaths));
        map.insert("assists".into(), json!(self.assists));
        map.insert("round".into(), json!(self.round));
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gsi::payload::parse;

    const CS2_COMPETITIVE: &str = include_str!("../../tests/fixtures/gsi_cs2_competitive.json");
    /// What a `.cfg` installed before `player_id` was subscribed produces: `player` has `state` and
    /// `match_stats` but no `steamid` or `name`, and `auth` is the last key as in a real payload.
    const CS2_NO_PLAYER_ID: &str = include_str!("../../tests/fixtures/gsi_cs2_no_player_id.json");
    const CS2_MENU: &str = include_str!("../../tests/fixtures/gsi_cs2_menu.json");
    const DOTA_IN_GAME: &str = include_str!("../../tests/fixtures/gsi_dota_in_game.json");
    const DOTA_OBSERVER: &str = include_str!("../../tests/fixtures/gsi_dota_observer.json");

    fn document(fixture: &str) -> GsiSnapshot {
        parse(fixture.as_bytes()).expect("the fixture parses")
    }

    /// The competitive fixture as a Rust value the tests can edit field by field.
    fn cs2() -> GsiSnapshot {
        document(CS2_COMPETITIVE)
    }

    /// The single player a Counter-Strike fixture describes, for editing one field at a time
    /// instead of a hundred near-copies of the JSON.
    fn player_of(snapshot: &mut GsiSnapshot) -> &mut Player {
        snapshot
            .player
            .first_mut()
            .expect("the fixture names one player")
    }

    fn kinds(events: &[GameEvent]) -> Vec<EventKind> {
        events.iter().map(|e| e.kind).collect()
    }

    fn payload(events: &[GameEvent], kind: EventKind) -> &str {
        events
            .iter()
            .find(|e| e.kind == kind)
            .and_then(|e| e.payload.as_deref())
            .unwrap_or_else(|| panic!("no {kind} in {events:?}"))
    }

    #[test]
    fn the_game_is_recognised_from_the_provider_block() {
        assert_eq!(Game::of(&cs2()), Some(Game::Cs2));
        assert_eq!(Game::of(&document(DOTA_IN_GAME)), Some(Game::Dota2));
        // A client that reports no app id is recognised by name; one that matches neither
        // is not attributed to a game at all.
        let mut snapshot = cs2();
        snapshot.provider.appid = 0;
        assert_eq!(Game::of(&snapshot), Some(Game::Cs2));
        snapshot.provider.name = "Something Else".into();
        assert_eq!(Game::of(&snapshot), None);
    }

    #[test]
    fn attaching_mid_match_reports_the_attach_and_adopts_the_scoreboard() {
        // Counter-Strike attach: 12 kills and 5 deaths are already on the board mid-round; none of it
        // is a clip.
        let mut tracker = Tracker::default();
        let start = tracker.observe(&cs2());

        assert_eq!(
            kinds(&start),
            vec![EventKind::GameStart],
            "one event, not one per stat"
        );
        assert!(tracker.in_game());
        let body = payload(&start, EventKind::GameStart);
        assert!(body.contains("\"game\":\"cs2\""), "got {body}");
        assert!(body.contains("\"map\":\"de_dust2\""), "got {body}");
        assert!(
            body.contains("\"kills\":12"),
            "the adopted baseline is recorded, so a reader can see why nothing was reported: {body}"
        );
    }

    #[test]
    fn a_repeated_document_emits_nothing() {
        // GSI repeats the state on a heartbeat. Every repeat must be a no-op or a match would write a
        // clip every ten seconds.
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());
        for _ in 0..5 {
            assert!(
                tracker.observe(&cs2()).is_empty(),
                "a heartbeat is not an event"
            );
        }
    }

    #[test]
    fn a_kill_and_a_death_and_an_assist_are_each_reported_once() {
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());

        let mut next = cs2();
        player_of(&mut next).match_stats.kills = Some(13);
        player_of(&mut next).match_stats.deaths = Some(6);
        player_of(&mut next).match_stats.assists = Some(4);
        let events = tracker.observe(&next);
        assert_eq!(
            kinds(&events),
            vec![EventKind::Kill, EventKind::Death, EventKind::Assist],
            "three stats, three reports"
        );
        assert!(payload(&events, EventKind::Kill).contains("\"delta\":1"));
        assert!(payload(&events, EventKind::Kill).contains("\"total\":13"));

        // The same document again: nothing.
        assert!(tracker.observe(&next).is_empty());
    }

    #[test]
    fn two_kills_inside_one_payload_window_are_one_event_with_a_delta() {
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());

        let mut next = cs2();
        player_of(&mut next).match_stats.kills = Some(14);
        let events = tracker.observe(&next);
        assert_eq!(kinds(&events), vec![EventKind::Kill], "one clip, not two");
        let body = payload(&events, EventKind::Kill);
        assert!(
            body.contains("\"delta\":2"),
            "and the delta is not hidden: {body}"
        );
        assert!(body.contains("\"total\":14"), "got {body}");
    }

    #[test]
    fn a_scoreboard_that_belongs_to_another_player_is_not_a_burst_of_events() {
        // A spectator switching view replaces one player's totals with another's; reporting the
        // difference would report somebody else's kills.
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());

        let mut other = cs2();
        let switched = player_of(&mut other);
        switched.steamid = Some("76561198000000042".into());
        switched.match_stats.kills = Some(30);
        switched.match_stats.deaths = Some(1);
        switched.match_stats.assists = Some(0);
        assert!(
            tracker.observe(&other).is_empty(),
            "a different player's scoreboard is a re-baseline, not a set of events"
        );

        // And that player's next kill is not reported either: the provider's id is the user's, and
        // this is somebody else.
        player_of(&mut other).match_stats.kills = Some(31);
        assert!(tracker.observe(&other).is_empty());
    }

    #[test]
    fn a_payload_that_names_nobody_never_reports_a_scoreboard_delta() {
        // Neither `player.steamid` nor `provider.steamid`: there is no one to attribute a delta to.
        let mut anonymous = cs2();
        player_of(&mut anonymous).steamid = None;
        anonymous.provider.steamid = None;
        let mut tracker = Tracker::default();
        tracker.observe(&anonymous);

        player_of(&mut anonymous).match_stats.kills = Some(99);
        assert!(
            tracker.observe(&anonymous).is_empty(),
            "there is no player to attribute a delta to"
        );
    }

    #[test]
    fn a_payload_without_player_id_is_attributed_to_the_client_in_provider() {
        // A `.cfg` installed before `player_id` was subscribed: `player` has stats but no steam id.
        // Dropping every kill would make the integration look broken, so the provider's id (the
        // client's own) stands in for the player.
        let before = document(CS2_NO_PLAYER_ID);
        assert!(
            before
                .player
                .first()
                .expect("a player block")
                .steamid
                .is_none(),
            "the fixture is the case under test"
        );
        let mut tracker = Tracker::default();
        tracker.observe(&before);

        let mut after = before.clone();
        player_of(&mut after).match_stats.kills = Some(13);
        assert_eq!(kinds(&tracker.observe(&after)), vec![EventKind::Kill]);
    }

    #[test]
    fn a_teammate_spectated_after_dying_is_not_the_users_kill_death_or_assist() {
        // Alive, the `player` block is the user. Dead, the game follows a teammate, and the block now
        // carries that teammate's totals under their own steam id while `provider.steamid` stays the
        // user's. None of it is the user's, and neither is the first document after respawn.
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());

        let mut spectating = cs2();
        let teammate = player_of(&mut spectating);
        teammate.steamid = Some("76561198000000042".into());
        teammate.match_stats.kills = Some(7);
        teammate.match_stats.deaths = Some(2);
        teammate.match_stats.assists = Some(3);
        assert!(tracker.observe(&spectating).is_empty());

        // The teammate gets a kill while the user watches: still not the user's.
        player_of(&mut spectating).match_stats.kills = Some(8);
        assert!(
            tracker.observe(&spectating).is_empty(),
            "the spectated player's kill is not the user's"
        );

        // Back to the user (next round): their own totals were never lost, so the move back is a
        // re-baseline and not a jump.
        assert!(tracker.observe(&cs2()).is_empty());
        let mut again = cs2();
        player_of(&mut again).match_stats.kills = Some(13);
        assert_eq!(kinds(&tracker.observe(&again)), vec![EventKind::Kill]);
    }

    #[test]
    fn warmup_kills_are_not_highlights_and_the_match_that_follows_starts_clean() {
        let mut warm = cs2();
        warm.map.phase = "warmup".into();
        warm.map.round = 0;
        let mut tracker = Tracker::default();
        tracker.observe(&warm);

        player_of(&mut warm).match_stats.kills = Some(15);
        player_of(&mut warm).match_stats.deaths = Some(9);
        player_of(&mut warm).match_stats.assists = Some(5);
        assert!(
            tracker.observe(&warm).is_empty(),
            "bot kills in warmup are not a highlight, and neither are the deaths or assists"
        );

        // The match starts, scores reset (below the warmup totals, so no delta) and kills count.
        let mut live = cs2();
        live.map.round = warm.map.round;
        player_of(&mut live).match_stats.kills = Some(0);
        player_of(&mut live).match_stats.deaths = Some(0);
        player_of(&mut live).match_stats.assists = Some(0);
        assert!(tracker.observe(&live).is_empty());
        player_of(&mut live).match_stats.kills = Some(1);
        assert_eq!(kinds(&tracker.observe(&live)), vec![EventKind::Kill]);
    }

    #[test]
    fn a_new_round_and_its_end_are_reported_on_the_edge() {
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());

        // Round 5 ends.
        let mut over = cs2();
        over.round.phase = "over".into();
        over.round.win_team = Some("CT".into());
        let ended = tracker.observe(&over);
        assert_eq!(kinds(&ended), vec![EventKind::RoundEnd]);
        assert!(payload(&ended, EventKind::RoundEnd).contains("\"win_team\":\"CT\""));

        // The same "over" state again (the heartbeat during the end-of-round delay) is not a second
        // round end.
        assert!(tracker.observe(&over).is_empty());

        // Round 6 begins.
        let mut next = cs2();
        next.map.round = 6;
        next.round.phase = "freezetime".into();
        next.map.team_ct.score = 4;
        let started = tracker.observe(&next);
        assert_eq!(kinds(&started), vec![EventKind::RoundStart]);
        let body = payload(&started, EventKind::RoundStart);
        assert!(body.contains("\"round\":6"), "got {body}");
        assert!(body.contains("\"ct\":4"), "got {body}");
    }

    #[test]
    fn the_bomb_is_reported_when_it_is_planted_and_when_it_is_defused() {
        let mut tracker = Tracker::default();
        // Attach on a round where the bomb is still carried.
        let mut carry = cs2();
        carry.bomb.state = "carried".into();
        carry.round.bomb = String::new();
        tracker.observe(&carry);

        let mut planted = carry.clone();
        planted.bomb.state = "planting".into();
        assert!(
            tracker.observe(&planted).is_empty(),
            "planting is not planted yet"
        );

        planted.bomb.state = "planted".into();
        let events = tracker.observe(&planted);
        assert_eq!(kinds(&events), vec![EventKind::BombPlanted]);
        assert!(payload(&events, EventKind::BombPlanted).contains("\"countdown\":\"38.5\""));

        assert!(
            tracker.observe(&planted).is_empty(),
            "the same plant twice is one plant"
        );

        let mut defused = planted.clone();
        defused.bomb.state = "defused".into();
        assert_eq!(
            kinds(&tracker.observe(&defused)),
            vec![EventKind::BombDefused]
        );
    }

    #[test]
    fn the_round_level_bomb_state_is_used_when_the_detailed_block_is_absent() {
        // A user who subscribes `round` but not `bomb` still gets the plant.
        let mut tracker = Tracker::default();
        let mut carry = cs2();
        carry.bomb.state = String::new();
        carry.round.bomb = "carried".into();
        tracker.observe(&carry);

        let mut planted = carry.clone();
        planted.round.bomb = "planted".into();
        assert_eq!(
            kinds(&tracker.observe(&planted)),
            vec![EventKind::BombPlanted]
        );
    }

    #[test]
    fn a_match_that_ends_reports_one_game_end_and_then_nothing() {
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());

        let mut over = cs2();
        over.map.phase = "gameover".into();
        let ended = tracker.observe(&over);
        assert_eq!(kinds(&ended), vec![EventKind::GameEnd]);
        assert!(!tracker.in_game());
        let body = payload(&ended, EventKind::GameEnd);
        assert!(body.contains("\"final_scoreboard\""), "got {body}");
        assert!(body.contains("\"final_state\":\"gameover\""), "got {body}");

        // The scoreboard after the match, and the main menu, are both silence.
        assert!(tracker.observe(&over).is_empty());
        assert!(tracker.observe(&document(CS2_MENU)).is_empty());
    }

    #[test]
    fn a_second_match_on_the_same_client_is_a_new_attach() {
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());
        let mut over = cs2();
        over.map.phase = "gameover".into();
        tracker.observe(&over);

        // A new match: round 1, warmup, empty scoreboard.
        let mut fresh = cs2();
        fresh.map.round = 1;
        fresh.map.phase = "warmup".into();
        fresh.round.phase = "freezetime".into();
        fresh.bomb.state = "carried".into();
        fresh.map.team_ct.score = 0;
        fresh.map.team_t.score = 0;
        let fresh_player = player_of(&mut fresh);
        fresh_player.match_stats.kills = Some(0);
        fresh_player.match_stats.deaths = Some(0);
        fresh_player.match_stats.assists = Some(0);

        assert_eq!(kinds(&tracker.observe(&fresh)), vec![EventKind::GameStart]);
        assert!(tracker.in_game());
    }

    #[test]
    fn a_payload_that_names_no_game_is_ignored_rather_than_read_as_an_end() {
        // A document with no provider block (an unattributable client) must not end a running game:
        // the state is kept and the payload dropped.
        let mut tracker = Tracker::default();
        tracker.observe(&cs2());

        let mut unknown = cs2();
        unknown.provider.appid = 0;
        unknown.provider.name = "Nothing We Know".into();
        assert!(tracker.observe(&unknown).is_empty());
        assert!(
            tracker.in_game(),
            "an unattributable payload is not the end of a game"
        );

        // And the next real payload still works.
        let mut next = cs2();
        player_of(&mut next).match_stats.kills = Some(13);
        assert_eq!(kinds(&tracker.observe(&next)), vec![EventKind::Kill]);
    }

    #[test]
    fn a_dota_match_reports_kills_and_deaths_from_its_own_scoreboard() {
        let mut tracker = Tracker::default();
        let start = tracker.observe(&document(DOTA_IN_GAME));
        assert_eq!(kinds(&start), vec![EventKind::GameStart]);
        assert!(payload(&start, EventKind::GameStart).contains("\"game\":\"dota2\""));
        assert!(
            payload(&start, EventKind::GameStart)
                .contains("\"DOTA_GAMERULES_STATE_GAME_IN_PROGRESS\""),
            "the Dota phase is its rules state"
        );

        let mut next = document(DOTA_IN_GAME);
        match &mut next.player {
            crate::gsi::payload::PlayerBlock::One(player) => {
                player.kills = Some(5);
                player.deaths = Some(3);
            }
            other => panic!("the fixture is a single player block: {other:?}"),
        }
        let events = tracker.observe(&next);
        assert_eq!(kinds(&events), vec![EventKind::Kill, EventKind::Death]);
        assert!(
            payload(&events, EventKind::Kill).contains("\"round\":0"),
            "Dota has no rounds; the field is zero rather than invented"
        );

        // Dota's post-game state ends the match.
        let mut post = document(DOTA_IN_GAME);
        post.map.game_state = "DOTA_GAMERULES_STATE_POST_GAME".into();
        assert_eq!(kinds(&tracker.observe(&post)), vec![EventKind::GameEnd]);
        assert!(!tracker.in_game());

        // And the strategy time of the next match starts a new one.
        let mut next_match = document(DOTA_IN_GAME);
        next_match.map.game_state = "DOTA_GAMERULES_STATE_STRATEGY_TIME".into();
        assert_eq!(
            kinds(&tracker.observe(&next_match)),
            vec![EventKind::GameStart]
        );
    }

    #[test]
    fn an_observer_payload_whose_player_block_is_an_array_is_accepted() {
        // The first entry is used, and since the scoreboard is only compared across documents naming
        // the same steam id, an observer payload cannot burst events for whoever is first in the list.
        let mut tracker = Tracker::default();
        assert_eq!(
            kinds(&tracker.observe(&document(DOTA_OBSERVER))),
            vec![EventKind::GameStart]
        );
        let mut next = document(DOTA_OBSERVER);
        if let crate::gsi::payload::PlayerBlock::Many(players) = &mut next.player {
            players[0].kills = Some(5);
        }
        assert_eq!(kinds(&tracker.observe(&next)), vec![EventKind::Kill]);
    }

    #[test]
    fn the_main_menu_is_not_a_game_and_never_ends_one_twice() {
        let mut tracker = Tracker::default();
        // A client in the main menu: no game, no event, however many payloads arrive.
        for _ in 0..3 {
            assert!(tracker.observe(&document(CS2_MENU)).is_empty());
            assert!(!tracker.in_game());
        }
    }
}
