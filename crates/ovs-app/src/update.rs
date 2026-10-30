//! Update checker: asks GitHub for the latest published release (drafts and pre-releases don't
//! count) and compares its version with this build.
//!
//! The request runs `curl` (built into macOS and Windows 10+) on a background thread, so the app
//! needs no HTTP library and startup never waits for the network. Only the main window's start
//! checks; tests never touch the network.

use gpui_kit::{App, Global};

/// The GitHub repository releases are published to.
pub const REPO: &str = "darshan5/openvisql";

/// A release newer than this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// Without the leading `v`, e.g. `0.2.0`.
    pub version: String,
    /// The release's web page.
    pub url: String,
}

/// The outcome of a check: a newer release, or `None` if this is the latest.
pub type CheckResult = Result<Option<Update>, String>;

/// A newer release found this run, shown as File > Update Available and in the tab bar.
#[derive(Default)]
pub struct AvailableUpdate(pub Option<Update>);

impl Global for AvailableUpdate {}

pub fn available(cx: &App) -> Option<&Update> {
    cx.try_global::<AvailableUpdate>()?.0.as_ref()
}

/// Remembers `update` and rebuilds the menus so File shows it.
pub fn set_available(update: Option<Update>, cx: &mut App) {
    cx.set_global(AvailableUpdate(update));
    crate::set_menus(cx);
    cx.refresh_windows();
}

/// This build's version.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Where all releases are listed.
pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases")
}

/// The numeric parts of `1.2.3` (a `v` prefix and `-beta` style suffixes are ignored).
fn parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// Is version `latest` newer than `current`?
pub fn is_newer(latest: &str, current: &str) -> bool {
    let (a, b) = (parts(latest), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// Reads GitHub's "latest release" JSON; `Some` if it's newer than `current`.
pub fn parse_latest(json: &str, current: &str) -> CheckResult {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("Unexpected reply from GitHub: {e}"))?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or_else(|| match v["message"].as_str() {
            // e.g. "Not Found" before the first release is published.
            Some(m) => format!("GitHub: {m}"),
            None => "No release found on GitHub".to_owned(),
        })?;
    let version = tag.trim_start_matches('v').to_owned();
    let url = v["html_url"]
        .as_str()
        .map_or_else(releases_page, str::to_owned);
    Ok(is_newer(&version, current).then_some(Update { version, url }))
}

/// Asks GitHub for the latest release. Blocking; run it on the background executor.
pub fn check() -> CheckResult {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--location",
        "--max-time",
        "15",
        "--header",
        "Accept: application/vnd.github+json",
        "--user-agent",
        concat!("openvisql/", env!("CARGO_PKG_VERSION")),
        &url,
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW: no console window flashing up.
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("Couldn't run curl to check for updates: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(if err.is_empty() {
            "Couldn't reach GitHub.".to_owned()
        } else {
            format!("Couldn't reach GitHub: {err}")
        });
    }
    parse_latest(&String::from_utf8_lossy(&out.stdout), current_version())
}

/// The startup check, if it's turned on. Failures are silent; Settings > Check now reports them.
pub fn check_at_startup(cx: &mut App) {
    if !crate::settings::prefs(cx).check_for_updates {
        return;
    }
    let job = cx.background_executor().spawn(async { check() });
    cx.spawn(async move |cx| {
        if let Ok(Some(update)) = job.await {
            cx.update(|cx| set_available(Some(update), cx));
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0", "0.9.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("1.0.0-beta", "1.0.0"));
    }

    #[test]
    fn reads_github_replies() {
        let newer = r#"{"tag_name": "v0.3.1", "html_url": "https://example.com/r/0.3.1"}"#;
        assert_eq!(
            parse_latest(newer, "0.1.0"),
            Ok(Some(Update {
                version: "0.3.1".into(),
                url: "https://example.com/r/0.3.1".into()
            }))
        );
        assert_eq!(parse_latest(newer, "0.3.1"), Ok(None));
        let none = r#"{"message": "Not Found", "status": "404"}"#;
        assert_eq!(parse_latest(none, "0.1.0"), Err("GitHub: Not Found".into()));
        assert!(parse_latest("<html>", "0.1.0").is_err());
    }

    /// Asks the real GitHub: `cargo test -p ovs-app live_check -- --ignored --nocapture`
    #[test]
    #[ignore = "network"]
    fn live_check() {
        println!("{:?}", check());
    }
}
