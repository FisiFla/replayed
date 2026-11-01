//! The Valve GSI POST body, as typed Rust.
//!
//! One document type covers Counter-Strike 2 / CS:GO and Dota 2. They overlap in `provider` and
//! `map` and differ elsewhere, and Valve's contract is that every block is optional (the
//! installed `.cfg` decides what is sent). So every field defaults, unknown keys are ignored,
//! and an unrecognised payload still parses and yields nothing.
//!
//! Two shape differences a hand-written parser gets wrong:
//!
//! * Dota 2's `player` block is an object in a player's own client and an array in a
//!   spectator's. [`PlayerBlock`] accepts both (see [`PlayerBlock::first`]).
//! * A player's kills live in `player.match_stats` in Counter-Strike and directly on `player`
//!   in Dota 2, under the same names. [`Player::kills`], [`Player::deaths`] and
//!   [`Player::assists`] answer for both.
//!
//! # The `auth` block is read on its own
//!
//! The token gate needs the `auth` block and nothing else, and a body can be two megabytes
//! of a scoreboard. So [`auth_token`] reads the block without parsing the rest of the
//! document: the parser walks the body until it finds `auth` and stops there, and the rest
//! is never turned into a value. The full document is parsed only after the token has been
//! accepted. Without that, an unauthenticated peer could make the recorder parse megabytes
//! per request, and a cross-origin `POST` from a web page in the user's browser is
//! unauthenticated by construction.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Parse a GSI POST body.
///
/// The body must be a JSON object. `serde`'s derive will read a struct out of a JSON array
/// (an empty `[]` becomes a document with every field defaulted), which would turn a nonsense
/// body into "a document that says nothing" that the listener answers `200`. The shape is
/// therefore checked once, at the edge.
pub fn parse(body: &[u8]) -> Result<GsiSnapshot, serde_json::Error> {
    let object: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_slice(body)?;
    serde_json::from_value(serde_json::Value::Object(object.into_iter().collect()))
}

/// What a GSI document's `auth` block says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthToken {
    /// The document carries this token.
    Present(String),
    /// The document is a JSON object with no `auth.token`: there is nothing to compare
    /// against, so the request is refused.
    Absent,
    /// The document is not a readable JSON object, so it has no `auth` block in it. The
    /// reason is not carried: the body is attacker-controlled until the token is accepted,
    /// and a parser's message can quote it.
    Unreadable,
}

/// The token in a GSI POST body's `auth` block, read without building the rest of the body.
///
/// The whole document is parsed only once the token has been accepted; this is the cheap
/// look that comes first, and it is cheap in one way that matters: it never turns the rest of
/// the body into values. It skips everything that is not `auth` ([`serde::de::IgnoredAny`]) and
/// stops at the block once it has it.
///
/// # The block is usually the last key
///
/// An earlier version read only the first 64 KiB, on the belief that the game sends `auth`
/// first. It does not: Valve writes the blocks in its own order and `auth` (the block the
/// installed cfg declares) comes after `provider`, `map`, `round`, `player`, `allplayers_*`
/// and `previously`/`added`. A body over the window was therefore cut off before its token and
/// answered `400`, and a game whose payload grew past 64 KiB (a full ten-player scoreboard with
/// weapons and positions can) would silently stop being heard. The probe now walks the body as
/// far as it must. The walk is bounded by the body's size, which [`crate::gsi::http`] has
/// already capped at [`crate::gsi::http::MAX_BODY_BYTES`] before this is called, and an
/// unauthenticated peer's walks are charged to the anonymous request budget in
/// [`crate::gsi`], not to the game's.
///
/// The first `auth` block wins. A body carrying two is not something the game sends, and
/// the document that is finally parsed is the authority on the token anyway
/// ([`crate::gsi::token_matches`]), so a body that hides a second block behind the first
/// is refused rather than believed.
pub fn auth_token(body: &[u8]) -> AuthToken {
    let mut found: Option<BTreeMap<String, String>> = None;
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let outcome =
        serde::Deserializer::deserialize_any(&mut deserializer, AuthVisitor { found: &mut found });
    match found {
        Some(auth) => match auth.get("token").cloned() {
            Some(token) => AuthToken::Present(token),
            None => AuthToken::Absent,
        },
        // The document was read to its end without an `auth` block: it is a document, and it
        // carries no token.
        None => match outcome {
            Ok(()) => AuthToken::Absent,
            Err(_) => AuthToken::Unreadable,
        },
    }
}

/// How much of each end of a body [`carries_token`] looks at.
const HINT_WINDOW: usize = 8 * 1024;

