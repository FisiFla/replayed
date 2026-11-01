//! Snapshot to events, as a pure state machine.
//!
//! No socket, clock or log: previous knowledge plus one document yields events, so every rule
//! is a unit test. [`crate::lol::poller`] puts it on a network.
//!
//! The Live Client serves the whole event log so far, and a game is usually well under way
//! before replayed starts. Replaying that history would be a storm of clips of footage nobody
//! asked for, hence the attach policy:
//!
//! | transition | what is emitted |
//! |---|---|
//! | no game to game | `GameStart` only. Every event already in the document is adopted as seen (`adopted_in_progress_events` in the payload counts them), none emitted. |
//! | game to no game | `GameEnd`, once, on the falling edge; every later no-game poll is silent. |
//! | game to game | only events whose id has not been seen. |
//! | a fresh `GameStart` line appears mid-game | the client restarted without the endpoint going quiet: reset, adopt again, emit `GameStart` only. |
//!
//! Idempotence: events are identified by `EventID` (a monotonic per-game counter) and only
//! unseen ids are emitted, so polling the same document twice emits nothing. A document whose
//! events carry no id (the client has omitted it) falls back to a key derived from the event's
//! content, which keeps idempotence without inventing an ordering.
//!
//! | `EventName` | kind |
//! |---|---|
//! | `ChampionKill` | `Kill` / `Death` / `Assist`, by comparing `KillerName` / `VictimName` / `Assisters` against the watched player's names |
//! | `DragonKill`, `BaronKill`, `HeraldKill`, `TurretKilled` / `FirstBrick`, `InhibKilled` | `Dragon`, `Baron`, `Herald`, `Turret`, `Inhibitor` |
//! | `EpicMonsterKill` (by `MonsterType`) | the matching sub-kind, else `Objective` |
//! | `HordeKill`, `FirstBlood`, `Ace`, `Multikill` | `Objective`, only when the watched player is the one named (killer or assister, `Recipient`, `Acer`, killer) |
//! | `GameStart`, `GameEnd` | the state machine above, never the table |
//! | anything else (`MinionsSpawning`, a future name) | nothing |
//!
//! A `ChampionKill` involving nobody the document names as the watched player is not emitted
//! (a fight the user was not in is not a clip), and neither is a personal milestone
//! (`Multikill`, `FirstBlood`, `Ace`, `HordeKill`) somebody else earned: a clip records the
//! user's own screen, which shows nothing of an enemy's multikill.
//!
//! When the document names no player at all there is nobody to compare against, and nothing
//! personal is emitted. That is the shape of a spectated game or a replay (`activePlayer` is
//! empty), where treating every `ChampionKill` as the user's own turned all ten players' kills
//! into a clip each, and of the loading screen (no events yet). The cost is that a patch which
//! moved the identity field would silence personal events until this module learns the new
//! spelling; that shows in the `GameStart` payload (`"identity": false`, as of the attach; a loading
//! screen may not have named the player yet) rather than as a flood of clips of footage that
//! is not the user's. Game-level milestones (a baron, a
//! turret) do not depend on identity and are still emitted.
//!
//! A document that already carries its own `GameEnd` line describes a finished game. Whether
//! the tracker was in that game (it emits the end, once) or not (it never attached), the
//! client keeps serving that document for a few seconds, and re-reading it must not start a
//! game that is already over.

use crate::lol::payload::{GameSnapshot, StreamEvent};
use crate::{EventKind, GameEvent, Source};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// The state machine: what has been seen, and what is currently true. `Default` is "nothing
/// seen, no game", what a freshly started poller knows.
#[derive(Debug, Default)]
pub struct Tracker {
    in_game: bool,
    /// Ids of the events already accounted for. Cleared when the game changes, since the client
    /// numbers ids per game.
    seen: HashSet<EventKey>,
    /// The game clock at the last document that described a game, for the `GameEnd` payload's
    /// duration.
    last_game_time: f64,
    /// The scoreboard of the last document that described a game: the falling edge has no document
    /// of its own, so the end-of-game scoreboard is the last one seen alive.
    last_scoreboard: Value,
    /// How many in-progress events the last attach adopted without emitting; the poller logs it so
    /// a user can see why the first clip is not preceded by forty others.
    adopted_on_attach: u64,
    /// The map of the last attach, for the poller's log line, kept rather than read back out of the
    /// rendered payload.
    attached_map: String,
}

