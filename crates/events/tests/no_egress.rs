//! Egress, enforced by inspecting the source tree.
//!
//! Static checks rather than runtime hopes:
//!
//! * no capability that could reach another machine, outside the files that are allowed to
//!   have one and say why ([`no_capability_to_reach_off_this_machine_is_used_outside_its_exemptions`]);
//! * no HTTP client in any manifest, and none in any dependency graph but the one that is
//!   allowed to have it ([`no_http_client_is_a_dependency`]);
//! * TLS certificate verification is relaxed in exactly one file, the one that pins the
//!   loopback address ([`certificate_verification_is_pinned_and_only_the_hostname_check_is_relaxed`]);
//! * every `TcpListener::bind` is pinned to loopback, and the *address* is read rather than
//!   the file ([`every_listener_is_bound_to_loopback`]);
//! * the tree's outbound sockets are exactly the four files that may have them
//!   ([`only_the_loopback_modules_talk_to_a_socket_at_all`]).
//!
//! The capability check is a denylist of *names* rather than an allowlist of constructors,
//! because an allowlist of constructors is only as good as the list: `reqwest::Client` is
//! one spelling of the reqwest client and `reqwest::get`, `reqwest::ClientBuilder::new` and
//! a dozen others are the others. A capability is a crate, a program or a standard-library
//! item, and those are few enough to list.
//!
//! Each exemption is by path and carries its reason. Every check reads code with comment
//! lines removed first, otherwise this module's own prose ("never bind `0.0.0.0`") would
//! satisfy the checks it asserts. Block comments are not stripped; the tree uses none, so one
//! would show up as an offender rather than a silent pass.
//!
//! # The one exception
//!
//! The application is not "zero egress, ever". It is **no egress except one user-initiated
//! update check**: the "Check for updates" button in the settings panel asks one fixed
//! GitHub URL whether there is a newer release, and the artifact it is offered is verified
//! against the minisign public key pinned in `apps/desktop/src-tauri/tauri.conf.json`
//! before it is installed.
//!
//! That exception is narrow in three ways, and each of them is a check here:
//!
//! - the *source* denylist above is unchanged — no file in this tree gained a client, a
//!   socket or a name resolution. The plugin's own code is not this project's code;
//! - the *manifest* check still fails on any direct HTTP-client dependency, and the
//!   *graph* check now fails on any resolved graph that contains one, with a single
//!   exemption: the desktop shell's, and only because the updater plugin is in it;
//! - the loopback bind check is untouched. The updater's requests leave the machine; they
//!   are not listeners, and nothing was loosened to let them out.

// The test bodies use `unwrap()` throughout; production code does not.
#![allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks)]
use std::collections::BTreeMap;
use std::path::Path;

/// A capability that would let the application reach another machine.
///
/// `whole_word` is for the names that are also words: `surf` is a crate and `surface` is a
/// word, and a guard that cannot tell them apart is a guard nobody reads.
const CAPABILITIES: &[(&str, bool, &str)] = &[
    ("reqwest", false, "the reqwest HTTP client"),
    ("ureq", false, "the ureq HTTP client"),
    ("hyper", false, "the hyper HTTP client"),
    ("hyper_util", false, "the hyper client's runtime"),
    ("isahc", false, "the isahc HTTP client"),
    ("attohttpc", false, "the attohttpc HTTP client"),
    ("surf", true, "the surf HTTP client"),
    ("curl", false, "the curl program or crate"),
    ("wget", false, "the wget program"),
    ("Curl", false, "the libcurl C API"),
    ("TcpStream", false, "a socket"),
    ("UdpSocket", false, "a datagram socket"),
    ("to_socket_addrs", false, "a name resolution"),
    ("lookup_host", false, "a name resolution"),
    ("getaddrinfo", false, "a name resolution"),
];

