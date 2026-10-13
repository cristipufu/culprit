//! `culprit update`: replace this binary with the latest published release's, verified against that release's
//! SHA256SUMS.txt. It runs only when asked: no other command checks for updates or goes online for them, so an agent
//! running `culprit scan` never makes a call it didn't ask for.

use std::cmp::Ordering;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use culprit_core::{Error, ErrorCode, Result};
use serde_json::{Value, json};

use crate::output::{Format, Out};

const REPO: &str = "cristipufu/culprit-sql-studio";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Far above any release binary, so a wrong or hostile response can't fill memory.
const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;

#[derive(Args)]
pub struct UpdateArgs {
    /// Only say whether a newer release exists; change nothing.
    #[arg(long)]
    check: bool,
}

/// A release version: `x.y.z`, with an optional pre-release tag that sorts below the release it precedes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    numbers: [u64; 3],
    pre: Option<String>,
}

impl Version {
    /// `0.2.0`, `v0.2.0` or `0.2.1-beta.1`. Build metadata (`+…`) is ignored.
    pub(crate) fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
            Some(_) => return None,
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut numbers = [0u64; 3];
        for slot in &mut numbers {
            *slot = parts.next()?.parse().ok()?;
        }
        parts.next().is_none().then_some(Version { numbers, pre })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [major, minor, patch] = self.numbers;
        write!(f, "{major}.{minor}.{patch}")?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers.cmp(&other.numbers).then_with(|| match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The release asset holding the bare CLI for this platform, or none where Culprit ships no prebuilt binary.
pub(crate) fn asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        // One universal binary for Apple silicon and Intel.
        ("macos", "aarch64" | "x86_64") => Some("culprit-cli-macos"),
        ("linux", "x86_64") => Some("culprit-cli-linux-x86_64"),
        ("windows", "x86_64") => Some("culprit-cli-windows-x64.exe"),
        _ => None,
    }
}

/// The SHA-256 a `sha256sum` listing gives for `name` (`<hex>  <name>`, or `<hex> *<name>` in binary mode).
pub(crate) fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn sibling(target: &Path, suffix: &str) -> io::Result<PathBuf> {
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the executable has no file name"))?;
    let mut name = name.to_os_string();
    name.push(suffix);
    Ok(target.with_file_name(name))
}

/// Writes `bytes` next to `target` and moves it into place, so the binary is never half-written: a failure leaves the
/// old one as it was and no staged file behind.
pub(crate) fn replace_file(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let staged = sibling(target, ".new")?;
    let written = (|| {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    move_into_place(&staged, target).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })
}

/// A rename over the running binary is fine on Unix: the process keeps the old file open until it exits.
#[cfg(not(windows))]
fn move_into_place(staged: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(staged, target)
}

/// Windows won't overwrite a running executable but will rename it: the old one moves aside to `<exe>.old` (removed on
/// the next run) and the new one takes its name.
#[cfg(windows)]
fn move_into_place(staged: &Path, target: &Path) -> io::Result<()> {
    let old = sibling(target, ".old")?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(target, &old)?;
    std::fs::rename(staged, target).inspect_err(|_| {
        let _ = std::fs::rename(&old, target);
    })
}

/// Removes the `<exe>.old` a previous update on Windows left behind. Best-effort, and a no-op elsewhere.
pub(crate) fn remove_stale_old() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe()
        && let Ok(old) = sibling(&exe, ".old")
    {
        let _ = std::fs::remove_file(old);
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        // Release downloads redirect from github.com to GitHub's asset storage. No token is sent with them.
        .max_redirects(5)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(300)))
        .user_agent(concat!("culprit/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn offline(what: &str, e: ureq::Error) -> Error {
    Error::new(ErrorCode::ConnectionFailed, format!("{what} failed: {e}"))
        .with_hint("Check the network connection and any proxy, then retry")
}

fn rate_limited(response: &ureq::http::Response<ureq::Body>) -> bool {
    let status = response.status().as_u16();
    status == 429
        || (status == 403 && response.headers().get("x-ratelimit-remaining").and_then(|v| v.to_str().ok()) == Some("0"))
}

/// The latest published release's tag. Drafts and pre-releases are not "latest" on GitHub.
fn latest_tag(agent: &ureq::Agent) -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut request =
        agent.get(&url).header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", "2022-11-28");
    // Only on this call, never on downloads: it lifts the API's rate limit for CI runners that share an address.
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        request = request.header("Authorization", &format!("Bearer {}", token.trim()));
    }
    let mut response = request.call().map_err(|e| offline("Checking for the latest release", e))?;
    if rate_limited(&response) {
        return Err(Error::new(ErrorCode::ConnectionFailed, "GitHub's API rate limit for this address is used up")
            .with_hint("Wait an hour, or set GITHUB_TOKEN to a GitHub token and retry"));
    }
    let status = response.status().as_u16();
    if status == 404 {
        return Err(Error::not_found("No published Culprit release was found")
            .with_hint(format!("See https://github.com/{REPO}/releases")));
    }
    if status != 200 {
        return Err(Error::new(
            ErrorCode::ConnectionFailed,
            format!("GitHub answered {status} for the latest release"),
        ));
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::new(ErrorCode::ConnectionFailed, format!("Reading the latest release failed: {e}")))?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    body.get("tag_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::internal("GitHub's latest release has no tag"))
}

