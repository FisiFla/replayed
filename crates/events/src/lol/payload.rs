//! The `allgamedata` response, as typed Rust.
//!
//! This describes Riot's document and treats it as untrusted: every field is optional, every
//! default is empty, and only the JSON parse itself can fail. The payload differs between game
//! modes, patches and the loading screen, and a parser insisting on a field would stop
//! integrating with a game running perfectly well.
//!
//! Two things the document does not give cleanly:
//!
//! * Who the watched player is. Event lines carry summoner names (`KillerName`, `VictimName`,
//!   `Assisters`) while `activePlayer` may carry a summoner name or a Riot ID depending on the
//!   patch. [`GameSnapshot::player_names`] returns every name offered, and the classifier in
//!   [`crate::lol::derive`] matches against all of them.
//! * Whether a game is in progress. [`GameSnapshot::game_present`] answers from the fields a
//!   game always has; an empty object (which the client briefly serves) reads as "no game".

use serde::Deserialize;

/// Parse the body of a `liveclientdata/allgamedata` response.
///
/// `Ok(None)` is the JSON literal `null`, a documented "no game", distinct from an empty
/// object. A body that is not JSON is an error; [`crate::lol::poller`] treats it as a failed
/// poll and keeps its state rather than ending the game.
pub fn parse(body: &[u8]) -> Result<Option<GameSnapshot>, serde_json::Error> {
    serde_json::from_slice::<Option<GameSnapshot>>(body)
}

/// One `allgamedata` document.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GameSnapshot {
    pub game_data: GameData,
    pub events: Events,
    pub active_player: ActivePlayer,
    pub all_players: Vec<Player>,
}

impl GameSnapshot {
    /// Whether this document describes a game being played.
    ///
    /// A game names its map and reports a non-zero clock (`gameTime` is negative during the
    /// loading screen and counts up from there). An empty document, as a client between games has
    /// been seen to send, fails both and reads as "no game", not as a game with no data.
    pub fn game_present(&self) -> bool {
        self.game_data.is_present()
    }

    /// The document's own event stream, oldest first as the client writes it.
    pub fn event_stream(&self) -> &[StreamEvent] {
        &self.events.events
    }

    /// Every name the document gives the watched player, deduplicated and without empty strings:
    /// the list is matched against every event line, and an empty name would match a line with no
    /// killer.
    pub fn player_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for candidate in [
            self.active_player.summoner_name.as_deref(),
            self.active_player.riot_id_game_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let candidate = candidate.trim();
            if !candidate.is_empty() && !names.contains(&candidate) {
                names.push(candidate);
            }
        }
        names
    }

    /// The end-of-game scoreboard: one row per entry of `allPlayers`, as a JSON array, read from
    /// the `allgamedata` document every poll already fetches (a GET; nothing is sent to the
    /// client). `is_local` marks the watched player by matching [`GameSnapshot::player_names`].
    /// Empty when the document lists no players.
    pub fn scoreboard(&self) -> serde_json::Value {
        let names = self.player_names();
        let rows: Vec<serde_json::Value> = self
            .all_players
            .iter()
            .map(|p| {
                let name = p
                    .riot_id_game_name
                    .as_deref()
                    .or(p.summoner_name.as_deref());
                let local = [p.summoner_name.as_deref(), p.riot_id_game_name.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|n| names.contains(&n.trim()));
                serde_json::json!({
                    "name": name,
                    "champion": p.champion_name,
                    "team": p.team,
                    "kills": p.scores.kills,
                    "deaths": p.scores.deaths,
                    "assists": p.scores.assists,
                    "cs": p.scores.creep_score,
                    "ward_score": p.scores.ward_score,
                    "is_local": local,
                })
            })
            .collect();
        serde_json::Value::Array(rows)
    }

    /// The game's clock in seconds, which is what the payloads here record as context.
    pub fn game_time(&self) -> f64 {
        self.game_data.game_time
    }
}

/// `gameData` — mode, map and clock.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GameData {
    pub game_mode: String,
    pub game_time: f64,
    pub map_name: String,
    pub map_number: i64,
    pub map_terrain: String,
}