/// The files that may hold a capability, and why.
///
/// A file that is not on this list and holds one fails the build. There is no baseline and
/// no "known findings" list: this is a list of *files*, and a new file has to argue for
/// itself.
const EXEMPT: &[(&str, &str)] = &[
    (
        "crates/events/src/lol",
        "the League client, the one place allowed to build an HTTP client: it is pinned to \
         an Endpoint, which can only be built for a loopback address",
    ),
    (
        "crates/events/tests/lol_mock",
        "the mock server the League tests drive, on loopback",
    ),
    (
        "crates/events/src/gsi.rs",
        "the GSI listener: it accepts connections on loopback and never connects out, which \
         the exact allowlist below proves",
    ),
    (
        "crates/events/tests/gsi_listener",
        "the test that drives the loopback listener",
    ),
    (
        "crates/events/tests/gsi_routing",
        "the test that drives the loopback listener's per-game routes",
    ),
    (
        "crates/encoder/src/ffmpeg.rs",
        "the encoder's audio input: ffmpeg dials a loopback listener to receive the PCM, \
         and the accepted peer is checked to be loopback",
    ),
    (
        "crates/encoder/src/audio_link.rs",
        "the encoder's audio link: loopback TCP on Windows, a Unix socket elsewhere. The peer's \
         process is authenticated against the ffmpeg child we spawned",
    ),
    (
        "crates/events/examples/gsi_demo",
        "an example that prints the command for a human to run; it is not part of the \
         application and makes no request of its own",
    ),
    (
        "xtask/src/sidecars.rs",
        "the development-time download: the file it fetches is hash-pinned in \
         xtask/sidecars.toml and checked immediately after, and xtask is not shipped",
    ),
    (
        "deny.toml",
        "the supply-chain configuration, which names the clients it forbids",
    ),
];

/// The clients that may not be depended on, directly or not.
///
/// `reqwest` and `hyper` are here and not in `deny.toml`: the desktop shell's own audit
/// cannot ban them, because `tauri` depends on `reqwest` itself. A *direct* dependency is a
/// different thing from a transitive one, and this is the check for it.
const BANNED_DEPENDENCIES: &[&str] = &[
    "reqwest",
    "ureq",
    "hyper",
    "isahc",
    "attohttpc",
    "surf",
    "curl",
];

/// The dependency graphs that may contain an HTTP client, and why.
///
/// **One entry, and it is the updater.** The desktop shell's graph holds `reqwest` because
/// `tauri` depends on it (which is why `deny.toml` cannot ban it) and because
/// `tauri-plugin-updater` does: the one thing in this tree that talks to the internet, and
/// it does so only when the user presses "Check for updates", it asks one fixed GitHub URL,
/// and every artifact it downloads is verified against the minisign public key pinned in
/// `apps/desktop/src-tauri/tauri.conf.json` before it is installed.
///
/// Every other graph — the core crates, the CLI, xtask — must stay free of HTTP clients.
/// The engine that records the screen does not talk to the internet, and a graph that
/// grew a client would be a capability nobody chose on purpose.
const GRAPH_EXEMPT: &[(&str, &str)] = &[(
    "apps/desktop/src-tauri/Cargo.lock",
    "the desktop shell, for the updater: the one thing here that talks to the internet, \
     and only when the user asks it to",
)];