impl Tracker {
    /// Observe one poll and return what it implies.
    ///
    /// `observed` is `None` when the poll found no game (a 404 or connection refusal). A poll that
    /// failed (malformed body, TLS error) must not be passed as `None`: the caller keeps its state,
    /// which [`crate::lol::poller::LolPoller::poll_once`] decides, not this function.
    pub fn observe(&mut self, observed: Option<&GameSnapshot>) -> Vec<GameEvent> {
        let Some(snapshot) = observed.filter(|snapshot| snapshot.game_present()) else {
            return self.end_game();
        };

        if !self.in_game {
            // The client keeps serving a finished game's document (its own `GameEnd` line included)
            // for a few seconds after the end. `seen` was cleared with the game, so reading it again
            // as a fresh attach would invent a `GameStart`, and the 404 after it a second `GameEnd`.
            if has_game_end(snapshot) {
                return Vec::new();
            }
            return self.start_game(snapshot);
        }
        // The player quit and requeued faster than the endpoint stopped answering, so this is a
        // different game. Re-attach as if the endpoint had gone quiet, or the new game's early events
        // arrive as a burst of clips.
        if self.started_again(snapshot) {
            return self.start_game(snapshot);
        }

        self.last_scoreboard = snapshot.scoreboard();
        let names = snapshot.player_names();
        let mut out = Vec::new();
        for entry in snapshot.event_stream() {
            let key = EventKey::of(entry);
            if self.seen.contains(&key) {
                continue;
            }
            match entry.event_name.as_str() {
                // The game's own `GameStart` line, seen for the first time. This happens when the client
                // starts including an event history it was not sending before (the loading screen serves an
                // empty list), so it is the game already watched, not a new one. Adopted silently; a
                // different game is recognised by its clock, below.
                "GameStart" => {
                    self.seen.insert(key);
                }
                // The document sometimes carries the end of the game as an event. Treat it as the end so the
                // 404 that follows does not report it again.
                //
                // The lines before it in this document were derived above and are delivered ahead of the
                // end: the last poll of a game is where the Multikill, the Ace and the final kill are.
                "GameEnd" => {
                    self.in_game = false;
                    self.seen.clear();
                    self.last_game_time = 0.0;
                    out.push(end_event(
                        snapshot.game_time(),
                        snapshot.scoreboard(),
                        entry.result.clone(),
                    ));
                    return out;
                }
                name => {
                    self.seen.insert(key);
                    if let Some(kind) = classify(name, entry, &names) {
                        out.push(stream_event(name, entry, kind, snapshot));
                    }
                }
            }
        }
        self.last_game_time = snapshot.game_time();
        out
    }

    /// Whether this document belongs to a different game than the one tracked.
    ///
    /// `gameTime` counts up from a negative value during the loading screen, so a clock that jumped
    /// backwards means a new game started without the endpoint going quiet, leaving no `GameEnd` to
    /// see. Event ids restart per game and cannot be used (`EventID` 0 is `GameStart` in every
    /// game). A document omitting the clock is no evidence and never triggers this.
    fn started_again(&self, snapshot: &GameSnapshot) -> bool {
        /// Seconds of backwards movement that cannot be jitter within one game.
        const BACKWARDS_S: f64 = 5.0;
        let now = snapshot.game_time();
        now != 0.0 && self.last_game_time != 0.0 && now < self.last_game_time - BACKWARDS_S
    }

    /// Whether a game is being played, as of the last observation.
    pub fn in_game(&self) -> bool {
        self.in_game
    }

    /// How many in-progress events the last attach adopted without emitting.
    pub fn adopted_on_attach(&self) -> u64 {
        self.adopted_on_attach
    }

    /// The game mode and map of the last attach, for a driver's log line; empty before the first.
    pub fn attached_map(&self) -> &str {
        &self.attached_map
    }

    /// No game (or a new game): emit `GameStart` and take the document's history as already seen.
    fn start_game(&mut self, snapshot: &GameSnapshot) -> Vec<GameEvent> {
        self.in_game = true;
        self.seen.clear();
        self.adopted_on_attach = self.adopt(snapshot);
        self.last_game_time = snapshot.game_time();
        self.last_scoreboard = snapshot.scoreboard();
        self.attached_map = snapshot.game_data.map_name.trim().to_string();
        vec![start_event(snapshot, self.adopted_on_attach)]
    }

    /// The falling edge: a game was being played and the endpoint no longer serves one.
    fn end_game(&mut self) -> Vec<GameEvent> {
        if !self.in_game {
            return Vec::new();
        }
        self.in_game = false;
        self.seen.clear();
        self.adopted_on_attach = 0;
        self.attached_map.clear();
        let event = end_event(
            self.last_game_time,
            std::mem::take(&mut self.last_scoreboard),
            None,
        );
        self.last_game_time = 0.0;
        vec![event]
    }

    /// Mark every event in the document as seen, returning how many that was.
    fn adopt(&mut self, snapshot: &GameSnapshot) -> u64 {
        let mut adopted = 0;
        for entry in snapshot.event_stream() {
            // Deduplicate: an id-less document can repeat a line, and the count reported should be the
            // number of events adopted.
            if self.seen.insert(EventKey::of(entry)) {
                adopted += 1;
            }
        }
        adopted
    }
}

/// Whether the document carries the game's own `GameEnd` line.
fn has_game_end(snapshot: &GameSnapshot) -> bool {
    snapshot
        .event_stream()
        .iter()
        .any(|entry| entry.event_name == "GameEnd")
}

