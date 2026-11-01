//! Sidecar acquisition: `xtask sidecars record` and `xtask sidecars fetch`.
//!
//! replayed ships no ffmpeg in the repository: the binaries are sidecars, downloaded at build
//! time and pinned by SHA-256 in `xtask/sidecars.toml`. The two halves of the pin are separate
//! commands:
//!
//! * `record` downloads each archive, hashes it and rewrites `sidecars.toml` in place. The hash
//!   is trust on first use: it attests nothing about who served the bytes, only that later
//!   fetches must match them or fail loudly. A human reviews the URL and hash in the diff; that
//!   review is the only trust anchor.
//! * `fetch` downloads, verifies the hash against the recorded one, and only then extracts the
//!   paths in the entry's `binaries` allowlist into `<repo>/binaries/`.
//!
//! Everything security-relevant happens here rather than in an external `unzip`/`tar`: the
//! traversal rules ([`validate_entry_path`]), destination rule ([`entry_destination`]) and
//! allowlist matching ([`scan`]) are ordinary tested Rust, with `zip` only as a decoder.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The placeholder `record` replaces. `fetch` refuses while it is still there.
pub const UNRECORDED: &str = "RECORD_ME";

/// Ceiling on one extracted file. A recorded hash pins an archive, not what it expands to, so a
/// decompression bomb inside an allowlisted entry would otherwise fill the disk. 2 GiB is an
/// order of magnitude over a current ffmpeg binary (~120 MB).
const MAX_EXTRACTED_FILE_BYTES: u64 = 2 << 30;

/// One `[[platform]]` entry of `xtask/sidecars.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub target: String,
    pub archive_url: String,
    pub sha256: String,
    #[serde(default)]
    pub archive_kind: Option<String>,
    #[serde(default)]
    pub binaries: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub platform: Vec<Entry>,
}

// ---------------------------------------------------------------------------------------
// Target selection
// ---------------------------------------------------------------------------------------

/// The platform triple this xtask binary was built for, in the spelling `sidecars.toml` uses.
/// `None` means a host with no name there; the caller must pass `--target`.
pub fn host_target() -> Option<String> {
    triple_for(std::env::consts::ARCH, std::env::consts::OS).map(str::to_string)
}

/// Testable core of [`host_target`].
pub fn triple_for(arch: &str, os: &str) -> Option<&'static str> {
    Some(match (arch, os) {
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        ("aarch64", "windows") => "aarch64-pc-windows-msvc",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        _ => return None,
    })
}

/// Pick the entry to fetch: an explicit `--target` wins, otherwise the host's.
fn select_entry<'a>(
    entries: &'a [Entry],
    requested: Option<&str>,
    host: Option<&str>,
) -> Result<&'a Entry> {
    let want = match (requested, host) {
        (Some(t), _) => t,
        (None, Some(h)) => h,
        (None, None) => bail!(
            "cannot tell which platform to fetch for: this host is not one of the triples \
             sidecars.toml knows. Pass --target <triple>, e.g. --target \
             x86_64-pc-windows-msvc"
        ),
    };
    entries.iter().find(|e| e.target == want).ok_or_else(|| {
        anyhow::anyhow!(
            "no sidecar entry for {}. sidecars.toml has: {}. Pass --target <triple> to fetch \
             another platform's sidecar (this is how a Windows sidecar is fetched from \
             macOS or Linux).",
            want,
            list_targets(entries)
        )
    })
}

fn list_targets(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return "no [[platform]] entries at all".to_string();
    }
    entries
        .iter()
        .map(|e| e.target.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------------------
// Manifest reading and rewriting
// ---------------------------------------------------------------------------------------

pub fn parse_manifest(text: &str) -> Result<Manifest> {
    toml::from_str(text).context("parsing xtask/sidecars.toml")
}

pub fn load_manifest(path: &Path) -> Result<Manifest> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_manifest(&text)
}

/// Rewrite the `sha256` value of each named target in a manifest's text, leaving every other
/// byte alone.
///
/// Line-based on purpose: `sidecars.toml` is hand-commented, and a TOML serialiser round-trip
/// would drop the comments. An update whose target is nowhere in the text is an error, so
/// `record` never looks like it recorded something it did not.
pub fn rewrite_sha256(text: &str, updates: &[(String, String)]) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut current: Option<String> = None;
    let mut done: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if let Some(v) = toml_string_value(trimmed, "target") {
            current = Some(v.to_string());
        }
        if let Some((key, _)) = trimmed.split_once('=') {
            if key.trim() == "sha256" {
                if let Some(target) = current.as_deref() {
                    if let Some((_, new)) = updates.iter().find(|(t, _)| t == target) {
                        out.push_str(&format!("{indent}sha256 = \"{new}\"\n"));
                        done.push(target.to_string());
                        continue;
                    }
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }

    for (target, _) in updates {
        if !done.contains(target) {
            bail!(
                "could not find a `sha256 = ...` line under `target = \"{target}\"` in the \
                 manifest; refusing to report success for a hash that was not written"
            );
        }
    }
    Ok(out)
}

/// `key = "value"` → `"value"`, for the simple string lines this file contains.
fn toml_string_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (k, v) = line.split_once('=')?;
    if k.trim() != key {
        return None;
    }
    let v = v.trim();
    v.strip_prefix('"')?.strip_suffix('"')
}

// ---------------------------------------------------------------------------------------
// Download and hashing
// ---------------------------------------------------------------------------------------

/// Largest archive a download may be, in bytes. The biggest sidecar archive is ~120 MB; this is
/// a ceiling against a redirect to something absurd, not a tight fit.
const MAX_DOWNLOAD_BYTES: &str = "536870912"; // 512 MiB

/// How long one download attempt may take, in seconds, so a stalled CDN fails a CI job in
/// minutes and not at the job's own timeout.
const MAX_DOWNLOAD_SECONDS: &str = "1200";

/// The transport flags every download uses (everything but the output file and the URL).
///
/// `validate_url` insists on `https` for the URL it is given, but `--location` follows redirects,
/// and curl follows one to **any** protocol unless told otherwise: a misconfigured host or CDN
/// could send the download to `http://`, or to something enormous. `--proto =https` and
/// `--proto-redir =https` refuse the downgrade at the transport (curl says "protocol not
/// supported" and fails, instead of the hash rejecting the content after the fact); TLS 1.2 is the
/// floor; the timeouts and the size cap bound a hang or a disk fill. `--max-filesize` is only
/// enforced when the server states a length, which is why the hash check stays the real pin.
const CURL_ARGS: &[&str] = &[
    "--fail",
    "--location",
    "--silent",
    "--show-error",
    "--retry",
    "3",
    "--proto",
    "=https",
    "--proto-redir",
    "=https",
    "--tlsv1.2",
    "--connect-timeout",
    "30",
    "--max-time",
    MAX_DOWNLOAD_SECONDS,
    "--max-filesize",
    MAX_DOWNLOAD_BYTES,
];