impl GameData {
    /// Whether this names a game: a map, or a clock that is not zero. The one definition both the
    /// poller's tracker and the presence check use, so the two cannot disagree about what "a game
    /// is being played" means.
    pub fn is_present(&self) -> bool {
        !self.map_name.trim().is_empty() || self.game_time != 0.0
    }
}

/// `events` — the game's own event log. The key really is capitalised in the payload.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Events {
    #[serde(rename = "Events")]
    pub events: Vec<StreamEvent>,
}

/// One line of the client's event log. Fields only some events carry (`MonsterType`,
/// `KillStreak`, ...) are optional, and unknown keys are ignored, which keeps this working
/// across patches.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct StreamEvent {
    #[serde(rename = "EventID")]
    pub event_id: i64,
    #[serde(rename = "EventName")]
    pub event_name: String,
    #[serde(rename = "EventTime")]
    pub event_time: f64,
    #[serde(rename = "KillerName")]
    pub killer_name: Option<String>,
    #[serde(rename = "VictimName")]
    pub victim_name: Option<String>,
    #[serde(rename = "Assisters")]
    pub assisters: Vec<String>,
    #[serde(rename = "MonsterType")]
    pub monster_type: Option<String>,
    #[serde(rename = "MonsterSubType")]
    pub monster_sub_type: Option<String>,
    #[serde(rename = "Stolen")]
    pub stolen: Option<String>,
    #[serde(rename = "KillStreak")]
    pub kill_streak: Option<i64>,
    /// `FirstBlood` only: who drew it (`Recipient`).
    #[serde(rename = "Recipient")]
    pub recipient: Option<String>,
    /// `Ace` only: who finished the enemy team (`Acer`).
    #[serde(rename = "Acer")]
    pub acer: Option<String>,
    #[serde(rename = "TurretKilled")]
    pub turret_killed: Option<String>,
    #[serde(rename = "InhibKilled")]
    pub inhib_killed: Option<String>,
    /// `GameEnd` only: `"Win"` or `"Lose"` for the local player's team.
    #[serde(rename = "Result")]
    pub result: Option<String>,
}

/// `activePlayer` — the local player, as far as the client will name them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ActivePlayer {
    pub summoner_name: Option<String>,
    pub riot_id_game_name: Option<String>,
    pub level: Option<i64>,
    pub current_gold: Option<f64>,
}

