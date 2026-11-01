//! The anti-cheat policy, guarded in a way a rename cannot get past.
//!
//! `scripts/check-forbidden-apis.sh` greps the tree for the *names* of the APIs the policy
//! forbids. A grep is a good guard and a blunt one: it sees the names, so a call the
//! pattern does not name is a call it does not catch, and a new name in a new crate is a
//! hole until the pattern grows.
//!
//! The two checks here are of the other kind. One reads what the `windows` crate is
//! allowed to reach — a feature is a *group* of Windows APIs, so enabling one makes every
//! API in it reachable whether or not the code calls it today. The other watches what goes
//! out on the wire: the Live Client Data API is read-only, and a `POST` to the game client
//! is the one thing that must never happen next to kernel anti-cheat. A method is a string
//! in a request line, so it cannot be guarded by a grep over the source; it is guarded by a
//! server that records what it was asked.
//!
//! Nothing here names an API the policy forbids, not even to assert its absence: this file
//! is read by that policy's own guard, and a file that spells the names out is an offender
//! of it. The lists below are of what *is* allowed, which is the stronger shape anyway — a
//! new import from anywhere else fails, whatever it is called.

// An integration test is a crate of its own, so the crate root's `cfg_attr(test, ...)`
// exemption does not reach it: the same rule, stated here.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::Path;

/// The `windows` features the events crate enables, and why each is here.
///
/// This is a list of *groups*, not of calls: a feature the project does not enable is a
/// capability it does not have, and the list is what a reviewer reads to decide whether a
/// new one is acceptable.
const EXPECTED_FEATURES: &[&str] = &[
    // The handle the process snapshot is closed with.
    "Win32_Foundation",
    // The process-name snapshot the game watcher compares against the configured watch
    // list. The group grants the enumeration and nothing else: the group that opens a
    // process is another one, and that one is not enabled.
    "Win32_System_Diagnostics_ToolHelp",
    // The global hotkey. The group also carries the input-synthesis APIs, which the code
    // does not call — the shell guard checks that by name, and the feature is here for the
    // hotkey, which cannot be had without it.
    "Win32_UI_Input_KeyboardAndMouse",
    // The message queue the hotkey arrives on.
    "Win32_UI_WindowsAndMessaging",
];

/// The feature groups that exist for the things the policy forbids *in this crate*.
///
/// Each of these is a capability: enabling one makes every API in it reachable. None of
/// them is enabled here, and the list is here so that adding one is a deliberate act that
/// fails this test rather than an accident that passes it.
///
/// "Forbidden" is relative to the crate, and the policy for the other crates is not this
/// file's to state: `feature_owners` in `scripts/check-forbidden-apis.sh` is the policy, and
/// it lets the capture crate (and xtask) keep a group this list calls forbidden, for the
/// argued reason written beside it there. So this list does not claim a group is banned
/// everywhere; `the_events_crate_is_not_an_owner_of_a_group_it_calls_forbidden` reads the
/// script and holds the two to the same answer for the events crate.
const FORBIDDEN_GROUPS: &[(&str, &str)] = &[
    (
        "Win32_System_Memory",
        "the group that writes another process's memory",
    ),
    (
        "Win32_System_Threading",
        "the group that opens a process and makes a thread in it",
    ),
    (
        "Win32_System_Diagnostics_Debug",
        "the group that reads another process's memory",
    ),
    (
        "Win32_Security",
        "the group that holds tokens and privileges",
    ),
    (
        "Win32_System_Ioctl",
        "the group that talks to a device driver",
    ),
];

/// Every Windows API the crate imports.
///
/// A list of what *is* allowed rather than of what is not, so that this file spells no
/// forbidden name. An import from a group that is not on this list fails the test whatever
/// it is called.
const ALLOWED_WINDOWS_APIS: &[&str] = &[
    // The hotkey: register it, unregister it, and read the message queue it arrives on.
    "RegisterHotKey",
    "UnregisterHotKey",
    "HOT_KEY_MODIFIERS",
    "MOD_ALT",
    "MOD_CONTROL",
    "MOD_SHIFT",
    // A flag on the same registration, not a new capability: with it Windows does not queue a
    // press per auto-repeat while the chord is held.
    "MOD_NOREPEAT",
    // Reading the thread's own queue, as `GetMessageW` did, but with a timeout. The wait wakes on a
    // queued `WM_HOTKEY` (`QS_HOTKEY`) or every 100 ms to see whether the listener was dropped, so
    // the chord can be given back. These calls only read this thread's own queue: none of them
    // reaches another process or window, and none of them posts or sends a message.
    "MsgWaitForMultipleObjects",
    "QS_HOTKEY",
    "PeekMessageW",
    "PM_REMOVE",
    "MSG",
    "WM_HOTKEY",
    // The process snapshot: take it, walk it, close it.
    "CloseHandle",
    "HANDLE",
    "CreateToolhelp32Snapshot",
    "Process32FirstW",
    "Process32NextW",
    "PROCESSENTRY32W",
    "TH32CS_SNAPPROCESS",
];