/// How an event is identified for the "have I seen this?" question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EventKey {
    /// The client's own `EventID`, which is monotonic within a game.
    Id(i64),
    /// A document with no id: a content hash, so the same line is not emitted twice.
    Content(u64),
}

impl EventKey {
    fn of(entry: &StreamEvent) -> Self {
        if entry.event_id != 0 {
            return EventKey::Id(entry.event_id);
        }
        // FNV-1a over the fields describing the event. Not a security hash: it only needs to be stable
        // and collision-resistant enough for a few hundred events.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(entry.event_name.as_bytes());
        feed(b"\x1f");
        // Time to a tenth of a second: the client's clock has more digits than needed, and two events
        // at the same tenth are the same event.
        feed(format!("{:.1}", entry.event_time).as_bytes());
        for name in [
            entry.killer_name.as_deref(),
            entry.victim_name.as_deref(),
            entry.monster_type.as_deref(),
            entry.monster_sub_type.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            feed(b"\x1f");
            feed(name.as_bytes());
        }
        for assister in &entry.assisters {
            feed(b"\x1f");
            feed(assister.as_bytes());
        }
        EventKey::Content(hash)
    }
}

/// Whether `candidate` is one of the names the document gives the watched player.
fn is_watched(names: &[&str], candidate: Option<&str>) -> bool {
    candidate.is_some_and(|c| names.contains(&c.trim()))
}

/// The documented `EventName` → [`EventKind`] mapping (see the module docs).
fn classify(name: &str, entry: &StreamEvent, names: &[&str]) -> Option<EventKind> {
    match name {
        // A champion kill is about the watched player, and only when the document names one: with
        // nobody to compare against (a spectated game, a replay) a kill is somebody's, not the
        // user's.
        "ChampionKill" => {
            if is_watched(names, entry.killer_name.as_deref()) {
                return Some(EventKind::Kill);
            }
            if is_watched(names, entry.victim_name.as_deref()) {
                return Some(EventKind::Death);
            }
            if entry
                .assisters
                .iter()
                .any(|a| is_watched(names, Some(a.as_str())))
            {
                return Some(EventKind::Assist);
            }
            // Somebody else's kill: not a clip. The hotkey exists for those moments.
            None
        }
        // Game-level milestones count whoever did them: a baron is a baron.
        "DragonKill" => Some(EventKind::Dragon),
        "BaronKill" => Some(EventKind::Baron),
        "HeraldKill" => Some(EventKind::Herald),
        "TurretKilled" | "FirstBrick" => Some(EventKind::Turret),
        "InhibKilled" => Some(EventKind::Inhibitor),
        // The client folds the epic monsters into one name on some patches; the monster type says
        // which. An epic monster this build cannot name stays a plain objective.
        "EpicMonsterKill" => Some(epic_monster_kind(entry.monster_type.as_deref())),
        // Personal milestones: an enemy's multikill, the other team's ace and somebody else's first
        // blood are not moments of the user's own screen. Each names its player in a different field.
        "Multikill" => {
            is_watched(names, entry.killer_name.as_deref()).then_some(EventKind::Objective)
        }
        "FirstBlood" => {
            is_watched(names, entry.recipient.as_deref()).then_some(EventKind::Objective)
        }
        "Ace" => is_watched(names, entry.acer.as_deref()).then_some(EventKind::Objective),
        "HordeKill" => (is_watched(names, entry.killer_name.as_deref())
            || entry
                .assisters
                .iter()
                .any(|a| is_watched(names, Some(a.as_str()))))
        .then_some(EventKind::Objective),
        // The lifecycle is the state machine's business; pacing events are not clip-worthy, and an
        // unknown name is a patch this build has not seen.
        _ => None,
    }
}

/// The sub-kind of an `EpicMonsterKill`, from the client's `MonsterType` (`"Dragon"`,
/// `"Baron"`, `"Horde"`, ...), matched case-insensitively and by prefix, as the client has
/// spelled these both `"BARON_NASHOR"` and `"Baron"`.
fn epic_monster_kind(monster_type: Option<&str>) -> EventKind {
    let monster = monster_type.unwrap_or_default().to_ascii_lowercase();
    if monster.starts_with("dragon") {
        EventKind::Dragon
    } else if monster.starts_with("baron") {
        EventKind::Baron
    } else if monster.starts_with("herald") || monster.starts_with("sru_riftherald") {
        EventKind::Herald
    } else {
        EventKind::Objective
    }
}