/// The most `"token"` keys looked at in each window, so a body made of nothing else costs a
/// bounded amount.
const HINT_CANDIDATES: usize = 8;

/// The longest string taken as a candidate token. The generated token is 32 hex digits; a
/// longer string cannot be it, and refusing to read one keeps the scan bounded.
const HINT_MAX_TOKEN: usize = 128;

/// Whether the start or the end of `body` holds the string `expected` as the value of a
/// `"token"` key. An advisory look, not a gate.
///
/// The listener uses it to tell, before it has parsed anything, a request that is very likely
/// the game's own from one that is not, so that the game's POSTs are not charged to the budget
/// an anonymous peer burns through ([`crate::gsi`]). It reads only the first and the last
/// `HINT_WINDOW` bytes, because the block sits at one end or the other of a real payload and
/// a scan of the middle of a two-megabyte body is the cost it exists to avoid.
///
/// What it decides is only which queue a request waits in. [`auth_token`] and
/// [`crate::gsi::token_matches`] still decide whether it is believed, so fooling this saves an
/// attacker nothing without the token: the comparison against the secret is constant time
/// (`crate::gsi::token_equals`, the same constant-time comparison the gate uses) and happens only on a string this function has cut out of
/// the body, never as a search of the body for the secret.
pub fn carries_token(body: &[u8], expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let head = &body[..body.len().min(HINT_WINDOW)];
    let tail = &body[body.len().saturating_sub(HINT_WINDOW)..];
    window_carries_token(head, expected)
        || (body.len() > HINT_WINDOW && window_carries_token(tail, expected))
}

fn window_carries_token(window: &[u8], expected: &str) -> bool {
    const KEY: &[u8] = b"\"token\"";
    let mut matched = false;
    let mut from = 0;
    for _ in 0..HINT_CANDIDATES {
        let Some(at) = window[from..]
            .windows(KEY.len())
            .position(|candidate| candidate == KEY)
        else {
            break;
        };
        let mut rest = &window[from + at + KEY.len()..];
        from += at + KEY.len();
        rest = skip_blanks(rest);
        let Some(after_colon) = rest.strip_prefix(b":") else {
            continue;
        };
        let Some(value) = skip_blanks(after_colon).strip_prefix(b"\"") else {
            continue;
        };
        let end = value
            .iter()
            .take(HINT_MAX_TOKEN + 1)
            .position(|byte| *byte == b'"');
        if let Some(end) = end {
            if let Ok(candidate) = std::str::from_utf8(&value[..end]) {
                // No early exit: every candidate is compared whether or not an earlier one matched.
                matched |= super::token_equals(expected, candidate);
            }
        }
    }
    matched
}

fn skip_blanks(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t' | b'\r' | b'\n', rest @ ..] = bytes {
        bytes = rest;
    }
    bytes
}

/// A visitor that reads a document's `auth` block and stops there.
///
/// The block is handed back through a slot rather than through the visitor's value, because
/// the value of a document whose reading was cut short is an error: the parser insists on
/// the closing brace of a map it has already read, and the whole point is to stop before the
/// rest of the document.
struct AuthVisitor<'a> {
    found: &'a mut Option<BTreeMap<String, String>>,
}

impl<'de, 'a> serde::de::Visitor<'de> for AuthVisitor<'a> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a GSI document")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if key == "auth" {
                *self.found = Some(map.next_value()?);
                // The block is all the gate needs. The rest of the document is left unread,
                // so a two-megabyte payload costs a few bytes of parsing. The error is the
                // signal to stop: the value of a document that was not read to its end is
                // not a value.
                return Err(serde::de::Error::custom("the auth block was read"));
            }
            // Everything else is skipped without being built: a document the size of the
            // attacker's choosing must not become values before the token has been checked.
            map.next_value::<serde::de::IgnoredAny>()?;
        }
        Ok(())
    }
}

/// One GSI document.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct GsiSnapshot {
    /// Whatever the installed `.cfg` declared under `"auth"`, echoed back verbatim by the game.
    /// The token arrives here (see [`crate::gsi::token_matches`]).
    pub auth: BTreeMap<String, String>,
    pub provider: Provider,
    pub map: Map,
    pub round: Round,
    pub player: PlayerBlock,
    pub bomb: Bomb,
    pub hero: Hero,
}

/// `provider` — which game is talking.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Provider {
    pub name: String,
    pub appid: u32,
    pub version: u32,
    pub steamid: Option<String>,
    pub timestamp: u64,
}