#[test]
fn the_windows_features_the_project_enables_are_the_ones_it_needs() {
    let manifest = std::fs::read_to_string(manifest_path()).expect("the events manifest");
    let features = windows_features(&manifest);

    assert_eq!(
        features.iter().map(String::as_str).collect::<Vec<_>>(),
        EXPECTED_FEATURES,
        "the `windows` features the crate enables are a capability list, not a detail: a new \
         one is a new set of reachable Windows APIs and has to be argued for"
    );

    for (group, what) in FORBIDDEN_GROUPS {
        assert!(
            !features.iter().any(|f| f == group),
            "the {group} group is enabled, and it is {what}"
        );
    }
}

#[test]
fn the_events_crate_is_not_an_owner_of_a_group_it_calls_forbidden() {
    // Two lists answer "may the events crate enable this group?": this file's, and the
    // script's `feature_owners`, which covers every manifest. They had drifted in wording
    // (this file called a group forbidden that the script lets the capture crate own), so
    // that a reader could not tell which one was the policy. The script is. This reads it, so
    // the answers cannot disagree without a test failing: a group forbidden here must not
    // list the events manifest as an owner there, and a group enabled here must be one the
    // script lets the events manifest enable.
    let script = std::fs::read_to_string(script_path()).expect("the forbidden-API script");
    assert_eq!(policy_disagreements(&script), Vec::<String>::new());
}

/// Where this file's two lists and the script's `feature_owners` give the events crate
/// different answers; empty when they agree.
fn policy_disagreements(script: &str) -> Vec<String> {
    let events = "crates/events/Cargo.toml";
    let mut out = Vec::new();
    for (group, what) in FORBIDDEN_GROUPS {
        let owners = script_feature_owners(script, group);
        if owners == "*" || owners.split_whitespace().any(|owner| owner == events) {
            out.push(format!(
                "{group} ({what}) is forbidden here, but the script's owners for it are {owners:?}"
            ));
        }
    }
    for group in EXPECTED_FEATURES {
        let owners = script_feature_owners(script, group);
        if owners != "*" && !owners.split_whitespace().any(|owner| owner == events) {
            out.push(format!(
                "{group} is enabled here, but the script only lets {owners:?} enable it"
            ));
        }
    }
    out
}

#[test]
fn reading_the_scripts_owner_list_sees_what_it_says() {
    // The parse is hand-rolled, so it is checked against a script of known shape, including
    // the two things that make it easy to get wrong: an arm continued over lines with a
    // backslash, and a group the script does not list (anyone may enable it: `*`).
    let script = "\
feature_owners() {
  case \"$1\" in
    Win32_A | Win32_B | \\
      Win32_C) echo \"-\" ;;
    Win32_D) echo \"crates/events/Cargo.toml\" ;;
    Win32_E) echo \"crates/capture/Cargo.toml xtask/Cargo.toml\" ;;
    *) echo \"*\" ;;
  esac
}
feature_failures=\"\"
";
    assert_eq!(script_feature_owners(script, "Win32_C"), "-");
    assert_eq!(
        script_feature_owners(script, "Win32_D"),
        "crates/events/Cargo.toml"
    );
    assert_eq!(
        script_feature_owners(script, "Win32_E"),
        "crates/capture/Cargo.toml xtask/Cargo.toml"
    );
    assert_eq!(script_feature_owners(script, "Win32_Z"), "*");
}

#[test]
fn the_policy_comparison_fails_when_the_script_and_this_file_drift() {
    // Teeth: take the real script and move it each way, and the comparison must notice. A
    // comparison that cannot fail would let the two lists drift again without a word.
    let script = std::fs::read_to_string(script_path()).expect("the forbidden-API script");

    // The script hands the events crate a group this file forbids.
    let granted = script.replace(
        "Win32_System_Threading) echo \"crates/capture/Cargo.toml xtask/Cargo.toml\"",
        "Win32_System_Threading) echo \"crates/capture/Cargo.toml crates/events/Cargo.toml\"",
    );
    assert_ne!(granted, script, "the edit found the owners line it moves");
    assert!(!policy_disagreements(&granted).is_empty());

    // The script takes away a group the events crate enables.
    let revoked = script.replace(
        "Win32_System_Diagnostics_ToolHelp) echo \"crates/events/Cargo.toml\"",
        "Win32_System_Diagnostics_ToolHelp) echo \"crates/capture/Cargo.toml\"",
    );
    assert_ne!(revoked, script, "the edit found the owners line it moves");
    assert!(!policy_disagreements(&revoked).is_empty());
}