/// Download `url` to `dest` with `curl`.
///
/// `curl` rather than an HTTP crate: an HTTP client with TLS would add a certificate stack and
/// a C build dependency to fetch one file, and `curl` ships with Windows 10 1803+, macOS and
/// Linux. The recorded hash is the pin, checked immediately after, so the transport is not
/// trusted.
pub fn download(url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let out = Command::new("curl")
        .args(CURL_ARGS)
        .arg("--output")
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "`curl` is not on PATH, and xtask uses it to download sidecars. Install \
                     curl (it ships with Windows 10 1803+, macOS and most Linux systems) and \
                     retry."
                )
            } else {
                anyhow::Error::new(e).context("running curl")
            }
        })?;
    if !out.status.success() {
        bail!(
            "curl failed with {} while downloading {url}:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let size = std::fs::metadata(dest)
        .with_context(|| format!("stat {}", dest.display()))?
        .len();
    if size == 0 {
        bail!("curl reported success but {url} produced a zero-byte file");
    }
    Ok(())
}

/// SHA-256 of a file, as lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Check a file against an expected lowercase-hex SHA-256, returning the actual digest.
/// Nothing downstream may run before this succeeds: `fetch` verifies the whole archive before
/// writing a single entry.
pub fn verify_sha256(path: &Path, expected_hex: &str) -> Result<String> {
    let expected = expected_hex.trim();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "sidecars.toml records {expected:?} as the sha256, which is not a 64-character \
             hex digest. Run `cargo xtask sidecars record` on a trusted network to record one."
        );
    }
    let actual = sha256_file(path)?;
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!(
            "checksum mismatch for {}: expected sha256 {}, got {}. Nothing was extracted. If \
             the upstream archive legitimately changed, re-run `cargo xtask sidecars record` \
             on a trusted network and review the diff before committing the new hash.",
            path.display(),
            expected.to_ascii_lowercase(),
            actual
        );
    }
    Ok(actual)
}

// ---------------------------------------------------------------------------------------
// The traversal rules
// ---------------------------------------------------------------------------------------