fn download(agent: &ureq::Agent, tag: &str, name: &str) -> Result<Vec<u8>> {
    // By tag, never /latest/download: a release published between two downloads can't mix versions.
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{name}");
    let mut response = agent.get(&url).call().map_err(|e| offline(&format!("Downloading {name}"), e))?;
    match response.status().as_u16() {
        200 => {}
        404 => {
            return Err(Error::not_found(format!("Release {tag} has no {name}"))
                .with_hint(format!("See https://github.com/{REPO}/releases/tag/{tag}")));
        }
        status => {
            return Err(Error::new(ErrorCode::ConnectionFailed, format!("GitHub answered {status} for {name}")));
        }
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(|e| Error::new(ErrorCode::ConnectionFailed, format!("Downloading {name} failed: {e}")))
}

/// Where this binary lives, and a refusal when a package manager owns it: replacing its file would leave the
/// manager's records pointing at a binary it didn't install.
fn this_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| Error::internal(format!("Can't locate this binary: {e}")))?;
    let path = exe.to_string_lossy();
    if path.contains("/Cellar/") || path.contains("/nix/store/") {
        return Err(Error::new(ErrorCode::Unsupported, format!("{} is managed by a package manager", exe.display()))
            .with_hint("Update Culprit with the package manager that installed it"));
    }
    Ok(exe)
}

fn version_of(text: &str) -> Result<Version> {
    Version::parse(text).ok_or_else(|| Error::internal(format!("'{text}' isn't a version")))
}

fn release_url(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/tag/{tag}")
}

pub async fn run(args: UpdateArgs, out: &Out) -> Result<u8> {
    let json = out.format() == Format::Json;
    let outcome =
        tokio::task::spawn_blocking(move || update(args.check)).await.map_err(|e| Error::internal(e.to_string()))??;
    match outcome {
        Outcome::Checked { current, latest, tag } => {
            let available = latest > current;
            if json {
                out.value(&json!({
                    "current": current.to_string(),
                    "latest": latest.to_string(),
                    "update_available": available,
                    "url": release_url(&tag),
                }));
            } else if available {
                println!("culprit {latest} is available (you have {current}). Run `culprit update`.");
            } else {
                println!("{}", up_to_date(&current, &latest));
            }
        }
        Outcome::UpToDate { current, latest } => {
            if json {
                out.value(&json!({ "updated": false, "current": current.to_string(), "latest": latest.to_string() }));
            } else {
                println!("{}", up_to_date(&current, &latest));
            }
        }
        Outcome::Updated { from, to, path } => {
            if json {
                out.value(&json!({
                    "updated": true,
                    "from": from.to_string(),
                    "to": to.to_string(),
                    "path": path.display().to_string(),
                }));
            } else {
                println!("Updated culprit {from} → {to} ({})", path.display());
            }
        }
    }
    Ok(0)
}

fn up_to_date(current: &Version, latest: &Version) -> String {
    if current == latest {
        format!("culprit {current} is the latest")
    } else {
        format!("culprit {current} is up to date (the latest release is {latest})")
    }
}

enum Outcome {
    Checked { current: Version, latest: Version, tag: String },
    UpToDate { current: Version, latest: Version },
    Updated { from: Version, to: Version, path: PathBuf },
}