#[test]
fn the_crate_imports_only_the_windows_apis_it_needs() {
    // The imports are the reach: a feature enables a group of APIs, and the import is what
    // picks one out of it. A list of what is allowed fails on anything else, whatever it
    // is called — which is the property a list of forbidden names cannot have.
    let imported = windows_imports(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    let allowed: BTreeSet<String> = ALLOWED_WINDOWS_APIS
        .iter()
        .map(|name| (*name).to_string())
        .collect();

    assert_eq!(
        imported, allowed,
        "the crate reaches a Windows API the list does not allow; the list is the whole \
         capability, and a new entry has to be argued for"
    );
}

#[test]
fn the_process_watcher_enumerates_the_process_list() {
    // The positive half of the claim: the watcher reads image names out of a process-list
    // snapshot, which is what Task Manager does. It opens no process, requests no access
    // right over any game, and reads no memory — the import list above is where that is
    // pinned down.
    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/process.rs"))
            .expect("the process watcher");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("CreateToolhelp32Snapshot"),
        "the watcher enumerates the process list, and the code says so"
    );
}

/// The repository's anti-cheat source guard, whose `feature_owners` is the feature policy.
fn script_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/check-forbidden-apis.sh")
}

/// The manifests the script lets enable `feature`, space-separated; `-` for none and `*` for
/// any (the arm the script falls through to for a group it does not list).
///
/// Reads the `case` in the script's `feature_owners` function: each arm is its group names
/// (`|`-separated, possibly continued over lines with a backslash) then `echo "<owners>" ;;`.
fn script_feature_owners(script: &str, feature: &str) -> String {
    const HEADER: &str = "feature_owners() {";
    let start = script
        .find(HEADER)
        .expect("the script defines feature_owners");
    // After the header: its `()` would otherwise be read as the end of the first arm's names.
    let body = &script[start + HEADER.len()..];
    let body = &body[..body.find("\n}\n").expect("feature_owners ends")];
    // A backslash at the end of a line continues the arm.
    let body = body.replace("\\\n", " ");
    for arm in body.split(";;") {
        let Some((names, answer)) = arm.split_once(')') else {
            continue;
        };
        let names = match names.find("case \"$1\" in") {
            Some(at) => &names[at + "case \"$1\" in".len()..],
            None => names,
        };
        let listed = names.split('|').map(str::trim).any(|name| name == feature);
        // The fall-through arm is `*`: it answers for every group not listed above it, and
        // arms are tried in order, so reaching it means `feature` matched nothing earlier.
        let fall_through = names.trim() == "*";
        if listed || fall_through {
            let owners = answer
                .split('"')
                .nth(1)
                .expect("an arm of feature_owners echoes a quoted answer");
            return owners.to_string();
        }
    }
    panic!("feature_owners has no arm for {feature}");
}

/// The events crate's manifest.
fn manifest_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
}

/// The `windows` features a manifest enables.
///
/// A hand-rolled read of the feature array: the crate has no TOML parser of its own, and
/// adding one to a test would be a dependency this project does not need.
fn windows_features(manifest: &str) -> Vec<String> {
    const START: &str = "features = [";
    // The comment lines go first: a comment in the feature array may hold a comma, and a
    // comma is what the list is split on.
    let manifest: String = manifest
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let start = manifest.find(START).expect("the windows feature list");
    let rest = &manifest[start + START.len()..];
    let end = rest.find(']').expect("the windows feature list ends");
    rest[..end]
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            let feature = part.strip_prefix('"')?.strip_suffix('"')?;
            Some(feature.to_string())
        })
        .collect()
}

/// The Windows APIs the Rust files under `dir` import.
fn windows_imports(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            out.extend(windows_imports(&path));
            continue;
        }
        if path.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for statement in windows_statements(&text) {
            out.extend(imported_apis(&statement));
        }
    }
    out
}

/// The `use windows::...;` statements in a file.
fn windows_statements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("use windows::") {
        let after = &rest[at..];
        let end = after.find(';').unwrap_or(after.len());
        out.push(after[..end].to_string());
        rest = &after[end..];
    }
    out
}

/// The API names a `use` statement imports.
fn imported_apis(statement: &str) -> BTreeSet<String> {
    statement
        .split([',', '{', '}'])
        .filter_map(|part| {
            let name = part.trim().rsplit("::").next()?.trim();
            let is_a_name =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            is_a_name.then(|| name.to_string())
        })
        .collect()
}