/// A per-event payload: what happened plus the fields the Live Client gave for it, built field
/// by field so an omitted key is absent rather than null (`"killer": null` is noise in the
/// column a human and the session UI read).
fn event_payload(source: &str, name: &str, entry: &StreamEvent, game_time: f64) -> Value {
    let mut map = Map::new();
    map.insert("source".into(), json!(source));
    map.insert("event".into(), json!(name));
    if entry.event_id != 0 {
        map.insert("event_id".into(), json!(entry.event_id));
    }
    map.insert("game_time_s".into(), json!(round_tenth(game_time)));
    if let Some(killer) = &entry.killer_name {
        map.insert("killer".into(), json!(killer));
    }
    if let Some(victim) = &entry.victim_name {
        map.insert("victim".into(), json!(victim));
    }
    if let Some(recipient) = &entry.recipient {
        map.insert("recipient".into(), json!(recipient));
    }
    if let Some(acer) = &entry.acer {
        map.insert("acer".into(), json!(acer));
    }
    if !entry.assisters.is_empty() {
        map.insert("assisters".into(), json!(entry.assisters));
    }
    if let Some(monster) = &entry.monster_type {
        map.insert("monster".into(), json!(monster));
    }
    if let Some(sub) = &entry.monster_sub_type {
        map.insert("monster_sub_type".into(), json!(sub));
    }
    if let Some(stolen) = &entry.stolen {
        // The client sends the string "True"/"False"; a boolean is what a reader wants.
        map.insert("stolen".into(), json!(stolen.eq_ignore_ascii_case("true")));
    }
    if let Some(streak) = entry.kill_streak {
        map.insert("kill_streak".into(), json!(streak));
    }
    Value::Object(map)
}

fn stream_event(
    name: &str,
    entry: &StreamEvent,
    kind: EventKind,
    snapshot: &GameSnapshot,
) -> GameEvent {
    GameEvent::new(
        Source::Lol,
        kind,
        event_payload("lol", name, entry, snapshot.game_time()),
    )
}

/// The `GameStart` payload: which game, and how much of it was already over.
fn start_event(snapshot: &GameSnapshot, adopted: u64) -> GameEvent {
    let mut map = Map::new();
    map.insert("source".into(), json!("lol"));
    map.insert("event".into(), json!("GameStart"));
    map.insert("game_mode".into(), json!(snapshot.game_data.game_mode));
    map.insert("map".into(), json!(snapshot.game_data.map_name));
    map.insert(
        "game_time_s".into(),
        json!(round_tenth(snapshot.game_time())),
    );
    map.insert("adopted_in_progress_events".into(), json!(adopted));
    // Whether the document named the watched player. `false` is a spectated game, a replay or a
    // patch that moved the field, and means no personal event (kill, death, assist) will follow.
    map.insert(
        "identity".into(),
        json!(!snapshot.player_names().is_empty()),
    );
    GameEvent::new(Source::Lol, EventKind::GameStart, Value::Object(map))
}

/// The `GameEnd` payload. The endpoint just stops answering, so the reason is stated in the
/// record rather than inferred from an absence.
///
/// `scoreboard` (the last seen) and `result` (`"Win"`/`"Lose"`, only when the document carried
/// its own `GameEnd` line) ride along so the match keeps its end-of-game scoreboard; `result` is
/// `null` rather than guessed when the endpoint merely went quiet.
fn end_event(last_game_time: f64, scoreboard: Value, result: Option<String>) -> GameEvent {
    let scoreboard = if scoreboard.is_null() {
        json!([])
    } else {
        scoreboard
    };
    GameEvent::new(
        Source::Lol,
        EventKind::GameEnd,
        json!({
            "source": "lol",
            "event": "GameEnd",
            "reason": "the Live Client stopped serving game data",
            "last_game_time_s": round_tenth(last_game_time),
            "result": result,
            "scoreboard": scoreboard,
        }),
    )
}