/// Refuse an archive entry name that could escape the destination directory.
///
/// Zip entry names are attacker-controlled. The rules are refusals, never rewrites: an archive
/// carrying `../escape.txt`, `/etc/passwd` or `..\..\x` is hostile, and silently sanitising it
/// would hide the event worth knowing about. A trailing `/` (zip's spelling of a directory) is
/// allowed; an empty component anywhere else is not.
pub fn validate_entry_path(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("archive entry has an empty name");
    }
    if name.contains('\0') {
        bail!("archive entry name contains a NUL byte: {name:?}");
    }
    if name.contains('\\') {
        bail!(
            "archive entry name contains a backslash, which zip does not use as a separator \
             (it would be read as a path separator only on Windows): {name:?}"
        );
    }
    if name.starts_with('/') {
        bail!("archive entry name is an absolute path: {name:?}");
    }
    if has_drive_prefix(name) {
        bail!("archive entry name starts with a Windows drive prefix: {name:?}");
    }
    let body = name.strip_suffix('/').unwrap_or(name);
    if body.is_empty() {
        bail!("archive entry name is just the directory marker {name:?}");
    }
    for component in body.split('/') {
        match component {
            "" => bail!("archive entry name has an empty path component: {name:?}"),
            "." | ".." => bail!(
                "archive entry name contains a {component:?} path component, which would \
                 climb out of the destination: {name:?}"
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Decide the destination path for an archive entry, or refuse it.
///
/// Only the entry's file name is kept, so `ffmpeg-<version>-essentials_build/bin/ffmpeg.exe`
/// lands at `<root>/ffmpeg.exe`. Keeping one component is itself a strong traversal defence,
/// but the name is validated first so a hostile name is reported, not quietly reduced.
pub fn entry_destination(root: &Path, name: &str) -> Result<PathBuf> {
    validate_entry_path(name)?;
    if name.ends_with('/') {
        bail!("archive entry {name:?} is a directory, not a file to extract");
    }
    let file_name = name.rsplit('/').next().unwrap_or(name);
    Ok(root.join(file_name))
}

/// A Windows path such as `C:ffmpeg.exe` is drive-relative, and `C:/x` absolute: neither is
/// safe to join.
fn has_drive_prefix(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Zip stores a Unix mode for entries made on Unix, and a symlink there is `S_IFLNK`.
/// Entries written on Windows carry DOS attributes instead and never look like this.
pub fn is_symlink_mode(mode: Option<u32>) -> bool {
    const S_IFMT: u32 = 0o170000;
    const S_IFLNK: u32 = 0o120000;
    mode.is_some_and(|m| m & S_IFMT == S_IFLNK)
}

// ---------------------------------------------------------------------------------------
// Reading the archive: scan, then extract
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub index: usize,
    pub name: String,
    pub is_dir: bool,
    pub bytes: u64,
}

/// One allowlisted path resolved to the archive entry that will supply it.
#[derive(Debug, Clone)]
pub struct Match {
    pub index: usize,
    /// The path as written in `sidecars.toml` (`bin/ffmpeg.exe`).
    pub allowlist_path: String,
    /// The path inside the archive (`ffmpeg-<version>-essentials_build/bin/ffmpeg.exe`).
    pub archive_name: String,
    /// The file name both of them end in, which is what lands in `binaries/`.
    pub file_name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Scan {
    /// Exactly one match per allowlisted path, in allowlist order.
    pub matches: Vec<Match>,
    /// Archive entries that are not allowlisted. Ignored, never extracted.
    pub ignored: Vec<String>,
    pub entry_count: usize,
}

#[derive(Debug, Clone)]
pub struct Extraction {
    pub path: PathBuf,
    pub bytes: u64,
    pub archive_name: String,
    pub allowlist_path: String,
}

#[derive(Debug, Clone)]
pub struct ExtractionOutcome {
    pub extracted: Vec<Extraction>,
    pub ignored: usize,
    pub entry_count: usize,
}

fn open_archive(path: &Path) -> Result<zip::ZipArchive<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    zip::ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("reading {} as a zip archive", path.display()))
}

/// Do an allowlist path and an archive entry name refer to the same file?
///
/// Upstream archives wrap their contents in a versioned top-level directory
/// (`ffmpeg-<version>-essentials_build/...`), so an allowlist entry matches either the whole
/// entry name or its final path components: `bin/ffmpeg.exe` matches `bin/ffmpeg.exe` and
/// `ffmpeg-anything/bin/ffmpeg.exe`. Two entries matching one allowlist path is an error.
fn path_matches(entry_name: &str, allowlist_path: &str) -> bool {
    if entry_name == allowlist_path {
        return true;
    }
    entry_name.len() > allowlist_path.len()
        && entry_name.ends_with(allowlist_path)
        && entry_name.as_bytes()[entry_name.len() - allowlist_path.len() - 1] == b'/'
}

/// Inspect an archive without writing anything: check every entry name, then resolve the
/// allowlist. Used by `record` to prove the allowlist names files the archive really has.
pub fn scan<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    root: &Path,
    allowlist: &[String],
) -> Result<Scan> {
    if allowlist.is_empty() {
        bail!("sidecars.toml lists no `binaries` for this platform; nothing to extract");
    }
    // The allowlist is hand-written, so validate it with the same rules as the archive: an
    // entry like `../ffmpeg.exe` must fail here, not steer the extraction.
    for want in allowlist {
        entry_destination(root, want)
            .with_context(|| format!("sidecars.toml lists an unsafe path: {want:?}"))?;
    }

    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let (name, is_dir, is_symlink, bytes) = {
            let entry = archive
                .by_index(index)
                .with_context(|| format!("reading zip entry #{index}"))?;
            (
                entry
                    .name()
                    .map_err(|e| anyhow::anyhow!("zip entry #{index} has an unreadable name: {e}"))?
                    .to_string(),
                entry.is_dir(),
                is_symlink_mode(entry.unix_mode()),
                entry.size(),
            )
        };
        // Every entry, allowlisted or not. An archive that smuggles `../escape.txt` or an absolute path
        // anywhere is not the archive the recorded hash was reviewed for.
        validate_entry_path(&name).with_context(|| format!("zip entry #{index} ({name:?})"))?;
        if is_symlink {
            bail!(
                "zip entry #{index} ({name:?}) is a symbolic link; sidecar archives must \
                 contain real files. Refusing the whole archive."
            );
        }
        entries.push(EntryInfo {
            index,
            name,
            is_dir,
            bytes,
        });
    }

    let mut matches = Vec::with_capacity(allowlist.len());
    for want in allowlist {
        let candidates: Vec<&EntryInfo> = entries
            .iter()
            .filter(|e| !e.is_dir && path_matches(&e.name, want))
            .collect();
        let file_name = want.rsplit('/').next().unwrap_or(want).to_string();
        match candidates.as_slice() {
            [] => bail!(
                "the archive does not contain {:?}. {} entries were checked; none matched that \
                 path (or that path below the archive's own top-level directory). This is not \
                 something to repair by guessing — the allowlist and the archive disagree.",
                want,
                archive.len()
            ),
            [one] => matches.push(Match {
                index: one.index,
                allowlist_path: want.clone(),
                archive_name: one.name.clone(),
                file_name,
                bytes: one.bytes,
            }),
            many => bail!(
                "{:?} matches {} entries in the archive ({}), so it is ambiguous: refusing \
                 rather than picking one",
                want,
                many.len(),
                many.iter()
                    .map(|e| e.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    for m in &matches {
        if m.bytes > MAX_EXTRACTED_FILE_BYTES {
            bail!(
                "{} declares {} bytes inside the archive, above the {} byte ceiling for one \
                 extracted file; refusing",
                m.archive_name,
                m.bytes,
                MAX_EXTRACTED_FILE_BYTES
            );
        }
    }

    let ignored = entries
        .iter()
        .filter(|e| !matches.iter().any(|m| m.index == e.index))
        .map(|e| e.name.clone())
        .collect();

    Ok(Scan {
        matches,
        ignored,
        entry_count: entries.len(),
    })
}

/// Extract exactly the allowlisted entries from `archive` into `root`. The archive must already
/// have been verified against its recorded hash.
pub fn extract_allowlisted(
    archive_path: &Path,
    root: &Path,
    allowlist: &[String],
) -> Result<ExtractionOutcome> {
    let mut archive = open_archive(archive_path)?;
    let scan = scan(&mut archive, root, allowlist)?;

    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;

    let mut extracted = Vec::with_capacity(scan.matches.len());
    for m in &scan.matches {
        let dest = entry_destination(root, &m.archive_name)
            .with_context(|| format!("choosing a destination for {:?}", m.archive_name))?;
        // Guaranteed by the suffix match above; asserted so a change to either rule fails the run
        // instead of writing a differently-named binary.
        if dest
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .as_deref()
            != Some(m.file_name.as_str())
        {
            bail!(
                "{:?} would be written as {}, but the allowlist asked for {}",
                m.archive_name,
                dest.display(),
                m.file_name
            );
        }
        let mut entry = archive
            .by_index(m.index)
            .with_context(|| format!("reading zip entry #{}", m.index))?;
        let bytes = write_entry_to_disk(&mut entry, &dest)?;
        extracted.push(Extraction {
            path: dest,
            bytes,
            archive_name: m.archive_name.clone(),
            allowlist_path: m.allowlist_path.clone(),
        });
    }

    Ok(ExtractionOutcome {
        extracted,
        ignored: scan.ignored.len(),
        entry_count: scan.entry_count,
    })
}

/// Stream one entry to `dest` via a `.part` sibling renamed into place only when the whole entry
/// is on disk: a truncated `ffmpeg.exe` where the app looks for it is worse than none.
fn write_entry_to_disk<R: Read>(entry: &mut R, dest: &Path) -> Result<u64> {
    let file_name = dest
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .context("destination has no file name")?;
    let partial = dest.with_file_name(format!("{file_name}.part"));

    let copied = (|| -> Result<u64> {
        let out =
            File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
        let mut writer = BufWriter::with_capacity(1 << 16, out);
        let mut buf = [0u8; 64 * 1024];
        let mut written = 0u64;
        loop {
            let n = entry.read(&mut buf).context("reading a zip entry")?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > MAX_EXTRACTED_FILE_BYTES {
                bail!(
                    "entry expands past the {MAX_EXTRACTED_FILE_BYTES} byte ceiling for one \
                     file; refusing (possible decompression bomb)"
                );
            }
            writer
                .write_all(&buf[..n])
                .context("writing an extracted file")?;
        }
        writer.flush().context("flushing an extracted file")?;
        Ok(written)
    })();

    let written = match copied {
        Ok(written) => written,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
    // `rename` over an existing file fails on Windows, so drop the old copy first.
    if dest.exists() {
        std::fs::remove_file(dest)
            .with_context(|| format!("replacing the existing {}", dest.display()))?;
    }
    std::fs::rename(&partial, dest)
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(written)
}

// ---------------------------------------------------------------------------------------
// The two commands
// ---------------------------------------------------------------------------------------

fn validate_entry(entry: &Entry) -> Result<()> {
    if entry.target.trim().is_empty() {
        bail!("a [[platform]] entry in sidecars.toml has no `target`");
    }
    if entry.binaries.is_empty() {
        bail!(
            "the {} entry in sidecars.toml lists no `binaries`; without an allowlist there is \
             nothing xtask may extract",
            entry.target
        );
    }
    match entry.archive_kind.as_deref() {
        Some("zip") => {}
        other => bail!(
            "the {} entry declares archive_kind = {:?}; only \"zip\" is implemented",
            entry.target,
            other
        ),
    }
    Ok(())
}

/// Refuse an entry whose URL is missing or still a placeholder, before any download.
fn validate_url(entry: &Entry) -> Result<()> {
    let url = entry.archive_url.trim();
    if url.is_empty() || url.contains("PLACEHOLDER") || url.contains("RECORD_ME") {
        bail!(
            "the {} entry has no usable archive_url (it is {:?}). Fill in the URL of the \
             LGPL build this platform should use before recording a hash for it.",
            entry.target,
            entry.archive_url
        );
    }
    if !url.starts_with("https://") {
        bail!(
            "the {} entry's archive_url is not https ({:?}). A hash recorded over an \
             unauthenticated transport pins whatever the network handed over, which is \
             exactly the thing the pin exists to avoid.",
            entry.target,
            url
        );
    }
    Ok(())
}

/// Download the URL to a file under the workspace's `target/` (gitignored).
fn work_archive(work_dir: &Path, target: &str) -> PathBuf {
    let safe: String = target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    work_dir.join(format!("{safe}.zip"))
}

/// `cargo xtask sidecars record [--target <triple>]`
///
/// Downloads every selected archive, hashes it, and rewrites `sidecars.toml`. All downloads
/// finish before anything is written, so a mid-way failure leaves the manifest untouched.
/// `binaries_dir` is only read, to validate the allowlist paths.
pub fn record(
    manifest_path: &Path,
    binaries_dir: &Path,
    work_dir: &Path,
    only: Option<&str>,
) -> Result<()> {
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest = parse_manifest(&text)?;

    let chosen: Vec<&Entry> = manifest
        .platform
        .iter()
        .filter(|e| only.is_none_or(|t| e.target == t))
        .collect();
    if chosen.is_empty() {
        bail!(
            "sidecars.toml has no entry for {}. It lists: {}",
            only.unwrap_or("<any target>"),
            list_targets(&manifest.platform)
        );
    }

    let mut updates = Vec::with_capacity(chosen.len());
    for entry in chosen {
        validate_entry(entry)?;
        validate_url(entry)?;
        let archive = work_archive(work_dir, &entry.target);

        println!("target  {}", entry.target);
        println!("url     {}", entry.archive_url);
        println!("saving  {}", archive.display());
        download(&entry.archive_url, &archive)?;

        let bytes = std::fs::metadata(&archive)
            .with_context(|| format!("stat {}", archive.display()))?
            .len();
        let hash = sha256_file(&archive)?;
        println!("bytes   {bytes}");
        println!("sha256  {hash}");

        // Hashing an archive that lacks the allowlisted files would record a pin `fetch` must then
        // fail on. Say so here, while a human is looking.
        let mut archive_reader = open_archive(&archive)?;
        let scan = scan(&mut archive_reader, binaries_dir, &entry.binaries)?;
        println!(
            "archive holds {} entries; the allowlist resolves to:",
            scan.entry_count
        );
        for m in &scan.matches {
            println!(
                "  {:<22} <- {} ({} bytes)",
                m.allowlist_path, m.archive_name, m.bytes
            );
        }
        println!();
        updates.push((entry.target.clone(), hash));
    }

    let updated = rewrite_sha256(&text, &updates)?;
    if updated != text {
        std::fs::write(manifest_path, &updated)
            .with_context(|| format!("writing {}", manifest_path.display()))?;
        println!("{} updated.", manifest_path.display());
    } else {
        println!(
            "{} already records these hashes; left unchanged.",
            manifest_path.display()
        );
    }

    println!(
        "\nTrust on first use. These hashes were computed from the archives downloaded just \
         now over HTTPS; nothing cryptographic attests that download, and whoever served it \
         could have served something else. What the recorded values do is pin *these* bytes: \
         from here on `cargo xtask sidecars fetch` refuses any archive that does not match \
         them. Check the URLs above against the upstream project's own download instructions, \
         then review and commit the diff of {} yourself — that review is the only trust \
         anchor in this scheme.",
        manifest_path.display()
    );
    Ok(())
}

/// `cargo xtask sidecars fetch [--target <triple>]`
///
/// Downloads the selected archive, verifies it against the recorded hash, and only then
/// extracts the allowlisted binaries into `binaries_dir`.
pub fn fetch(
    manifest_path: &Path,
    binaries_dir: &Path,
    requested: Option<&str>,
    host: Option<&str>,
    work_dir: &Path,
) -> Result<()> {
    let manifest = load_manifest(manifest_path)?;
    let entry = select_entry(&manifest.platform, requested, host)?;
    validate_entry(entry)?;
    validate_url(entry)?;

    let recorded = entry.sha256.trim();
    if recorded.is_empty() || recorded.eq_ignore_ascii_case(UNRECORDED) {
        bail!(
            "sidecars.toml has an unrecorded sha256 for {} (sha256 = {:?}). Run `cargo xtask \
             sidecars record` on a trusted network, review the diff, and commit the recorded \
             hash. Fetching will then extract the archives into {}.",
            entry.target,
            entry.sha256,
            binaries_dir.display()
        );
    }

    let archive = work_archive(work_dir, &entry.target);
    println!("target   {}", entry.target);
    if let Some(host) = host {
        if host != entry.target {
            println!(
                "host     {host} (fetching another platform's sidecar: {})",
                entry.target
            );
        } else {
            println!("host     {host}");
        }
    }
    println!("url      {}", entry.archive_url);
    println!("expected sha256 {recorded}");
    println!("saving   {}", archive.display());
    download(&entry.archive_url, &archive)?;

    // Verification, and only verification, comes before extraction.
    let actual = verify_sha256(&archive, recorded)?;
    println!("actual   sha256 {actual}  OK");

    let outcome = extract_allowlisted(&archive, binaries_dir, &entry.binaries)?;
    for f in &outcome.extracted {
        println!(
            "wrote    {}  ({} bytes)  from {} (allowlisted as {})",
            f.path.display(),
            f.bytes,
            f.archive_name,
            f.allowlist_path
        );
    }
    println!(
        "         {} of {} archive entries ignored (not in the allowlist: {}).",
        outcome.ignored,
        outcome.entry_count,
        entry.binaries.join(", ")
    );

    // Prove the binaries run only where they can: a Windows .exe cannot run on this host, and
    // pretending to have verified it would be the worst outcome.
    if host == Some(entry.target.as_str()) {
        for f in &outcome.extracted {
            let name = f.path.file_name().map(|n| n.to_string_lossy().to_string());
            if !name.as_deref().is_some_and(is_ffmpeg_family) {
                continue;
            }
            println!("checking {}", f.path.display());
            println!("         {}", first_line_of_version(&f.path)?);
        }
    } else {
        println!(
            "not run  the host is {}, the archive is for {}: these binaries cannot be executed \
             here, so `ffmpeg -version` was NOT run. That check has to happen on a {} machine.",
            host.unwrap_or("<unknown>"),
            entry.target,
            entry.target
        );
    }
    Ok(())
}

/// Both sidecar binaries answer `-version`; other allowlist entries are left alone rather than
/// reported as a failed check.
fn is_ffmpeg_family(file_name: &str) -> bool {
    ["ffmpeg", "ffmpeg.exe", "ffprobe", "ffprobe.exe"].contains(&file_name)
}

/// Run `<bin> -version` and return its first non-empty output line, through the media crate's
/// bounded runner so a wedged binary (a missing DLL, say) cannot park the build.
fn first_line_of_version(bin: &Path) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.arg("-version");
    let out = replayed_media::binaries::run_with_timeout(cmd, Duration::from_secs(15))
        .with_context(|| format!("running `{} -version`", bin.display()))?;
    let text = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).to_string()
    } else {
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    text.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .context("ffmpeg -version printed nothing")
}

#[cfg(test)]
mod tests {
    /// The value that follows `flag` in curl's argument list.
    fn curl_value(flag: &str) -> Option<&'static str> {
        let at = CURL_ARGS.iter().position(|a| *a == flag)?;
        CURL_ARGS.get(at + 1).copied()
    }

    #[test]
    fn a_download_refuses_a_redirect_to_anything_but_https() {
        // `--location` alone follows a redirect to http (or ftp, or anything curl speaks).
        assert_eq!(curl_value("--proto"), Some("=https"));
        assert_eq!(curl_value("--proto-redir"), Some("=https"));
        assert!(
            CURL_ARGS.contains(&"--location"),
            "redirects are still followed"
        );
        assert!(CURL_ARGS.contains(&"--tlsv1.2"), "TLS 1.2 is the floor");
    }

    #[test]
    fn a_download_is_bounded_in_time_and_size() {
        let seconds: u64 = curl_value("--max-time").unwrap().parse().unwrap();
        let connect: u64 = curl_value("--connect-timeout").unwrap().parse().unwrap();
        let bytes: u64 = curl_value("--max-filesize").unwrap().parse().unwrap();
        assert!(
            connect <= 60 && seconds <= 3600,
            "a stalled CDN must fail in bounded time"
        );
        assert!(
            bytes >= 200 * 1024 * 1024,
            "the real archive (~120 MB) must still fit"
        );
        assert!(bytes <= 1024 * 1024 * 1024, "and a runaway one must not");
    }

    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------------------
    // A tiny zip *writer*, so fixtures can express what the `zip` crate's safe writer refuses to
    // create (a symlink entry, `../` names, absolute paths). Entries are stored uncompressed.
    // -----------------------------------------------------------------------------------

    struct FixtureEntry<'a> {
        name: &'a str,
        data: &'a [u8],
        /// Unix mode. `Some(0o120777)` makes the entry a symlink; `None` writes a DOS-made
        /// entry with no mode at all, like a zip built on Windows.
        mode: Option<u32>,
        is_dir: bool,
    }

    fn zip_fixture(entries: &[FixtureEntry<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for e in entries {
            let offset = out.len() as u32;
            let crc = crc32(e.data);
            let name = e.name.as_bytes();
            let version_made_by: u16 = match e.mode {
                Some(_) => 3 << 8 | 20, // Unix
                None => 20,
            };
            let external: u32 = match e.mode {
                Some(mode) => mode << 16,
                None => {
                    if e.is_dir {
                        0x10
                    } else {
                        0
                    }
                }
            };

            // Local file header.
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
            out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0x21u16.to_le_bytes()); // date: 1980-01-01
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra length
            out.extend_from_slice(name);
            out.extend_from_slice(e.data);

            // Central directory entry.
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&version_made_by.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0x0800u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0x21u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk number
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&external.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
        }

        let central_offset = out.len() as u32;
        let central_size = central.len() as u32;
        out.extend_from_slice(&central);

        // End of central directory.
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&central_size.to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out
    }

    /// CRC-32 (IEEE), computed here so the fixtures do not lean on a transitive dependency.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in data {
            crc ^= *byte as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn write_fixture(dir: &Path, name: &str, entries: &[FixtureEntry<'_>]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, zip_fixture(entries)).expect("writing the fixture archive");
        path
    }

    fn allowlist(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    /// A believable archive: the upstream wrapper directory, the two binaries, plus decoys
    /// that must not be extracted.
    fn windows_like_archive(dir: &Path) -> PathBuf {
        write_fixture(
            dir,
            "sidecar.zip",
            &[
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/",
                    data: &[],
                    mode: None,
                    is_dir: true,
                },
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/bin/",
                    data: &[],
                    mode: None,
                    is_dir: true,
                },
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/bin/ffmpeg.exe",
                    data: b"pretend ffmpeg",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/bin/ffprobe.exe",
                    data: b"pretend ffprobe",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/README.txt",
                    data: b"decoy",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/bin/ffmpeg.txt",
                    data: b"decoy",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
                FixtureEntry {
                    name: "ffmpeg.exe",
                    data: b"decoy at the archive root",
                    mode: None,
                    is_dir: false,
                },
            ],
        )
    }

    // -----------------------------------------------------------------------------------
    // The traversal rules, pinned exhaustively.
    // -----------------------------------------------------------------------------------

    #[test]
    fn entry_destination_keeps_only_the_file_name() {
        let root = Path::new("/dest");
        for (name, expected) in [
            ("ffmpeg.exe", "/dest/ffmpeg.exe"),
            ("bin/ffmpeg.exe", "/dest/ffmpeg.exe"),
            ("ffmpeg-7.1-essentials_build/bin/ffprobe", "/dest/ffprobe"),
            ("a/b/c/d.bin", "/dest/d.bin"),
            ("with space.exe", "/dest/with space.exe"),
            ("dotted.name.exe", "/dest/dotted.name.exe"),
            (".hidden", "/dest/.hidden"),
            ("-leading-dash", "/dest/-leading-dash"),
            ("unicodé.exe", "/dest/unicodé.exe"),
        ] {
            let got = entry_destination(root, name).expect("safe name");
            assert_eq!(got, PathBuf::from(expected), "for {name:?}");
        }
    }

    #[test]
    fn entry_destination_refuses_every_escape_attempt() {
        let root = Path::new("/dest");
        for name in [
            "",
            "../escape.exe",
            "..\\escape.exe",
            "bin/../../escape.exe",
            "bin/../../../etc/passwd",
            "./bin/ffmpeg.exe",
            "bin/./ffmpeg.exe",
            "/etc/passwd",
            "/absolute.exe",
            "//server/share/ffmpeg.exe",
            "C:/Windows/system32/evil.exe",
            "c:evil.exe",
            "Z:evil.exe",
            "bin\\ffmpeg.exe",
            "\\\\server\\share\\evil.exe",
            "bin//ffmpeg.exe",
            "bin/",
            "/",
            "..",
            ".",
            "bin/..",
            "ffmpeg.exe\0",
            "nodir/..",
        ] {
            let err =
                entry_destination(root, name).expect_err(&format!("{name:?} must be refused"));
            assert!(
                !err.to_string().is_empty(),
                "the refusal must say why: {name:?}"
            );
        }
    }

    #[test]
    fn validate_entry_path_accepts_ordinary_archive_names() {
        for name in [
            "ffmpeg-7.1-essentials_build/",
            "ffmpeg-7.1-essentials_build/bin/",
            "ffmpeg-7.1-essentials_build/bin/ffmpeg.exe",
            "LICENSE",
            "a b/c d.txt",
            ".gitignore",
        ] {
            validate_entry_path(name).unwrap_or_else(|e| panic!("{name:?} should be fine: {e}"));
        }
    }

    #[test]
    fn only_symlink_modes_are_symlinks() {
        assert!(is_symlink_mode(Some(0o120777)));
        assert!(is_symlink_mode(Some(0o120_000)));
        assert!(!is_symlink_mode(Some(0o100_644)));
        assert!(!is_symlink_mode(Some(0o40755))); // directory
        assert!(!is_symlink_mode(Some(0)));
        assert!(!is_symlink_mode(None)); // DOS-made entry, no mode at all
    }

    // -----------------------------------------------------------------------------------
    // Extraction
    // -----------------------------------------------------------------------------------

    #[test]
    fn extracts_exactly_the_allowlisted_entries_and_ignores_decoys() {
        let tmp = TempDir::new().expect("temp dir");
        let archive = windows_like_archive(tmp.path());
        let dest = tmp.path().join("binaries");

        let outcome = extract_allowlisted(
            &archive,
            &dest,
            &allowlist(&["bin/ffmpeg.exe", "bin/ffprobe.exe"]),
        )
        .expect("extraction");

        assert_eq!(outcome.extracted.len(), 2);
        assert_eq!(
            fs::read(dest.join("ffmpeg.exe")).expect("ffmpeg.exe"),
            b"pretend ffmpeg"
        );
        assert_eq!(
            fs::read(dest.join("ffprobe.exe")).expect("ffprobe.exe"),
            b"pretend ffprobe"
        );
        // The decoys stay out — including the `ffmpeg.exe` sitting at the archive root,
        // which must not match the `bin/ffmpeg.exe` allowlist entry.
        assert_eq!(
            fs::read_dir(&dest).expect("listing").count(),
            2,
            "only the two allowlisted files may be written"
        );
        assert_eq!(outcome.entry_count, 7);
        assert_eq!(outcome.ignored, 5);
        // No leftover `.part` files.
        assert!(!dest.join("ffmpeg.exe.part").exists());
    }

    #[test]
    fn zip_slip_entry_is_rejected_and_nothing_is_written_outside_the_destination() {
        let tmp = TempDir::new().expect("temp dir");
        let dest = tmp.path().join("binaries");
        let archive = write_fixture(
            tmp.path(),
            "slip.zip",
            &[
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/bin/ffmpeg.exe",
                    data: b"pretend ffmpeg",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/../../../escape.txt",
                    data: b"escaped",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
            ],
        );

        let err = extract_allowlisted(&archive, &dest, &allowlist(&["bin/ffmpeg.exe"]))
            .expect_err("a zip-slip entry must abort the whole extraction");
        let msg = format!("{err:#}");
        // Printed so `--nocapture` shows the refusal itself, not just a passing test.
        println!("zip-slip refusal: {msg}");
        assert!(
            msg.contains("escape.txt"),
            "the error must name the entry: {msg}"
        );
        assert!(
            msg.contains(".."),
            "the error must say what was wrong with it: {msg}"
        );

        // Nothing at all was written: not the escaping file, not the files around it.
        assert!(!tmp.path().join("escape.txt").exists());
        assert!(!tmp.path().join("binaries").exists());
        assert!(!dest.join("ffmpeg.exe").exists());
        let stray: Vec<_> = walk_files(tmp.path())
            .into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "escape.txt"))
            .collect();
        assert!(stray.is_empty(), "escaped files found: {stray:?}");
    }

    #[test]
    fn absolute_path_entry_is_rejected() {
        let tmp = TempDir::new().expect("temp dir");
        let dest = tmp.path().join("binaries");
        let outside = tmp.path().join("absolute-outside.txt");
        let archive = write_fixture(
            tmp.path(),
            "absolute.zip",
            &[FixtureEntry {
                name: "/tmp/absolute-outside.txt",
                data: b"escaped",
                mode: Some(0o100_644),
                is_dir: false,
            }],
        );
        let err = extract_allowlisted(&archive, &dest, &allowlist(&["bin/ffmpeg.exe"]))
            .expect_err("an absolute entry name must be refused");
        assert!(format!("{err:#}").contains("absolute"), "{err:#}");
        assert!(!outside.exists());
        assert!(!dest.exists());
    }

    #[test]
    fn windows_drive_and_backslash_entries_are_rejected() {
        let tmp = TempDir::new().expect("temp dir");
        let dest = tmp.path().join("binaries");
        for (file, name, reason) in [
            ("drive.zip", "C:/sidecars/ffmpeg.exe", "drive prefix"),
            ("backslash.zip", "..\\escape.exe", "backslash"),
            ("unc.zip", "\\\\server\\share\\ffmpeg.exe", "backslash"),
        ] {
            let archive = write_fixture(
                tmp.path(),
                file,
                &[FixtureEntry {
                    name,
                    data: b"escaped",
                    mode: Some(0o100_644),
                    is_dir: false,
                }],
            );
            let err = extract_allowlisted(&archive, &dest, &allowlist(&["bin/ffmpeg.exe"]))
                .expect_err("a drive-prefixed or backslashed entry must be refused");
            let msg = format!("{err:#}");
            assert!(
                msg.contains(reason),
                "{name:?} should report {reason:?}: {msg}"
            );
            assert!(!dest.exists(), "{name:?} was not allowed to write anything");
        }
    }

    #[test]
    fn symlink_entry_is_rejected() {
        let tmp = TempDir::new().expect("temp dir");
        let dest = tmp.path().join("binaries");
        // A real symlink entry: mode 0o120777 in the central directory, `/etc/passwd` as
        // its target. Zip can express this; the reader reports it and extraction refuses.
        let archive = write_fixture(
            tmp.path(),
            "symlink.zip",
            &[
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/bin/ffmpeg.exe",
                    data: b"pretend ffmpeg",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/bin/ffprobe.exe",
                    data: b"/etc/passwd",
                    mode: Some(0o120777),
                    is_dir: false,
                },
            ],
        );

        // First, the fixture really is what the test thinks it is.
        let mut reader = open_archive(&archive).expect("opening the fixture");
        let mode = reader
            .by_name("ffmpeg-7.1-essentials_build/bin/ffprobe.exe")
            .expect("entry")
            .unix_mode();
        assert!(
            is_symlink_mode(mode),
            "fixture mode {mode:?} should read as a symlink"
        );

        let err = extract_allowlisted(
            &archive,
            &dest,
            &allowlist(&["bin/ffmpeg.exe", "bin/ffprobe.exe"]),
        )
        .expect_err("a symlink entry must be refused, allowlisted or not");
        let msg = format!("{err:#}");
        println!("symlink refusal: {msg}");
        assert!(msg.contains("symbolic link"), "{msg}");
        assert!(
            msg.contains("ffprobe.exe"),
            "the error must name the entry: {msg}"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn missing_allowlisted_entry_is_an_error_naming_it() {
        let tmp = TempDir::new().expect("temp dir");
        let dest = tmp.path().join("binaries");
        let archive = windows_like_archive(tmp.path());
        let err = extract_allowlisted(
            &archive,
            &dest,
            &allowlist(&["bin/ffmpeg.exe", "bin/ffplay.exe"]),
        )
        .expect_err("a missing allowlisted path must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bin/ffplay.exe"),
            "must name the missing path: {msg}"
        );
        assert!(!dest.exists(), "a failed extraction writes nothing");
    }

    #[test]
    fn ambiguous_allowlist_match_is_refused_rather_than_guessed() {
        let tmp = TempDir::new().expect("temp dir");
        let dest = tmp.path().join("binaries");
        let archive = write_fixture(
            tmp.path(),
            "ambiguous.zip",
            &[
                FixtureEntry {
                    name: "ffmpeg-7.1-essentials_build/bin/ffmpeg.exe",
                    data: b"one",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
                FixtureEntry {
                    name: "ffmpeg-7.2-essentials_build/bin/ffmpeg.exe",
                    data: b"two",
                    mode: Some(0o100_644),
                    is_dir: false,
                },
            ],
        );
        let err = extract_allowlisted(&archive, &dest, &allowlist(&["bin/ffmpeg.exe"]))
            .expect_err("two candidates is ambiguous");
        assert!(format!("{err:#}").contains("ambiguous"), "{err:#}");
        assert!(!dest.exists());
    }

    #[test]
    fn an_unsafe_allowlist_entry_is_refused_too() {
        let tmp = TempDir::new().expect("temp dir");
        let dest = tmp.path().join("binaries");
        let archive = windows_like_archive(tmp.path());
        let err = extract_allowlisted(&archive, &dest, &allowlist(&["../ffmpeg.exe"]))
            .expect_err("the allowlist is untrusted input as well");
        assert!(format!("{err:#}").contains("unsafe"), "{err:#}");
        assert!(!dest.exists());
    }

    #[test]
    fn scan_reports_what_record_would_hash_against() {
        let tmp = TempDir::new().expect("temp dir");
        let archive = windows_like_archive(tmp.path());
        let mut reader = open_archive(&archive).expect("opening the fixture");
        let scan =
            scan(&mut reader, tmp.path(), &allowlist(&["bin/ffprobe.exe"])).expect("scanning");
        assert_eq!(scan.matches.len(), 1);
        assert_eq!(scan.matches[0].file_name, "ffprobe.exe");
        assert_eq!(
            scan.matches[0].archive_name,
            "ffmpeg-7.1-essentials_build/bin/ffprobe.exe"
        );
        assert_eq!(scan.ignored.len(), scan.entry_count - 1);
        assert!(scan.ignored.iter().any(|n| n.ends_with("README.txt")));
    }

    // -----------------------------------------------------------------------------------
    // Verification
    // -----------------------------------------------------------------------------------

    #[test]
    fn checksum_mismatch_aborts_before_any_extraction() {
        let tmp = TempDir::new().expect("temp dir");
        let archive = windows_like_archive(tmp.path());
        let dest = tmp.path().join("binaries");

        // The real digest of the fixture, and the same digest with one nibble changed.
        let good = sha256_file(&archive).expect("hashing the fixture");
        assert_eq!(good.len(), 64);
        let bad = format!("{}0", &good[..63]);
        assert_ne!(good, bad);

        verify_sha256(&archive, &good).expect("the real digest verifies");
        let err = verify_sha256(&archive, &bad).expect_err("a wrong digest must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains(&good), "expected hash must be named: {msg}");
        assert!(msg.contains(&bad), "actual hash must be named: {msg}");
        assert!(msg.contains("Nothing was extracted"), "{msg}");

        // The sequence `fetch` runs: verification first, extraction only after it passes.
        assert!(verify_sha256(&archive, &bad).is_err() && !dest.exists());

        // A placeholder or malformed digest is refused as such, not compared as a hash.
        let err = verify_sha256(&archive, UNRECORDED).expect_err("RECORD_ME is not a digest");
        assert!(format!("{err:#}").contains("64-character"), "{err:#}");
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        let tmp = TempDir::new().expect("temp dir");
        let file = tmp.path().join("abc.bin");
        fs::write(&file, b"abc").expect("writing");
        assert_eq!(
            sha256_file(&file).expect("hashing"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn fetches_a_mismatched_archive_refuses_and_extracts_nothing() {
        // The full `fetch` path minus the network: the digest check runs against a real
        // archive file with the wrong recorded hash, and the extraction must never start.
        let tmp = TempDir::new().expect("temp dir");
        let archive = windows_like_archive(tmp.path());
        let dest = tmp.path().join("binaries");
        let wrong = "0".repeat(64);
        let err = verify_sha256(&archive, &wrong).expect_err("mismatch");
        assert!(format!("{err:#}").contains("checksum mismatch"));
        assert!(!dest.exists());
        // And with the right digest the same archive does extract, so the test above is not
        // passing because extraction is broken.
        let good = sha256_file(&archive).expect("hashing");
        verify_sha256(&archive, &good).expect("verified");
        extract_allowlisted(&archive, &dest, &allowlist(&["bin/ffmpeg.exe"]))
            .expect("extraction after a successful verification");
        assert!(dest.join("ffmpeg.exe").is_file());
    }

    // -----------------------------------------------------------------------------------
    // Manifest handling
    // -----------------------------------------------------------------------------------

    const SAMPLE: &str = "\
# xtask/sidecars.toml
#
# LGPL ffmpeg builds only: a comment that must survive rewriting.

[[platform]]
target = \"x86_64-pc-windows-msvc\"
archive_url = \"https://example.invalid/ffmpeg.zip\"
# Recorded on first fetch. Reviewed in the PR.
sha256 = \"RECORD_ME\"
archive_kind = \"zip\"
binaries = [\"bin/ffmpeg.exe\"]

[[platform]]
target = \"aarch64-apple-darwin\"
archive_url = \"https://example.invalid/ffmpeg-macos.zip\"
sha256 = \"aaaa\"
archive_kind = \"zip\"
binaries = [\"bin/ffmpeg\"]
";

    #[test]
    fn rewrite_touches_only_the_named_target_and_keeps_the_comments() {
        let updated = rewrite_sha256(
            SAMPLE,
            &[("x86_64-pc-windows-msvc".into(), "deadbeef".into())],
        )
        .expect("rewriting");
        assert!(updated.contains("sha256 = \"deadbeef\""));
        assert!(
            updated.contains("sha256 = \"aaaa\""),
            "the other entry is untouched"
        );
        assert!(updated.contains("# Recorded on first fetch. Reviewed in the PR."));
        assert!(updated.contains("# LGPL ffmpeg builds only"));
        assert!(!updated.contains("RECORD_ME"));
        // Exactly one line changed.
        let before: Vec<_> = SAMPLE.lines().collect();
        let after: Vec<_> = updated.lines().collect();
        assert_eq!(before.len(), after.len());
        assert_eq!(
            before
                .iter()
                .zip(after.iter())
                .filter(|(a, b)| a != b)
                .count(),
            1
        );
        // Rewriting is idempotent, and the result still parses.
        let again = rewrite_sha256(
            &updated,
            &[("x86_64-pc-windows-msvc".into(), "deadbeef".into())],
        )
        .expect("rewriting again");
        assert_eq!(again, updated);
        parse_manifest(&updated).expect("still valid TOML");
    }

    #[test]
    fn rewrite_refuses_to_report_success_for_a_target_it_did_not_find() {
        let err = rewrite_sha256(SAMPLE, &[("x86_64-unknown-linux-gnu".into(), "x".into())])
            .expect_err("a target that is not in the file must be an error");
        assert!(
            format!("{err:#}").contains("x86_64-unknown-linux-gnu"),
            "{err:#}"
        );
    }

    #[test]
    fn rewrite_keeps_two_hashes_for_the_same_target_distinct() {
        // The `target` line is what scopes a `sha256` line: the second entry must not be
        // rewritten by an update aimed at the first.
        let updated = rewrite_sha256(SAMPLE, &[("aarch64-apple-darwin".into(), "cafe".into())])
            .expect("rewriting");
        assert!(updated.contains("sha256 = \"cafe\""));
        assert!(updated.contains("sha256 = \"RECORD_ME\""));
        assert!(!updated.contains("sha256 = \"aaaa\""));
    }

    #[test]
    fn placeholder_manifest_parses_and_looks_unrecorded() {
        let manifest = parse_manifest(SAMPLE).expect("parsing");
        assert_eq!(manifest.platform.len(), 2);
        assert_eq!(manifest.platform[0].sha256, UNRECORDED);
        assert_eq!(manifest.platform[0].archive_kind.as_deref(), Some("zip"));
        assert_eq!(
            manifest.platform[0].binaries,
            allowlist(&["bin/ffmpeg.exe"])
        );
    }

    // -----------------------------------------------------------------------------------
    // Target selection
    // -----------------------------------------------------------------------------------

    fn entries_of(manifest: &Manifest) -> Vec<Entry> {
        manifest.platform.clone()
    }

    #[test]
    fn requested_target_wins_and_unknown_targets_list_the_alternatives() {
        let manifest = parse_manifest(SAMPLE).expect("parsing");
        let entries = entries_of(&manifest);

        let picked = select_entry(
            &entries,
            Some("aarch64-apple-darwin"),
            Some("x86_64-apple-darwin"),
        )
        .expect("explicit --target wins over the host");
        assert_eq!(picked.target, "aarch64-apple-darwin");

        let by_host =
            select_entry(&entries, None, Some("x86_64-pc-windows-msvc")).expect("the host's entry");
        assert_eq!(by_host.target, "x86_64-pc-windows-msvc");

        let err = select_entry(&entries, Some("x86_64-unknown-linux-gnu"), None)
            .expect_err("a target that is not in the manifest must be an error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("x86_64-pc-windows-msvc"),
            "must list what exists: {msg}"
        );
        assert!(
            msg.contains("aarch64-apple-darwin"),
            "must list what exists: {msg}"
        );
    }

    #[test]
    fn a_host_with_no_entry_says_so_instead_of_fetching_anything() {
        let manifest = parse_manifest(SAMPLE).expect("parsing");
        let err = select_entry(
            &entries_of(&manifest),
            None,
            Some("x86_64-unknown-linux-gnu"),
        )
        .expect_err("the host has no entry, which must be an error and not a fetch");
        let msg = format!("{err:#}");
        assert!(msg.contains("x86_64-unknown-linux-gnu"), "{msg}");
        assert!(msg.contains("--target"), "must say how to proceed: {msg}");

        let err = select_entry(&entries_of(&manifest), None, None)
            .expect_err("an unknown host must ask for --target");
        assert!(format!("{err:#}").contains("--target"));
    }

    // -----------------------------------------------------------------------------------
    // The refusal `fetch` makes before it downloads anything
    // -----------------------------------------------------------------------------------

    /// A manifest on disk with one Windows entry, so `fetch` can be driven without a network.
    fn manifest_file(dir: &Path, sha256: &str, url: &str) -> PathBuf {
        let path = dir.join("sidecars.toml");
        fs::write(
            &path,
            format!(
                "[[platform]]\ntarget = \"x86_64-pc-windows-msvc\"\narchive_url = \"{url}\"\n\
                 sha256 = \"{sha256}\"\narchive_kind = \"zip\"\nbinaries = [\"bin/ffmpeg.exe\"]\n"
            ),
        )
        .expect("writing the manifest");
        path
    }

    #[test]
    fn fetch_refuses_record_me_with_the_actionable_message() {
        let tmp = TempDir::new().expect("temp dir");
        let manifest = manifest_file(
            tmp.path(),
            UNRECORDED,
            "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
        );
        let binaries = tmp.path().join("binaries");
        let err = fetch(
            &manifest,
            &binaries,
            Some("x86_64-pc-windows-msvc"),
            Some("x86_64-apple-darwin"),
            &tmp.path().join("target/sidecars"),
        )
        .expect_err("RECORD_ME must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("sha256 = \"RECORD_ME\""), "{msg}");
        assert!(msg.contains("sidecars record"), "must name the fix: {msg}");
        assert!(
            msg.contains("trusted network"),
            "must say where to record: {msg}"
        );
        assert!(
            msg.contains(&binaries.display().to_string()),
            "must name the sidecar directory: {msg}"
        );
        assert!(!binaries.exists(), "nothing may be written");
    }

    #[test]
    fn fetch_refuses_a_placeholder_url_before_downloading() {
        let tmp = TempDir::new().expect("temp dir");
        let manifest = manifest_file(
            tmp.path(),
            &"a".repeat(64),
            "https://example.invalid/PLACEHOLDER.zip",
        );
        let err = fetch(
            &manifest,
            &tmp.path().join("binaries"),
            Some("x86_64-pc-windows-msvc"),
            None,
            &tmp.path().join("target/sidecars"),
        )
        .expect_err("a placeholder URL must be refused");
        assert!(format!("{err:#}").contains("archive_url"), "{err:#}");
    }

    #[test]
    fn fetch_refuses_a_plain_http_url() {
        let tmp = TempDir::new().expect("temp dir");
        let manifest = manifest_file(
            tmp.path(),
            &"a".repeat(64),
            "http://example.invalid/ffmpeg.zip",
        );
        let err = fetch(
            &manifest,
            &tmp.path().join("binaries"),
            Some("x86_64-pc-windows-msvc"),
            None,
            &tmp.path().join("target/sidecars"),
        )
        .expect_err("http must be refused");
        assert!(format!("{err:#}").contains("https"), "{err:#}");
    }

    #[test]
    fn only_the_ffmpeg_family_is_version_probed() {
        for name in ["ffmpeg", "ffmpeg.exe", "ffprobe", "ffprobe.exe"] {
            assert!(is_ffmpeg_family(name), "{name} should be probed");
        }
        for name in ["readme.txt", "ffplay.exe", "ffmpeg-notes.md", "ffprobe64"] {
            assert!(!is_ffmpeg_family(name), "{name} should not be probed");
        }
    }

    #[test]
    fn first_line_of_version_reports_a_real_binarys_version() {
        // The reporting `fetch` does when the archive matches the host platform, run against whatever
        // ffmpeg the host has on PATH. Needs a real ffmpeg, like `crates/media/tests/lossless.rs`.
        let Some(ffmpeg) = find_on_path("ffmpeg") else {
            eprintln!("skipping: no ffmpeg on PATH");
            return;
        };
        let line = first_line_of_version(&ffmpeg).expect("running ffmpeg -version");
        assert!(
            line.to_lowercase().contains("ffmpeg"),
            "first line should name ffmpeg: {line:?}"
        );
        println!("{ffmpeg:?} -version -> {line}");
    }

    fn find_on_path(stem: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let mut candidates = vec![PathBuf::from(stem)];
        if cfg!(windows) {
            candidates.insert(0, PathBuf::from(format!("{stem}.exe")));
        }
        std::env::split_paths(&path)
            .find_map(|dir| candidates.iter().map(|c| dir.join(c)).find(|p| p.is_file()))
    }

    #[test]
    fn record_refuses_an_entry_with_a_placeholder_url() {
        let tmp = TempDir::new().expect("temp dir");
        let manifest = manifest_file(
            tmp.path(),
            UNRECORDED,
            "https://example.invalid/PLACEHOLDER.zip",
        );
        let before = fs::read_to_string(&manifest).expect("reading");
        let err = record(
            &manifest,
            &tmp.path().join("binaries"),
            &tmp.path().join("target/sidecars"),
            None,
        )
        .expect_err("recording against a placeholder URL makes no sense");
        assert!(format!("{err:#}").contains("archive_url"), "{err:#}");
        assert_eq!(
            fs::read_to_string(&manifest).expect("re-reading"),
            before,
            "a refused record must not rewrite the manifest"
        );
    }

    #[test]
    fn record_refuses_an_entry_whose_binaries_list_is_empty() {
        let tmp = TempDir::new().expect("temp dir");
        let manifest = tmp.path().join("sidecars.toml");
        fs::write(
            &manifest,
            "[[platform]]\ntarget = \"x86_64-pc-windows-msvc\"\n\
             archive_url = \"https://example.invalid/ffmpeg.zip\"\nsha256 = \"RECORD_ME\"\n\
             archive_kind = \"zip\"\nbinaries = []\n",
        )
        .expect("writing");
        let err = record(
            &manifest,
            &tmp.path().join("binaries"),
            &tmp.path().join("target/sidecars"),
            None,
        )
        .expect_err("no allowlist means nothing to extract");
        assert!(format!("{err:#}").contains("binaries"), "{err:#}");
    }

    #[test]
    fn record_reports_an_unknown_target_rather_than_silently_doing_nothing() {
        let tmp = TempDir::new().expect("temp dir");
        let manifest = manifest_file(tmp.path(), UNRECORDED, "https://example.invalid/ffmpeg.zip");
        let err = record(
            &manifest,
            &tmp.path().join("binaries"),
            &tmp.path().join("target/sidecars"),
            Some("aarch64-apple-darwin"),
        )
        .expect_err("an unknown target must be an error");
        assert!(
            format!("{err:#}").contains("aarch64-apple-darwin"),
            "{err:#}"
        );
    }

    // -----------------------------------------------------------------------------------
    // Deliberate, network-using end-to-end run.
    // -----------------------------------------------------------------------------------

    /// Run with `cargo test -p xtask -- --ignored --nocapture`.
    ///
    /// Downloads the real Windows archive twice (record, then fetch), records its hash in
    /// `xtask/sidecars.toml` and extracts the two binaries into the gitignored `<repo>/binaries/`.
    /// A test cannot check that a Windows `.exe` runs on this host; `fetch` says it did not check.
    #[test]
    #[ignore = "downloads ~120 MB from gyan.dev and writes <repo>/binaries; run deliberately"]
    fn real_windows_sidecar_records_then_fetches() {
        const TARGET: &str = "x86_64-pc-windows-msvc";
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask lives in the repository root")
            .to_path_buf();
        let manifest = repo.join("xtask/sidecars.toml");
        let work = repo.join("target/sidecars");
        let binaries = repo.join("binaries");

        record(&manifest, &binaries, &work, Some(TARGET))
            .expect("recording the Windows sidecar hash");

        let recorded = load_manifest(&manifest)
            .expect("re-reading the manifest")
            .platform
            .into_iter()
            .find(|e| e.target == TARGET)
            .expect("the Windows entry")
            .sha256;
        assert_eq!(recorded.len(), 64, "record wrote a sha256: {recorded}");
        assert_ne!(recorded, UNRECORDED);

        fetch(
            &manifest,
            &binaries,
            Some(TARGET),
            host_target().as_deref(),
            &work,
        )
        .expect("fetching the recorded Windows sidecar");

        for name in ["ffmpeg.exe", "ffprobe.exe"] {
            let path = binaries.join(name);
            let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                meta.len() > 1_000_000,
                "{} is suspiciously small: {} bytes",
                path.display(),
                meta.len()
            );
            println!("{}: {} bytes", path.display(), meta.len());
        }
    }

    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found
    }
}