/// One entry of `allPlayers`. Only the identity and the scoreboard are kept: the document
/// is large, and nothing else in it is used.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Player {
    pub summoner_name: Option<String>,
    pub riot_id_game_name: Option<String>,
    pub champion_name: Option<String>,
    pub team: Option<String>,
    pub scores: Scoreboard,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Scoreboard {
    pub kills: i64,
    pub deaths: i64,
    pub assists: i64,
    pub creep_score: i64,
    pub ward_score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A game in progress: the shape the live endpoint serves, trimmed to the parts this crate
    /// reads.
    const IN_PROGRESS: &str = include_str!("../../tests/fixtures/lol_in_progress.json");

    #[test]
    fn parses_a_game_in_progress() {
        let snapshot = parse(IN_PROGRESS.as_bytes())
            .expect("the fixture is valid JSON")
            .expect("the fixture is not null");
        assert!(snapshot.game_present());
        assert_eq!(snapshot.game_data.game_mode, "CLASSIC");
        assert_eq!(snapshot.game_data.map_name, "Map11");
        assert_eq!(snapshot.game_data.map_number, 11);
        assert!(
            snapshot.game_time() > 600.0,
            "the fixture is well into the game"
        );
        assert_eq!(snapshot.all_players.len(), 2);
        assert_eq!(snapshot.all_players[0].scores.kills, 5);
        assert_eq!(snapshot.event_stream().len(), 6);
    }

    #[test]
    fn the_event_stream_keeps_its_order_and_its_optional_fields() {
        let snapshot = parse(IN_PROGRESS.as_bytes()).unwrap().unwrap();
        let events = snapshot.event_stream();
        assert_eq!(events[0].event_id, 0);
        assert_eq!(events[0].event_name, "GameStart");
        assert_eq!(events[1].event_name, "MinionsSpawning");
        assert_eq!(events[2].event_name, "ChampionKill");
        assert_eq!(events[2].killer_name.as_deref(), Some("TestPlayer#EUW"));
        assert_eq!(events[2].victim_name.as_deref(), Some("ZedMain"));
        assert_eq!(events[2].assisters, vec!["SupportMain".to_string()]);

        let dragon = events
            .iter()
            .find(|e| e.event_name == "DragonKill")
            .expect("a dragon");
        assert_eq!(dragon.monster_type.as_deref(), Some("DRAGON"));
        assert_eq!(dragon.monster_sub_type.as_deref(), Some("FIRE_DRAGON"));
        assert_eq!(dragon.stolen.as_deref(), Some("False"));
    }

    #[test]
    fn names_the_watched_player_by_every_name_the_document_offers() {
        let snapshot = parse(IN_PROGRESS.as_bytes()).unwrap().unwrap();
        // The fixture gives the same player under both naming schemes a patch may use; both must
        // match an event line and neither may appear twice.
        let names = snapshot.player_names();
        assert!(names.contains(&"TestPlayer#EUW"), "got {names:?}");
        assert!(names.contains(&"TestPlayer"), "got {names:?}");
        assert_eq!(names.len(), 2, "no duplicates: {names:?}");
    }

    #[test]
    fn a_document_with_no_active_player_names_nobody() {
        let snapshot = parse(br#"{"gameData":{"gameTime":10.0,"mapName":"Map11"}}"#)
            .unwrap()
            .unwrap();
        assert!(snapshot.game_present());
        assert!(snapshot.player_names().is_empty());
        assert!(snapshot.event_stream().is_empty());
    }

    #[test]
    fn a_null_body_is_not_a_game() {
        assert_eq!(parse(b"null").unwrap(), None);
        assert_eq!(parse(b"  null\n").unwrap(), None);
    }

    #[test]
    fn an_empty_document_is_not_a_game() {
        let snapshot = parse(b"{}").unwrap().expect("an object, not null");
        assert!(
            !snapshot.game_present(),
            "an empty document describes no game"
        );
        assert!(snapshot.player_names().is_empty());
    }

    #[test]
    fn a_body_that_is_not_json_is_an_error_and_not_a_panic() {
        for body in [
            &b"not json at all"[..],
            &b"<html><body>404</body></html>"[..],
            &b"{"[..],
            &b"\xff\xfe\x00"[..],
            &b""[..],
        ] {
            assert!(
                parse(body).is_err(),
                "{:?} must not parse",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn unknown_and_missing_fields_are_ignored() {
        // A future patch adding fields and a partial document missing them must both parse.
        let body = br#"{
            "gameData": {"gameMode": "ARAM", "mapName": "Map12", "somethingNew": [1,2,3]},
            "events": {"Events": [{"EventID": 7, "EventName": "Ace", "EventTime": 300.5,
                                   "NewField": {"nested": true}}], "Extra": 1},
            "activePlayer": {"summonerName": "Someone"},
            "aBlockFromTheFuture": {"x": 1}
        }"#;
        let snapshot = parse(body).unwrap().unwrap();
        assert!(snapshot.game_present());
        assert_eq!(snapshot.game_data.game_mode, "ARAM");
        assert_eq!(snapshot.event_stream()[0].event_name, "Ace");
        assert_eq!(snapshot.player_names(), vec!["Someone"]);
    }

    #[test]
    fn a_payload_whose_types_disagree_is_an_error() {
        // The Live Client has served numbers where strings belong and vice versa; a wrong type must
        // be a parse error, not a silently zeroed field.
        assert!(parse(br#"{"gameData": {"gameTime": "soon"}}"#).is_err());
        assert!(parse(br#"{"allPlayers": {"not": "an array"}}"#).is_err());
    }
}