/// Two decimal places is more than these values need and keeps payloads readable and
/// comparable in tests.
fn round_tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const IN_PROGRESS: &str = include_str!("../../tests/fixtures/lol_in_progress.json");
    const EVENTS_NEXT: &str = include_str!("../../tests/fixtures/lol_events_next.json");
    const SECOND_GAME: &str = include_str!("../../tests/fixtures/lol_second_game.json");
    const NO_IDENTITY: &str = include_str!("../../tests/fixtures/lol_no_identity.json");
    const NONE: &str = include_str!("../../tests/fixtures/lol_none.json");
    const EMPTY: &str = include_str!("../../tests/fixtures/lol_empty.json");

    fn snapshot(fixture: &str) -> Option<GameSnapshot> {
        crate::lol::payload::parse(fixture.as_bytes()).expect("the fixture parses")
    }

    fn kinds(events: &[GameEvent]) -> Vec<EventKind> {
        events.iter().map(|e| e.kind).collect()
    }

    fn payload_of(events: &[GameEvent], kind: EventKind) -> &str {
        events
            .iter()
            .find(|e| e.kind == kind)
            .and_then(|e| e.payload.as_deref())
            .expect("the event is present and carries a payload")
    }

    #[test]
    fn attaching_to_a_game_already_in_progress_reports_the_attach_and_nothing_else() {
        // The event storm this policy prevents: the document already holds six events, two of them the
        // watched player's kill and death.
        let mut tracker = Tracker::default();
        let first = tracker.observe(snapshot(IN_PROGRESS).as_ref());

        assert_eq!(
            kinds(&first),
            vec![EventKind::GameStart],
            "one event, not seven"
        );
        assert_eq!(
            tracker.adopted_on_attach(),
            6,
            "the whole history is adopted as seen"
        );
        assert!(tracker.in_game());
        assert_eq!(tracker.attached_map(), "Map11");
        let payload = first[0].payload.as_deref().unwrap();
        assert!(
            payload.contains("\"adopted_in_progress_events\":6"),
            "got {payload}"
        );
        assert!(payload.contains("\"map\":\"Map11\""), "got {payload}");
    }

    #[test]
    fn the_same_document_twice_emits_nothing_the_second_time() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        // The poller serves the same document for several seconds while a game is quiet; each of those
        // polls must be a no-op.
        for _ in 0..5 {
            assert!(tracker.observe(snapshot(IN_PROGRESS).as_ref()).is_empty());
        }
    }

    #[test]
    fn every_event_of_a_full_history_is_derived_once_and_in_order() {
        // No attach here: the tracker starts on an empty document. Reached by attaching to an empty
        // game and then seeing the history grow.
        let mut tracker = Tracker::default();

        // 1. A game with no events yet: attach.
        let start =
            crate::lol::payload::parse(br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"}}"#)
                .unwrap();
        assert_eq!(
            kinds(&tracker.observe(start.as_ref())),
            vec![EventKind::GameStart]
        );

        // 2. The full history arrives in one document.
        let full = tracker.observe(snapshot(IN_PROGRESS).as_ref());
        assert_eq!(
            kinds(&full),
            vec![
                EventKind::Kill,   // EventID 2, the player's kill
                EventKind::Death,  // EventID 3, the player died
                EventKind::Dragon, // EventID 5, a dragon
            ],
            "the player's kills/deaths and the objectives, in the document's order — and \
             nothing for EventID 4, which involved nobody the document names as the player"
        );
        assert_eq!(
            full.iter().map(|e| e.source).collect::<Vec<_>>(),
            vec![Source::Lol; 3]
        );

        // 3. MinionsSpawning (id 1) and GameStart (id 0) are adopted without a clip.
        let payload = payload_of(&full, EventKind::Kill);
        assert!(
            payload.contains("\"killer\":\"TestPlayer#EUW\""),
            "got {payload}"
        );
        assert!(payload.contains("\"victim\":\"ZedMain\""), "got {payload}");

        // 4. The same document again: nothing.
        assert!(tracker.observe(snapshot(IN_PROGRESS).as_ref()).is_empty());
    }

    #[test]
    fn one_new_event_between_two_polls_is_emitted_exactly_once() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());

        let next = tracker.observe(snapshot(EVENTS_NEXT).as_ref());
        assert_eq!(
            kinds(&next),
            vec![EventKind::Kill, EventKind::Baron],
            "exactly the two new lines (ids 6 and 7): the six already seen are not replayed"
        );
        assert!(
            payload_of(&next, EventKind::Baron).contains("\"monster\":\"BARON_NASHOR\""),
            "the incremental event carries its own detail"
        );
        assert!(payload_of(&next, EventKind::Baron).contains("\"stolen\":true"));

        assert!(tracker.observe(snapshot(EVENTS_NEXT).as_ref()).is_empty());
    }

    #[test]
    fn the_game_ending_emits_one_end_event_and_nothing_after_it() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        tracker.observe(snapshot(EVENTS_NEXT).as_ref());

        // The Live Client stops answering when the game ends: the poller reports that as
        // "no game".
        let ended = tracker.observe(None);
        assert_eq!(kinds(&ended), vec![EventKind::GameEnd]);
        assert!(!tracker.in_game());
        assert_eq!(
            tracker.attached_map(),
            "",
            "the game is over; there is no map"
        );
        let payload = ended[0].payload.as_deref().unwrap();
        assert!(
            payload.contains("\"last_game_time_s\":730.2"),
            "got {payload}"
        );

        // And every later no-game poll is silent.
        for _ in 0..3 {
            assert!(
                tracker.observe(None).is_empty(),
                "the falling edge fires once"
            );
        }
    }

    #[test]
    fn a_null_payload_or_an_empty_document_is_no_game() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());

        // `null` is a documented way for the endpoint to say "no game"; an empty object is
        // a client that is up with nothing to report. Both end the game, once.
        assert_eq!(
            kinds(&tracker.observe(snapshot(NONE).as_ref())),
            vec![EventKind::GameEnd]
        );
        assert!(
            tracker.observe(snapshot(EMPTY).as_ref()).is_empty(),
            "already ended"
        );
    }

    #[test]
    fn a_new_game_starting_mid_game_resets_the_tracker_instead_of_replaying_it() {
        // The abrupt re-attach: the player quit and requeued and the endpoint never went quiet, so the
        // first thing seen of the new game is a fresh GameStart line, with ids restarting at 0.
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        tracker.observe(snapshot(EVENTS_NEXT).as_ref());

        let second = tracker.observe(snapshot(SECOND_GAME).as_ref());
        assert_eq!(
            kinds(&second),
            vec![EventKind::GameStart],
            "the new game's history is adopted, not emitted"
        );
        assert_eq!(tracker.adopted_on_attach(), 2);
        assert!(tracker.in_game());
    }

    #[test]
    fn without_a_named_player_no_champion_kill_is_the_users() {
        // A spectated game, a replay or a patch that moved the field: the document names no player.
        // Every kill used to become the user's own `Kill`, so a replay clipped all ten players.
        let mut tracker = Tracker::default();
        let start = tracker.observe(
            crate::lol::payload::parse(br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"}}"#)
                .unwrap()
                .as_ref(),
        );
        assert!(
            start[0]
                .payload
                .as_deref()
                .unwrap()
                .contains("\"identity\":false"),
            "the attach says it had no identity"
        );
        // The document names no player but reports a kill, and a baron (which needs no identity).
        let body = br#"{"gameData":{"gameTime":100.0,"mapName":"Map11"},
                       "events":{"Events":[
                         {"EventID":11,"EventName":"ChampionKill","EventTime":95.0,"KillerName":"Somebody","VictimName":"Someone"},
                         {"EventID":12,"EventName":"Multikill","EventTime":96.0,"KillerName":"Somebody","KillStreak":2},
                         {"EventID":13,"EventName":"BaronKill","EventTime":97.0,"KillerName":"Somebody"}]}}"#;
        let events = tracker.observe(crate::lol::payload::parse(body).unwrap().as_ref());
        assert_eq!(
            kinds(&events),
            vec![EventKind::Baron],
            "no personal event without an identity; the game-level milestone still counts"
        );
        // The shipped no-identity fixture (an `activePlayer` with no name) behaves the same.
        let mut tracker = Tracker::default();
        tracker.observe(
            crate::lol::payload::parse(br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"}}"#)
                .unwrap()
                .as_ref(),
        );
        assert!(tracker.observe(snapshot(NO_IDENTITY).as_ref()).is_empty());
    }

    #[test]
    fn personal_milestones_are_clipped_only_for_the_watched_player() {
        let me = ["TestPlayer#EUW", "TestPlayer"];
        let entry = |json: &str| -> StreamEvent { serde_json::from_str(json).unwrap() };
        for (name, mine, theirs) in [
            (
                "Multikill",
                r#"{"EventName":"Multikill","KillerName":"TestPlayer","KillStreak":3}"#,
                r#"{"EventName":"Multikill","KillerName":"EnemyMid","KillStreak":3}"#,
            ),
            (
                "FirstBlood",
                r#"{"EventName":"FirstBlood","Recipient":"TestPlayer#EUW"}"#,
                r#"{"EventName":"FirstBlood","Recipient":"EnemyTop"}"#,
            ),
            (
                "Ace",
                r#"{"EventName":"Ace","Acer":"TestPlayer","AcingTeam":"ORDER"}"#,
                r#"{"EventName":"Ace","Acer":"EnemyAdc","AcingTeam":"CHAOS"}"#,
            ),
            (
                "HordeKill",
                r#"{"EventName":"HordeKill","KillerName":"TestPlayer"}"#,
                r#"{"EventName":"HordeKill","KillerName":"AllyJungle"}"#,
            ),
        ] {
            assert_eq!(
                classify(name, &entry(mine), &me),
                Some(EventKind::Objective),
                "{name} by the watched player"
            );
            assert_eq!(
                classify(name, &entry(theirs), &me),
                None,
                "{name} by somebody else is not a clip of this screen"
            );
            assert_eq!(
                classify(name, &entry(mine), &[]),
                None,
                "{name} with no identity cannot be called the user's"
            );
        }
        // Assisting a horde kill is taking part in it.
        assert_eq!(
            classify(
                "HordeKill",
                &entry(
                    r#"{"EventName":"HordeKill","KillerName":"AllyJungle","Assisters":["TestPlayer"]}"#
                ),
                &me
            ),
            Some(EventKind::Objective)
        );
    }

    #[test]
    fn an_own_kill_is_a_kill_a_shared_kill_is_an_assist_and_an_own_death_is_a_death() {
        // One document per case, so classification is pinned to the rule, not the fixture's ordering.
        let with_player = |event: &str| -> Vec<EventKind> {
            let body = format!(
                r#"{{"gameData":{{"gameTime":100.0,"mapName":"Map11"}},
                     "activePlayer":{{"summonerName":"TestPlayer#EUW"}},
                     "events":{{"Events":[{event}]}}}}"#
            );
            let mut tracker = Tracker::default();
            // Attach first on a document with no events, so the event under test is a new line, not part
            // of an adopted history.
            tracker.observe(
                crate::lol::payload::parse(
                    br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"},
                         "activePlayer":{"summonerName":"TestPlayer#EUW"}}"#,
                )
                .unwrap()
                .as_ref(),
            );
            let events = tracker.observe(
                crate::lol::payload::parse(body.as_bytes())
                    .unwrap()
                    .as_ref(),
            );
            kinds(&events)
        };

        assert_eq!(
            with_player(
                r#"{"EventID":2,"EventName":"ChampionKill","EventTime":100.0,"KillerName":"TestPlayer#EUW","VictimName":"ZedMain"}"#
            ),
            vec![EventKind::Kill]
        );
        assert_eq!(
            with_player(
                r#"{"EventID":2,"EventName":"ChampionKill","EventTime":100.0,"KillerName":"ZedMain","VictimName":"TestPlayer#EUW"}"#
            ),
            vec![EventKind::Death]
        );
        assert_eq!(
            with_player(
                r#"{"EventID":2,"EventName":"ChampionKill","EventTime":100.0,"KillerName":"ZedMain","VictimName":"EnemyTop","Assisters":["TestPlayer#EUW"]}"#
            ),
            vec![EventKind::Assist]
        );
        assert!(
            with_player(r#"{"EventID":2,"EventName":"ChampionKill","EventTime":100.0,"KillerName":"EnemyTop","VictimName":"EnemyJungle"}"#).is_empty(),
            "a fight the player was not in is not a clip"
        );
    }

    #[test]
    fn game_level_milestones_are_derived_whoever_did_them() {
        for (name, kind) in [
            ("DragonKill", EventKind::Dragon),
            ("BaronKill", EventKind::Baron),
            ("HeraldKill", EventKind::Herald),
            ("EpicMonsterKill", EventKind::Objective),
            ("TurretKilled", EventKind::Turret),
            ("InhibKilled", EventKind::Inhibitor),
            ("FirstBrick", EventKind::Turret),
        ] {
            assert_eq!(
                classify(name, &StreamEvent::default(), &["me"]),
                Some(kind),
                "{name} is an objective"
            );
            assert!(kind.is_objective());
        }
        // An epic monster is named by its type when the client folds them into one event.
        for (monster, kind) in [
            ("Dragon", EventKind::Dragon),
            ("BARON_NASHOR", EventKind::Baron),
            ("SRU_RiftHerald", EventKind::Herald),
            ("Horde", EventKind::Objective),
        ] {
            let entry = StreamEvent {
                monster_type: Some(monster.into()),
                ..StreamEvent::default()
            };
            assert_eq!(
                classify("EpicMonsterKill", &entry, &["me"]),
                Some(kind),
                "{monster}"
            );
        }
        // Pacing and lifecycle events never come out of the table.
        for name in [
            "MinionsSpawning",
            "GameStart",
            "GameEnd",
            "SomethingFromAPatch",
        ] {
            assert_eq!(
                classify(name, &StreamEvent::default(), &["me"]),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn an_id_less_document_still_emits_an_event_only_once() {
        // The client has omitted `EventID`. The fallback key is the event's content, so polling the
        // same document twice is still a no-op.
        let body = br#"{"gameData":{"gameTime":100.0,"mapName":"Map11"},
                       "activePlayer":{"summonerName":"TestPlayer#EUW"},
                       "events":{"Events":[{"EventName":"DragonKill","EventTime":99.3,"MonsterType":"DRAGON"}]}}"#;
        let document = crate::lol::payload::parse(body).unwrap();

        let mut tracker = Tracker::default();
        tracker.observe(
            crate::lol::payload::parse(
                br#"{"gameData":{"gameTime":-20.0,"mapName":"Map11"},
                     "activePlayer":{"summonerName":"TestPlayer#EUW"}}"#,
            )
            .unwrap()
            .as_ref(),
        );
        assert_eq!(
            kinds(&tracker.observe(document.as_ref())),
            vec![EventKind::Dragon]
        );
        assert!(
            tracker.observe(document.as_ref()).is_empty(),
            "the same line again"
        );

        // A different id-less event is emitted: the key is content, not "any id-less event".
        let other = crate::lol::payload::parse(
            br#"{"gameData":{"gameTime":110.0,"mapName":"Map11"},
                 "activePlayer":{"summonerName":"TestPlayer#EUW"},
                 "events":{"Events":[{"EventName":"DragonKill","EventTime":99.3,"MonsterType":"DRAGON"},
                                     {"EventName":"BaronKill","EventTime":108.9,"MonsterType":"BARON_NASHOR"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            kinds(&tracker.observe(other.as_ref())),
            vec![EventKind::Baron]
        );
    }

    #[test]
    fn a_game_end_event_inside_the_document_is_not_reported_twice() {
        // Some builds put the end of the game in the stream. It ends the game; the 404 that follows
        // must not end it again.
        let body = br#"{"gameData":{"gameTime":2400.0,"mapName":"Map11"},
                       "activePlayer":{"summonerName":"TestPlayer#EUW"},
                       "events":{"Events":[{"EventID":40,"EventName":"GameEnd","EventTime":2400.0}]}}"#;
        let document = crate::lol::payload::parse(body).unwrap();

        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        assert_eq!(
            kinds(&tracker.observe(document.as_ref())),
            vec![EventKind::GameEnd]
        );
        assert!(!tracker.in_game());
        assert!(
            tracker.observe(None).is_empty(),
            "the endpoint going quiet is not a second end"
        );
    }

    #[test]
    fn the_last_poll_before_the_end_keeps_its_events_and_the_repeated_document_starts_nothing() {
        // The final document: the player's kill and a baron, then the game's own `GameEnd`. Those
        // events used to be derived and then thrown away by the `return`, and the same document,
        // served for a few more seconds, re-attached as a new game (a second `GameStart`) whose
        // `GameEnd` the 404 then reported again.
        let body = br#"{"gameData":{"gameTime":2400.0,"mapName":"Map11"},
                       "activePlayer":{"summonerName":"TestPlayer#EUW"},
                       "events":{"Events":[
                         {"EventID":38,"EventName":"ChampionKill","EventTime":2395.0,"KillerName":"TestPlayer#EUW","VictimName":"ZedMain"},
                         {"EventID":39,"EventName":"BaronKill","EventTime":2398.0,"KillerName":"SupportMain"},
                         {"EventID":40,"EventName":"GameEnd","EventTime":2400.0,"Result":"Win"}]}}"#;
        let document = crate::lol::payload::parse(body).unwrap();

        let mut tracker = Tracker::default();
        let mut all = tracker.observe(snapshot(IN_PROGRESS).as_ref());
        let last = tracker.observe(document.as_ref());
        assert_eq!(
            kinds(&last),
            vec![EventKind::Kill, EventKind::Baron, EventKind::GameEnd],
            "the events before the end are delivered, ahead of it"
        );
        all.extend(last);
        // The client keeps serving the finished game; then the endpoint goes quiet.
        for _ in 0..3 {
            all.extend(tracker.observe(document.as_ref()));
            assert!(!tracker.in_game(), "a finished game is not re-entered");
        }
        all.extend(tracker.observe(None));
        all.extend(tracker.observe(None));
        let count = |kind| all.iter().filter(|e| e.kind == kind).count();
        assert_eq!(count(EventKind::GameStart), 1, "one game, one start");
        assert_eq!(count(EventKind::GameEnd), 1, "one game, one end");

        // And the next game is still recognised.
        let next = tracker.observe(snapshot(IN_PROGRESS).as_ref());
        assert_eq!(kinds(&next), vec![EventKind::GameStart]);
    }

    #[test]
    fn attaching_to_a_document_that_already_ended_starts_no_game() {
        // replayed was started in the seconds after a game ended, while the client still serves
        // its last document: there is no game to attach to.
        let body = br#"{"gameData":{"gameTime":2400.0,"mapName":"Map11"},
                       "events":{"Events":[{"EventID":40,"EventName":"GameEnd","EventTime":2400.0}]}}"#;
        let document = crate::lol::payload::parse(body).unwrap();
        let mut tracker = Tracker::default();
        assert!(tracker.observe(document.as_ref()).is_empty());
        assert!(!tracker.in_game());
        assert!(tracker.observe(None).is_empty());
    }

    #[test]
    fn the_falling_edge_end_event_carries_the_last_scoreboard() {
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        let ended = tracker.observe(None);
        let value: serde_json::Value =
            serde_json::from_str(ended[0].payload.as_deref().unwrap()).unwrap();
        let rows = value["scoreboard"].as_array().expect("a scoreboard array");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["champion"], "Ahri");
        assert_eq!(rows[0]["kills"], 5);
        assert_eq!(rows[0]["cs"], 142);
        assert_eq!(rows[0]["is_local"], true);
        assert_eq!(rows[1]["is_local"], false);
        assert!(
            value["result"].is_null(),
            "no result is guessed from an absence"
        );
    }

    #[test]
    fn an_in_stream_game_end_carries_the_result_and_the_scoreboard() {
        let body = br#"{"gameData":{"gameTime":2400.0,"mapName":"Map11"},
                       "activePlayer":{"summonerName":"TestPlayer#EUW"},
                       "allPlayers":[{"summonerName":"TestPlayer#EUW","championName":"Ahri","team":"ORDER",
                                      "scores":{"kills":7,"deaths":2,"assists":9,"creepScore":210,"wardScore":20.0}}],
                       "events":{"Events":[{"EventID":40,"EventName":"GameEnd","EventTime":2400.0,"Result":"Win"}]}}"#;
        let document = crate::lol::payload::parse(body).unwrap();
        let mut tracker = Tracker::default();
        tracker.observe(snapshot(IN_PROGRESS).as_ref());
        let ended = tracker.observe(document.as_ref());
        let value: serde_json::Value =
            serde_json::from_str(ended[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(value["result"], "Win");
        assert_eq!(value["scoreboard"][0]["kills"], 7);
        assert_eq!(value["scoreboard"][0]["is_local"], true);
    }
}