/// `map` — the map, the phase of the match, and (in Dota 2) the rules state.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Map {
    pub mode: String,
    pub name: String,
    /// Counter-Strike: `warmup` | `live` | `intermission` | `gameover`.
    pub phase: String,
    /// Counter-Strike: the round number, 1-based.
    pub round: u32,
    pub team_ct: Team,
    pub team_t: Team,
    /// Dota 2: `DOTA_GAMERULES_STATE_*`.
    pub game_state: String,
    /// Dota 2: the match clock in seconds.
    pub game_time: i64,
    pub match_id: Option<String>,
    pub radiant_score: Option<u32>,
    pub dire_score: Option<u32>,
    pub win_team: Option<String>,
}

/// A team's score.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Team {
    pub score: u32,
    pub name: String,
}

/// `round` — Counter-Strike's phase and bomb state for the current round.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Round {
    /// `freezetime` | `live` | `over`.
    pub phase: String,
    /// `planted` | `defused` | `exploded` (absent when nothing happened).
    pub bomb: String,
    pub win_team: Option<String>,
}

/// `bomb`: the detailed bomb state, richer than `round.bomb`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Bomb {
    /// `carried` | `planting` | `planted` | `defusing` | `defused` | `dropped` | `exploded`.
    pub state: String,
    pub countdown: Option<String>,
    pub player: Option<String>,
}

/// `player` — either an object or an array; see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PlayerBlock {
    /// Absent: the `player` block is not subscribed, or the game has not sent one yet.
    #[default]
    Missing,
    One(Box<Player>),
    Many(Vec<Player>),
}

impl PlayerBlock {
    /// The block's player, if it has one.
    ///
    /// A Dota 2 spectator payload has one entry per player slot with no way to tell which is
    /// watched; the first is used. The kill/death derivation is guarded by `steamid` (see
    /// [`crate::gsi::derive`]), so a wrong guess produces no events rather than a burst.
    pub fn first(&self) -> Option<&Player> {
        match self {
            PlayerBlock::Missing => None,
            PlayerBlock::One(player) => Some(player),
            PlayerBlock::Many(players) => players.first(),
        }
    }

    /// The same player, mutably, so derivation tests can edit one field of a fixture.
    pub fn first_mut(&mut self) -> Option<&mut Player> {
        match self {
            PlayerBlock::Missing => None,
            PlayerBlock::One(player) => Some(player),
            PlayerBlock::Many(players) => players.first_mut(),
        }
    }
}

/// `player` — who is playing, and their scoreboard.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Player {
    pub steamid: Option<String>,
    pub name: Option<String>,
    pub team: Option<String>,
    pub state: PlayerState,
    pub match_stats: MatchStats,
    /// Dota 2 puts the scoreboard directly on the player block.
    pub kills: Option<u32>,
    pub deaths: Option<u32>,
    pub assists: Option<u32>,
    pub gold: Option<u32>,
    pub net_worth: Option<u32>,
}

impl Player {
    /// Kills this player has, from whichever block holds them. `None` when the document does not
    /// say, which differs from zero: a missing block must not look like a score reset.
    pub fn kills(&self) -> Option<u32> {
        self.match_stats.kills.or(self.kills)
    }

    pub fn deaths(&self) -> Option<u32> {
        self.match_stats.deaths.or(self.deaths)
    }

    pub fn assists(&self) -> Option<u32> {
        self.match_stats.assists.or(self.assists)
    }
}

/// `player.state` — Counter-Strike's live per-round state.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PlayerState {
    pub health: Option<i32>,
    pub armor: Option<i32>,
    pub round_kills: Option<u32>,
    pub round_killhs: Option<u32>,
}

/// `player.match_stats` — Counter-Strike's per-match totals.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct MatchStats {
    pub kills: Option<u32>,
    pub assists: Option<u32>,
    pub deaths: Option<u32>,
    pub mvps: Option<u32>,
    pub score: Option<u32>,
}