fn update(check_only: bool) -> Result<Outcome> {
    let current = version_of(CURRENT)?;
    let agent = agent();
    let tag = latest_tag(&agent)?;
    let latest = version_of(&tag)?;
    if check_only {
        return Ok(Outcome::Checked { current, latest, tag });
    }
    if latest <= current {
        return Ok(Outcome::UpToDate { current, latest });
    }
    let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        Error::new(
            ErrorCode::Unsupported,
            format!("Culprit ships no prebuilt binary for {} on {}", std::env::consts::OS, std::env::consts::ARCH),
        )
        .with_hint(format!("Build it from source: https://github.com/{REPO}#get-started"))
    })?;
    let exe = this_binary()?;

    let sums = String::from_utf8(download(&agent, &tag, "SHA256SUMS.txt")?)
        .map_err(|_| Error::internal("SHA256SUMS.txt isn't text"))?;
    let expected = checksum_for(&sums, asset)
        .ok_or_else(|| Error::internal(format!("Release {tag}'s SHA256SUMS.txt doesn't list {asset}")))?;
    let binary = download(&agent, &tag, asset)?;
    let actual = sha256_hex(&binary);
    if actual != expected {
        return Err(Error::new(
            ErrorCode::Internal,
            format!(
                "{asset} from {tag} doesn't match its SHA-256 (expected {expected}, got {actual}); nothing was changed"
            ),
        )
        .with_hint("Retry; if it keeps failing, download the release by hand and report it"));
    }

    replace_file(&exe, &binary).map_err(|e| {
        let dir = exe.parent().map(|d| d.display().to_string()).unwrap_or_default();
        if e.kind() == io::ErrorKind::PermissionDenied {
            Error::new(
                ErrorCode::PermissionDenied,
                format!("Can't replace {}: this user can't write to {dir}", exe.display()),
            )
            .with_hint(if cfg!(windows) {
                "Run it again from an elevated prompt, or install culprit somewhere you own".to_string()
            } else {
                "Run it again with sudo (sudo culprit update), or install culprit somewhere you own, such as ~/.local/bin"
                    .to_string()
            })
        } else {
            Error::internal(format!("Replacing {} failed: {e}; nothing was changed", exe.display()))
        }
    })?;
    Ok(Outcome::Updated { from: current, to: latest, path: exe })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn reads_versions_and_orders_them_as_releases() {
        assert_eq!(v("v0.2.0"), v("0.2.0"));
        assert_eq!(v("0.2.0+build.7").to_string(), "0.2.0");
        assert!(v("0.2.1") > v("0.2.0"));
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        // A pre-release comes before the release it leads up to.
        assert!(v("0.2.0-beta.1") < v("0.2.0"));
        assert!(v("0.2.0-beta.1") > v("0.1.9"));
        assert_eq!(v("0.2.1-rc.1").to_string(), "0.2.1-rc.1");
        for wrong in ["", "0.2", "0.2.0.1", "x.y.z", "0.2.0-", "v"] {
            assert!(Version::parse(wrong).is_none(), "{wrong}");
        }
    }

    #[test]
    fn picks_the_binary_for_the_platform() {
        assert_eq!(asset_for("macos", "aarch64"), Some("culprit-cli-macos"));
        assert_eq!(asset_for("macos", "x86_64"), Some("culprit-cli-macos"));
        assert_eq!(asset_for("linux", "x86_64"), Some("culprit-cli-linux-x86_64"));
        assert_eq!(asset_for("windows", "x86_64"), Some("culprit-cli-windows-x64.exe"));
        assert_eq!(asset_for("linux", "aarch64"), None);
        assert_eq!(asset_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn reads_a_sha256sum_listing() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums =
            format!("{a}  culprit-cli-linux-x86_64\n{b} *culprit-cli-macos\nnot-a-hash  culprit-cli-windows-x64.exe\n");
        assert_eq!(checksum_for(&sums, "culprit-cli-linux-x86_64"), Some(a));
        assert_eq!(checksum_for(&sums, "culprit-cli-macos"), Some("b".repeat(64)));
        assert_eq!(checksum_for(&sums, "culprit-cli-windows-x64.exe"), None);
        assert_eq!(checksum_for(&sums, "culprit-cli-linux-x86_64.tar.gz"), None);
    }

    #[test]
    fn hashes_with_sha256() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn replaces_a_file_whole_and_leaves_nothing_staged() {
        let dir = std::env::temp_dir().join(format!("culprit-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("culprit");
        std::fs::write(&target, b"old binary").unwrap();

        replace_file(&target, b"new binary").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        assert!(!dir.join("culprit.new").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_it_cant_write_leaves_the_binary_as_it_was() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("culprit-update-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("culprit");
        std::fs::write(&target, b"old binary").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = replace_file(&target, b"new binary");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root can write anywhere, so the refusal is only observable as another user.
        if std::fs::read(&target).unwrap() == b"old binary" {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert!(!dir.join("culprit.new").exists());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