#[test]
fn no_capability_to_reach_off_this_machine_is_used_outside_its_exemptions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders: Vec<(String, String)> = Vec::new();

    for entry in walk_source(&root) {
        let rel = relative(&root, &entry);
        if rel.contains("/target/") || rel.ends_with("no_egress.rs") {
            // The guard must not scan itself: this file holds the names it searches for.
            continue;
        }
        if let Some(reason) = exemption(&rel) {
            let _ = reason;
            continue;
        }
        let text = code_of(&entry);
        for (marker, whole_word, means) in CAPABILITIES {
            let found = if *whole_word {
                contains_word(&text, marker)
            } else {
                contains_identifier(&text, marker)
            };
            if found {
                offenders.push((rel.clone(), (*means).to_string()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a capability to reach another machine is used outside the files that may have one \
         ({offenders:?}); each of those needs a reason on the exemption list, and the reason \
         is what a reader of the code will believe"
    );
}

#[test]
fn no_http_client_is_a_dependency() {
    // The source-tree guard reads our code; a dependency is a capability nobody wrote. The
    // desktop shell's own audit (`deny.toml`) covers the graph behind the manifests; this
    // covers the manifests, where a client would be *chosen*.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();

    for entry in walk_source(&root) {
        if entry.extension().is_none_or(|x| x != "toml") {
            continue;
        }
        let rel = relative(&root, &entry);
        if rel == "deny.toml" {
            // The supply-chain configuration names the clients it forbids.
            continue;
        }
        for line in code_of(&entry).lines() {
            let line = line.trim();
            for banned in BANNED_DEPENDENCIES {
                if let Some(rest) = line.strip_prefix(banned) {
                    if rest.starts_with(|c: char| c == '=' || c.is_whitespace()) {
                        offenders.push(format!("{rel}: {line}"));
                    }
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "an HTTP client is a direct dependency ({offenders:?}); the League module builds its \
         own client, and nothing else may"
    );

    // The *graph*, not just the manifests: a client can arrive without any manifest
    // naming it, and the one that does arrive that way is the updater. This is the half
    // that has to know about it, and it knows about exactly one graph.
    let mut graphs = Vec::new();
    for entry in walk_lock_files(&root) {
        let rel = relative(&root, &entry);
        let clients = http_clients_in_the_graph(&code_of(&entry));
        if clients.is_empty() {
            continue;
        }
        if exemption_in(GRAPH_EXEMPT, &rel).is_some() {
            continue;
        }
        graphs.push(format!("{rel}: {clients:?}"));
    }

    assert!(
        graphs.is_empty(),
        "a dependency graph holds an HTTP client ({graphs:?}); the only graph allowed to \
         hold one is the desktop shell's, because the updater plugin is in it — and the \
         updater talks to the internet only when the user asks it to"
    );
}

/// The HTTP clients a resolved dependency graph holds, and what pulled them in.
///
/// The packages a lock file names, and the packages each of them depends on: a client is
/// "in the graph" when it is a package the graph resolved, not only when a manifest named
/// it. The dependents are reported too, because "this graph holds a client" is a much
/// less useful failure message than "this graph holds a client, and these are what
/// brought it in".
fn http_clients_in_the_graph(lock: &str) -> Vec<(String, Vec<String>)> {
    let packages = locked_packages(lock);
    let mut out = Vec::new();
    for (name, _) in &packages {
        if !BANNED_DEPENDENCIES.contains(&name.as_str()) {
            continue;
        }
        let dependents: Vec<String> = packages
            .iter()
            .filter(|(_, dependencies)| {
                dependencies
                    .iter()
                    .any(|dependency| dependency.as_str() == name.as_str())
            })
            .map(|(dependent, _)| dependent.clone())
            .collect();
        out.push((name.clone(), dependents));
    }
    out
}

/// The packages a lock file names, and the packages each of them depends on.
///
/// The shape of a `Cargo.lock` is a list of `[[package]]` blocks, each with a `name` and
/// optionally a `dependencies` list whose entries are `"name"`, `"name version"` or
/// `"name version (source)"`. Only the name is wanted here.
fn locked_packages(lock: &str) -> Vec<(String, Vec<String>)> {
    lock.split("[[package]]")
        .skip(1)
        .map(|block| {
            let name = block
                .lines()
                .find_map(|line| line.trim().strip_prefix("name = "))
                .map(|value| unquote(value.trim()).to_string())
                .unwrap_or_default();
            (name, locked_dependencies(block))
        })
        .collect()
}

/// The packages one `[[package]]` block depends on, by name.
fn locked_dependencies(block: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in block.lines() {
        let line = line.trim();
        if line == "dependencies = []" {
            continue;
        }
        if line == "dependencies = [" {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if line == "]" {
            inside = false;
            continue;
        }
        let entry = line.trim().trim_end_matches(',').trim_matches('"');
        if let Some(name) = entry.split_whitespace().next() {
            out.push(name.to_string());
        }
    }
    out
}

/// A quoted value, without its quotes.
fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"')
}

#[test]
fn a_resolved_client_is_a_client_the_graph_has() {
    // The rule the dependency check applies, on lock files made for the purpose. A client
    // can arrive in a graph without any manifest naming it, so the check reads the graph;
    // this is the shape it reads.
    let with_client = "\
[[package]]
name = \"replayed-events\"
version = \"0.2.0\"
dependencies = [
 \"reqwest 0.13.5 (registry+https://github.com/rust-lang/crates.io-index)\",
]

[[package]]
name = \"reqwest\"
version = \"0.13.5\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
";
    assert_eq!(
        http_clients_in_the_graph(with_client),
        vec![("reqwest".to_string(), vec!["replayed-events".to_string()])],
        "a graph that resolved a client has one, whatever the manifest said, and the \
         package that brought it in is named"
    );

    let without_client = "\
[[package]]
name = \"replayed-events\"
version = \"0.2.0\"
dependencies = [
 \"serde 1.0.0\",
]

[[package]]
name = \"serde\"
version = \"1.0.0\"
dependencies = []
";
    assert!(
        http_clients_in_the_graph(without_client).is_empty(),
        "a graph with no client in it has none"
    );
}

#[test]
fn certificate_verification_is_pinned_and_only_the_hostname_check_is_relaxed() {
    // The League client trusts Riot's pinned root only, so no file may turn chain verification
    // off, and the one remaining relaxation (the host-name match, which an IP literal cannot
    // satisfy) lives in exactly one file: the one that pins the address.
    const THE_ONE_FILE: &str = "crates/events/src/lol/client.rs";
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut chain_offenders = Vec::new();
    let mut name_offenders = Vec::new();

    for entry in walk_source(&root) {
        let rel = relative(&root, &entry);
        if rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if text.contains("danger_accept_invalid_certs") {
            chain_offenders.push(rel.clone());
        }
        if text.contains("danger_accept_invalid_hostnames") {
            name_offenders.push(rel);
        }
    }

    assert!(
        chain_offenders.is_empty(),
        "nothing may skip certificate-chain verification; pin a root instead: {chain_offenders:?}"
    );
    assert_eq!(
        name_offenders,
        vec![THE_ONE_FILE.to_string()],
        "exactly one file may relax the host-name check, and it must be the loopback client"
    );

    // The client must really pin: platform roots off, Riot's root added.
    let client = code_of(&root.join(THE_ONE_FILE));
    assert!(
        client.contains("disable_built_in_roots(true)"),
        "platform roots must be disabled"
    );
    assert!(
        client.contains("add_root_certificate"),
        "the pinned root must be added"
    );

    // That file must also pin the address: the relaxation and the loopback endpoint are one
    // decision, not two.
    let client = code_of(&root.join(THE_ONE_FILE));
    assert!(
        client.contains("is_loopback"),
        "the file that relaxes verification must also be the file that checks loopback"
    );
    assert!(
        client.contains("Endpoint"),
        "and must address an Endpoint, which cannot be built for a non-loopback host"
    );
}

#[test]
fn every_listener_is_bound_to_loopback() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for entry in walk_source(&root) {
        let rel = relative(&root, &entry);
        if rel.contains("/target/") || rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if !text.contains("TcpListener::bind") {
            continue;
        }
        // The address the call binds, read off the source. A bind written as
        // `TcpListener::bind(&addr)` says nothing by itself — the address is in the binding
        // a few lines up — so the name is resolved before the address is judged. A file that
        // binds every interface and mentions `127.0.0.1` somewhere else is the mistake this
        // catches, and a file-level mention could never catch it.
        let addresses = bound_addresses(&text);
        assert!(
            !addresses.is_empty(),
            "{} binds a listener but no address could be read from it",
            entry.display()
        );
        for address in addresses {
            match address {
                Bound::Address(literal) => assert!(
                    is_loopback_address(&literal),
                    "{} binds {literal}, which is not loopback",
                    entry.display()
                ),
                Bound::Unknown => assert!(
                    text.contains("is_loopback"),
                    "{} binds an address the source does not name, and the file does not \
                     check that it is loopback",
                    entry.display()
                ),
            }
        }
        // The wildcards, as a backstop for the address the source does not name: the code
        // that ships may not contain them at all. A `#[cfg(test)]` module may: a test names
        // the addresses it refuses.
        let shipped = shipped_code(&text);
        for wildcard in ["0.0.0.0", "[::]", "UNSPECIFIED", "::ANY"] {
            assert!(
                !shipped.contains(wildcard),
                "{} names the wildcard address {wildcard} in code that ships",
                entry.display()
            );
        }
    }
}

#[test]
fn only_the_loopback_modules_talk_to_a_socket_at_all() {
    // The socket clients are the League module and the test mock. A new `TcpStream::connect`
    // elsewhere is a new outbound path and belongs in this list deliberately.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut connecting: Vec<String> = Vec::new();

    for entry in walk_source(&root) {
        let rel = relative(&root, &entry);
        if rel.contains("/target/") || rel.ends_with("no_egress.rs") {
            continue;
        }
        let text = code_of(&entry);
        if text.contains("TcpStream::connect") {
            connecting.push(rel);
        }
    }

    connecting.sort();
    assert_eq!(
        connecting,
        vec![
            // The listener's own unit tests dial its loopback socket (rate-limiter test).
            "crates/events/src/gsi.rs".to_string(),
            "crates/events/src/lol/client.rs".to_string(),
            "crates/events/tests/gsi_listener.rs".to_string(),
            "crates/events/tests/gsi_routing.rs".to_string(),
            "crates/events/tests/lol_mock.rs".to_string(),
        ],
        "the tree's outbound sockets are the League client and the tests that drive the \
         loopback mocks; anything else needs a reason and an exemption here"
    );
}

/// The address a `TcpListener::bind` call binds.
enum Bound {
    /// The address, read off a literal in the source.
    Address(String),
    /// The source does not say what the call binds: a parameter, a computed value.
    Unknown,
}

/// The addresses the `TcpListener::bind` calls in `code` bind.
///
/// The string literals a file binds are read first, so a name bound a few lines above the
/// call can be resolved; a call whose argument is neither a literal nor such a name is
/// `Unknown`, and the file has to pin loopback some other way for it to be allowed.
fn bound_addresses(code: &str) -> Vec<Bound> {
    let literals = string_bindings(code);
    let mut out = Vec::new();
    let mut rest = code;
    while let Some(at) = rest.find("TcpListener::bind(") {
        let after = &rest[at + "TcpListener::bind(".len()..];
        let argument = match matching_paren(after) {
            Some(end) => &after[..end],
            None => after,
        };
        out.push(resolve(argument, &literals));
        rest = after;
    }
    out
}

/// The `let name = "literal"` bindings in `code`, by name.
fn string_bindings(code: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in code.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("let ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        let name = name.trim().trim_end_matches(':').trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim();
        let Some(literal) = string_literal(value) else {
            continue;
        };
        out.insert(name.to_string(), literal);
    }
    out
}

/// The string literal a value is, if it is one: `"127.0.0.1:0"` or `"127.0.0.1:0".to_string()`.
fn string_literal(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches(';').trim();
    let value = value
        .strip_suffix(".to_string()")
        .or_else(|| value.strip_suffix(".to_owned()"))
        .or_else(|| value.strip_suffix(".as_str()"))
        .or_else(|| value.strip_suffix(".into()"))
        .unwrap_or(value)
        .trim();
    let literal = value.strip_prefix('"')?.strip_suffix('"')?;
    Some(literal.to_string())
}

/// The address a bind argument names, if the source says.
fn resolve(argument: &str, literals: &BTreeMap<String, String>) -> Bound {
    let argument = argument.trim();
    let argument = argument.strip_prefix('&').unwrap_or(argument).trim();
    if let Some(literal) = string_literal(argument) {
        return Bound::Address(literal);
    }
    let name = argument.trim_end_matches(';').trim();
    if let Some(literal) = literals.get(name) {
        return Bound::Address(literal.clone());
    }
    Bound::Unknown
}

/// The text up to the parenthesis that closes `text`, which starts inside an open one.
fn matching_paren(text: &str) -> Option<usize> {
    let mut depth = 1;
    for (at, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// The host part of an address literal: `127.0.0.1:45671` is `127.0.0.1`, `[::1]:45671` is
/// `::1`, and `:45671` is the empty host, which is every interface.
fn host_of(address: &str) -> &str {
    let address = address.trim();
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match address.split(':').count() {
        2 => address
            .split_once(':')
            .map(|(host, _)| host)
            .unwrap_or(address),
        _ => address,
    }
}

/// Whether an address literal names the loopback interface.
fn is_loopback_address(address: &str) -> bool {
    matches!(host_of(address), "127.0.0.1" | "::1")
}

/// The reason a path may hold a capability, if it may.
fn exemption(rel: &str) -> Option<&'static str> {
    exemption_in(EXEMPT, rel)
}

/// The reason a path may be on a list, if it may.
fn exemption_in(list: &[(&'static str, &'static str)], rel: &str) -> Option<&'static str> {
    list.iter().find_map(|(path, reason)| {
        let matches =
            rel == *path || (rel.starts_with(path) && rel[path.len()..].starts_with(['/', '.']));
        matches.then_some(*reason)
    })
}

/// Whether `haystack` holds `needle` at the start of an identifier: the character before it
/// is not part of a name. `curl` is found in `curl_easy_init` and in `Command::new("curl")`.
fn contains_identifier(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(found) = haystack[start..].find(needle) {
        let at = start + found;
        if at == 0 || !is_word_byte(bytes[at - 1]) {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Whether `haystack` holds `needle` as a whole word: the characters around it are not part
/// of a name. `surf` is a crate; `surface` is a word.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(found) = haystack[start..].find(needle) {
        let at = start + found;
        let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Whether a byte is part of an identifier.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// A file's code with its comment lines removed (doc comments included): the guards are
/// about what the code does, not what it says about itself.
///
/// `//` is a comment in every language the tree is written in. `#` is a comment in the
/// manifests and the scripts, but an attribute in Rust, so it is only stripped there for
/// the files that are not Rust.
fn code_of(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let rust = path.extension().is_some_and(|x| x == "rs");
    text.lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with("//") && (rust || !line.starts_with('#'))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A file's code with its `#[cfg(test)]` module removed.
///
/// A test may name the addresses it refuses — the listener's own test does — while the code
/// that ships may not.
fn shipped_code(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut in_test = false;
    for line in text.lines() {
        if !in_test {
            if line.trim() == "#[cfg(test)]" {
                in_test = true;
                depth = 0;
                continue;
            }
            out.push(line);
            continue;
        }
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        if depth <= 0 && line.contains('}') {
            in_test = false;
        }
    }
    out.join("\n")
}

/// A path relative to the tree root, with forward slashes.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The file extensions the tree's source is written in.
const EXTENSIONS: &[&str] = &[
    "rs", "toml", "sh", "ps1", "bat", "cmd", "ts", "svelte", "mjs", "js", "yml", "yaml",
];

/// Whether a path is one of the tree's source files.
fn is_source(path: &Path) -> bool {
    path.extension().is_some_and(|x| {
        EXTENSIONS
            .iter()
            .any(|e| e.eq_ignore_ascii_case(x.to_str().unwrap_or_default()))
    })
}

/// Whether a path is one of the tree's lock files.
fn is_lock_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "Cargo.lock")
}

/// The source files in the tree.
///
/// The frontend's dependencies are not walked: they are not this project's code, and a
/// dependency that can make a request is the manifest's business, not the source's.
fn walk_source(dir: &Path) -> Vec<std::path::PathBuf> {
    walk_tree(dir, &is_source)
}

/// The lock files in the tree.
///
/// They are not walked by [`walk_source`]: a lock file is not source, and the capability
/// denylist is about what this project's own code says, not what a resolved graph happens
/// to contain. The dependency check reads these, because a client can arrive in the graph
/// without any manifest naming it.
fn walk_lock_files(dir: &Path) -> Vec<std::path::PathBuf> {
    walk_tree(dir, &is_lock_file)
}

/// The files in the tree a predicate accepts.
fn walk_tree<F: Fn(&Path) -> bool>(dir: &Path, accept: &F) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        // Prune build output, dependencies and VCS metadata: `target/` holds the crates.io
        // registry cache, whose vendored sources would otherwise be walked on every run, and
        // `node_modules/` holds a dependency tree that is not this project's code.
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if p.is_dir()
            && (name == "target" || name == ".git" || name == "node_modules" || name == "dist")
        {
            continue;
        }
        if p.is_dir() {
            out.extend(walk_tree(&p, accept));
        } else if accept(&p) {
            out.push(p);
        }
    }
    out
}