/// `hero` — Dota 2's hero block, kept for the payload's detail.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Hero {
    pub id: Option<u32>,
    pub name: String,
    pub level: Option<u32>,
    pub alive: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CS2_COMPETITIVE: &str = include_str!("../../tests/fixtures/gsi_cs2_competitive.json");
    const CS2_MENU: &str = include_str!("../../tests/fixtures/gsi_cs2_menu.json");
    const DOTA_IN_GAME: &str = include_str!("../../tests/fixtures/gsi_dota_in_game.json");
    const DOTA_OBSERVER: &str = include_str!("../../tests/fixtures/gsi_dota_observer.json");

    #[test]
    fn parses_a_counter_strike_competitive_match() {
        let gsi = parse(CS2_COMPETITIVE.as_bytes()).expect("the fixture parses");
        assert_eq!(gsi.provider.appid, 730);
        assert_eq!(gsi.provider.name, "Counter-Strike: Global Offensive");
        assert_eq!(gsi.map.name, "de_dust2");
        assert_eq!(gsi.map.phase, "live");
        assert_eq!(gsi.map.round, 5);
        assert_eq!(gsi.map.team_ct.score, 3);
        assert_eq!(gsi.map.team_t.score, 1);
        assert_eq!(gsi.round.phase, "live");
        assert_eq!(gsi.bomb.state, "planted");
        assert_eq!(
            gsi.player.first().unwrap().steamid.as_deref(),
            Some("76561198000000001")
        );
        assert_eq!(gsi.player.first().unwrap().kills(), Some(12));
        assert_eq!(gsi.player.first().unwrap().deaths(), Some(5));
        assert_eq!(gsi.player.first().unwrap().assists(), Some(3));
    }

    #[test]
    fn parses_the_main_menu_where_nothing_is_subscribed() {
        let gsi = parse(CS2_MENU.as_bytes()).expect("the fixture parses");
        assert_eq!(gsi.map.name, "");
        assert_eq!(gsi.map.phase, "");
        assert!(gsi.player.first().is_none());
        assert_eq!(gsi.player, PlayerBlock::Missing);
    }

    #[test]
    fn parses_a_dota_match_whose_scoreboard_sits_on_the_player_block() {
        let gsi = parse(DOTA_IN_GAME.as_bytes()).expect("the fixture parses");
        assert_eq!(gsi.provider.appid, 570);
        assert_eq!(gsi.map.game_state, "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS");
        assert_eq!(gsi.map.game_time, 1240);
        let player = gsi.player.first().expect("a single player block");
        assert_eq!(
            player.kills(),
            Some(4),
            "Dota puts the scoreboard on `player`"
        );
        assert_eq!(player.deaths(), Some(2));
        assert_eq!(player.assists(), Some(6));
        assert_eq!(gsi.hero.name, "npc_dota_hero_queenofpain");
    }

    #[test]
    fn accepts_an_observer_payload_whose_player_block_is_an_array() {
        let gsi = parse(DOTA_OBSERVER.as_bytes()).expect("the fixture parses");
        match &gsi.player {
            PlayerBlock::Many(players) => assert_eq!(players.len(), 2),
            other => panic!("expected the observer shape, got {other:?}"),
        }
        assert_eq!(
            gsi.player.first().unwrap().steamid.as_deref(),
            Some("76561198000000009")
        );
    }

    #[test]
    fn a_missing_field_is_not_a_parse_error_and_a_wrong_type_is() {
        // Every block optional: the smallest document the game could send.
        let minimal = parse(br#"{"provider":{"appid":730,"name":"cs"}}"#).unwrap();
        assert_eq!(minimal.map.name, "");
        assert_eq!(minimal.player, PlayerBlock::Missing);

        // A wrong type is an error, not a silently zeroed field: a number where the score should be,
        // and a block that is neither object nor array.
        assert!(parse(br#"{"map":{"round":"five"}}"#).is_err());
        assert!(parse(br#"{"player":"nobody"}"#).is_err());
        assert!(parse(b"not json").is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn the_auth_block_is_an_arbitrary_string_map() {
        let gsi = parse(br#"{"auth":{"token":"abc","kind":"local"}}"#).unwrap();
        assert_eq!(gsi.auth.get("token").map(String::as_str), Some("abc"));
        assert_eq!(gsi.auth.get("kind").map(String::as_str), Some("local"));
        assert_eq!(gsi.auth.get("nothing").map(String::as_str), None);
    }

    #[test]
    fn a_kill_count_that_is_absent_is_not_zero() {
        // The tri-state matters: a payload without `player_match_stats` must not look like a score
        // reset.
        let player = Player::default();
        assert_eq!(player.kills(), None);
        assert_eq!(player.deaths(), None);
        assert_eq!(player.assists(), None);

        let cs2 = Player {
            match_stats: MatchStats {
                kills: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(cs2.kills(), Some(0), "a real zero is a value");
    }

    #[test]
    fn the_auth_block_is_read_without_the_rest_of_the_document() {
        // The gate reads the block and stops: the rest of the body is not parsed, so a
        // document that is broken *after* the block still yields its token. The full parse
        // is what rejects it, later.
        assert_eq!(
            auth_token(br#"{"auth":{"token":"abc"},"map":{"round":"five"}}"#),
            AuthToken::Present("abc".to_string())
        );
        assert_eq!(
            auth_token(br#"{"auth":{"token":"abc"},"map":"#),
            AuthToken::Present("abc".to_string())
        );
    }

    #[test]
    fn a_document_without_a_token_says_so() {
        assert_eq!(
            auth_token(br#"{"provider":{"appid":730}}"#),
            AuthToken::Absent
        );
        assert_eq!(auth_token(br#"{"auth":{"kind":"cs2"}}"#), AuthToken::Absent);
        assert_eq!(auth_token(b"{}"), AuthToken::Absent);
    }

    #[test]
    fn a_body_that_is_not_a_document_has_no_token() {
        // The reason is not carried: the body is attacker-controlled until the token has
        // been accepted, and a parser's message can quote it.
        for body in [
            &b"not json at all"[..],
            &b"["[..],
            &b""[..],
            &b"{\"auth\":"[..],
            &b"{\"auth\":{\"token\":1}}"[..],
        ] {
            assert_eq!(auth_token(body), AuthToken::Unreadable, "got for {body:?}");
        }
    }

    /// A realistic payload: `auth` is the last key, after a scoreboard of the requested size.
    fn payload_with_auth_last(filler_bytes: usize, token: &str) -> Vec<u8> {
        let mut body =
            String::from(r#"{"provider":{"name":"Counter-Strike 2","appid":730},"allplayers":{"#);
        let mut n = 0;
        while body.len() < filler_bytes {
            body.push_str(&format!(
                r#""7656119800000{n:04}":{{"name":"p{n}","state":{{"health":100}}}},"#
            ));
            n += 1;
        }
        body.push_str(&format!(r#""x":{{}}}},"auth":{{"token":"{token}"}}}}"#));
        body.into_bytes()
    }

    #[test]
    fn the_token_is_found_when_auth_is_the_last_key_however_large_the_body() {
        // The probe used to stop at 64 KiB, so a payload whose `auth` block sat behind a large
        // scoreboard (where the game puts it) was answered 400 and never heard.
        for size in [
            200,
            60 * 1024,
            64 * 1024 + 10,
            300 * 1024,
            1024 * 1024 + 512 * 1024,
        ] {
            let body = payload_with_auth_last(size, "the-token");
            assert!(body.len() >= size);
            assert_eq!(
                auth_token(&body),
                AuthToken::Present("the-token".to_string()),
                "a {}-byte body with auth last",
                body.len()
            );
        }
        // And the same body with no auth block is still a document with no token.
        assert_eq!(
            auth_token(br#"{"provider":{"appid":730},"player":{"name":"x"}}"#),
            AuthToken::Absent
        );
    }

    #[test]
    fn the_hint_sees_a_token_at_either_end_and_only_the_exact_token() {
        let token = "0123456789abcdef0123456789abcdef"; // gitleaks:allow
                                                        // First key (small body), and last key behind a large body.
        assert!(carries_token(
            format!(r#"{{"auth":{{"token":"{token}"}},"map":{{}}}}"#).as_bytes(),
            token
        ));
        assert!(carries_token(
            &payload_with_auth_last(500 * 1024, token),
            token
        ));
        // Spacing the game might use.
        assert!(carries_token(
            format!("{{\n  \"auth\": {{\n    \"token\" : \"{token}\"\n  }}\n}}").as_bytes(),
            token
        ));
        // Not the token: wrong, a prefix, an extension, empty, absent, and the token in the
        // middle of a large body where the hint does not look.
        for wrong in [
            "0123456789abcdef0123456789abcdee",
            &token[..31],
            "0123456789abcdef0123456789abcdef0",
            "",
        ] {
            assert!(
                !carries_token(
                    format!(r#"{{"auth":{{"token":"{wrong}"}}}}"#).as_bytes(),
                    token
                ),
                "{wrong:?}"
            );
        }
        assert!(!carries_token(b"{}", token));
        assert!(!carries_token(b"not json", token));
        assert!(!carries_token(
            format!(r#"{{"auth":{{"token":"{token}"}}}}"#).as_bytes(),
            ""
        ));
        let mut middle = vec![b' '; 100 * 1024];
        let block = format!(r#""token":"{token}""#);
        middle[50 * 1024..50 * 1024 + block.len()].copy_from_slice(block.as_bytes());
        assert!(!carries_token(&middle, token));
        // The token as a *key* or in another field is not a `"token": "<value>"` pair.
        assert!(!carries_token(
            format!(r#"{{"note":"{token}"}}"#).as_bytes(),
            token
        ));
    }
}
