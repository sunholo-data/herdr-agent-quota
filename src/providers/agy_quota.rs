//! Agy subscription quota, read from `agy -p /quota`.
//!
//! The statusLine hook (`providers::agy`) only reports quota after a turn in a
//! pane that has the hook installed. `/quota` is a slash command the Agy CLI
//! answers itself, without a model call, so it can report the plan's windows
//! with no pane at all. It prints one row per pool and window, with the fields
//! separated by tabs (shown here as `|`):
//!
//! ```text
//! Gemini Models | Weekly Limit Remaining | 97% | 2026-11-30T14:09:17Z
//! Gemini Models | Five Hour Limit Remaining | 95% | 2026-11-23T19:09:17Z
//! Claude and GPT models | Weekly Limit Remaining | 100% | 2026-11-30T14:21:28Z
//! Claude and GPT models | Five Hour Limit Remaining | 100% | 2026-11-23T19:21:28Z
//! ```
//!
//! The percentage is what is **left**; this module stores used = 100 − left,
//! like every other collector, and the sidebar's `--quota-percent` setting
//! decides which side is shown.
//!
//! Agy keeps one sign-in per `HOME`, so the `HOME` the command runs under
//! decides whose quota is read. It defaults to this process's own home and can
//! be pointed elsewhere (`HERDR_AGENT_QUOTA_AGY_HOME`, or the `agy-home` file
//! in the plugin config directory).
//!
//! Everything fails closed: a signed-out CLI, a row whose direction or number
//! cannot be read, or a run that times out yields an error, never 0% or 100%.

use crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use crate::prefs;
use crate::process::run_program_with_deadline;
use crate::providers::ProviderError;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Overrides the `HOME` the quota command runs under.
pub const HOME_ENV: &str = "HERDR_AGENT_QUOTA_AGY_HOME";
/// Overrides the `agy` executable.
pub const BIN_ENV: &str = "HERDR_AGENT_QUOTA_AGY_BIN";

/// Agy's own limit for the print-mode turn. `/quota` is a network call that
/// measured 2–4 s; this only bounds a hung one.
const PRINT_TIMEOUT: &str = "20s";
/// Hard wall-clock budget for the whole process, enforced by killing its
/// process group, in case the CLI ignores its own print timeout.
const COMMAND_BUDGET: Duration = Duration::from_secs(30);
/// Four rows are a few hundred bytes; anything far larger is not a report.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// A conservative PATH for the child, which gets no other inherited variable.
const CHILD_PATH: &str = "/usr/bin:/bin";

/// Where the quota command runs, and as whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSource {
    pub executable: PathBuf,
    pub home: PathBuf,
    /// The home was configured rather than defaulted. A configured home is
    /// the user saying whose quota to show, so it outranks a statusLine
    /// observation from whichever Agy pane ticked last.
    pub home_is_configured: bool,
}

impl QuotaSource {
    /// Resolve from the environment, then the plugin config directory, then
    /// the defaults (this process's home; `agy` on `PATH`, then
    /// `~/.local/bin/agy`).
    pub fn resolve() -> Result<Self, ProviderError> {
        let real_home = directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .ok_or_else(|| ProviderError::Unavailable("home directory unknown".to_string()))?;
        Self::from_parts(
            setting(HOME_ENV, prefs::AGY_HOME),
            setting(BIN_ENV, prefs::AGY_BIN),
            &real_home,
            std::env::var_os("PATH"),
        )
    }

    /// Pure resolution, so precedence and validation are testable without
    /// touching the process environment.
    pub fn from_parts(
        home_override: Option<String>,
        bin_override: Option<String>,
        real_home: &Path,
        path_var: Option<OsString>,
    ) -> Result<Self, ProviderError> {
        let home_is_configured = home_override.is_some();
        let home = match home_override {
            Some(value) => {
                let home = expand(&value, real_home)?;
                if !home.is_dir() {
                    return Err(ProviderError::Unavailable(format!(
                        "configured Agy home {} is not a directory",
                        home.display()
                    )));
                }
                home
            }
            None => real_home.to_path_buf(),
        };
        let executable = match bin_override {
            Some(value) => {
                let executable = expand(&value, real_home)?;
                if !executable.is_file() {
                    return Err(ProviderError::Unavailable(format!(
                        "configured agy executable {} does not exist",
                        executable.display()
                    )));
                }
                executable
            }
            None => find_on_path(path_var.as_deref())
                .or_else(|| {
                    Some(real_home.join(".local/bin/agy")).filter(|candidate| candidate.is_file())
                })
                .ok_or_else(|| ProviderError::Unavailable("agy CLI not found".to_string()))?,
        };
        Ok(Self {
            executable,
            home,
            home_is_configured,
        })
    }

    /// The credential scope a snapshot read through this source belongs to.
    ///
    /// Agy exposes no account id without reading its token store, and the
    /// sign-in is per `HOME`, so the `HOME` is the identity. Stamping it means
    /// pointing the plugin at another home never keeps showing the previous
    /// home's numbers as "last good".
    pub fn account_id(&self) -> String {
        format!("agy-home:{}", self.home.display())
    }
}

/// Whether an Agy home was configured at all, valid or not.
pub fn home_is_configured() -> bool {
    setting(HOME_ENV, prefs::AGY_HOME).is_some()
}

/// The credential scope Agy quota is currently read for, if resolvable.
pub fn current_account_id() -> Option<String> {
    QuotaSource::resolve()
        .ok()
        .map(|source| source.account_id())
}

/// Run `agy -p /quota` for one source and parse the answer.
pub fn fetch(source: &QuotaSource, now_unix: u64) -> Result<ProviderSnapshot, ProviderError> {
    let mut command = Command::new(&source.executable);
    command
        .args(["--print-timeout", PRINT_TIMEOUT, "-p", "/quota"])
        // Nothing is inherited: an API key in the environment would switch
        // Agy to its key route, and the self-updater must never run from a
        // quota read.
        .env_clear()
        .env("HOME", &source.home)
        .env("PATH", CHILD_PATH)
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .current_dir(std::env::temp_dir());
    let output = run_program_with_deadline(command, COMMAND_BUDGET)
        .map_err(|error| ProviderError::Request(format!("{error:#}")))?;
    if output.timed_out {
        return Err(ProviderError::Request("agy /quota timed out".to_string()));
    }
    if output.stdout.len() > MAX_OUTPUT_BYTES {
        return Err(ProviderError::UnsupportedResponse(
            "agy /quota output too large".to_string(),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_quota(&text, now_unix);
    match (parsed, output.exit_code) {
        (Ok(snapshot), _) => Ok(snapshot.with_account_id(Some(source.account_id()))),
        // The signed-out state is reported whatever the exit code was; Agy
        // has been seen to exit 0 while asking for a login.
        (Err(ProviderError::MissingCredentials), _) => Err(ProviderError::MissingCredentials),
        (Err(_), Some(code)) if code != 0 => Err(ProviderError::Unavailable(format!(
            "agy /quota exited with status {code}"
        ))),
        (Err(error), _) => Err(error),
    }
}

/// Parse `agy -p /quota` output.
///
/// Agy keeps separate Gemini and Claude/GPT pools and `/quota` cannot say
/// which one a pane is drawing from, so each window shows the tighter pool —
/// the same conservative rule the statusLine parser applies to an
/// unrecognised model. Lines that are not four tab-separated fields are
/// ignored (banners, blank lines); a row that is shaped like a quota row but
/// cannot be read is an error, because skipping it could hide the tighter
/// pool.
pub fn parse_quota(output: &str, fetched_at_unix: u64) -> Result<ProviderSnapshot, ProviderError> {
    if is_signed_out(output) {
        return Err(ProviderError::MissingCredentials);
    }
    let mut tightest: Vec<(WindowKind, f64, Option<ResetAt>)> = Vec::new();
    for line in output.lines() {
        let fields = line.split('\t').map(str::trim).collect::<Vec<_>>();
        let [_pool, label, percent, reset] = fields.as_slice() else {
            continue;
        };
        let Some(kind) = window_kind(label) else {
            continue;
        };
        let used = used_percent(label, percent)?;
        let reset = ResetAt::parse_rfc3339(reset);
        match tightest
            .iter_mut()
            .find(|(existing, _, _)| *existing == kind)
        {
            Some(entry) if used > entry.1 => *entry = (kind, used, reset),
            Some(_) => {}
            None => tightest.push((kind, used, reset)),
        }
    }
    let mut windows = Vec::new();
    for kind in [WindowKind::FiveHour, WindowKind::Weekly] {
        if let Some((_, used, reset)) = tightest.iter().find(|(existing, _, _)| *existing == kind) {
            windows.push(
                UsageWindow::new(kind, *used, *reset)
                    .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))?,
            );
        }
    }
    if windows.is_empty() {
        return Err(ProviderError::UnsupportedResponse(
            "agy /quota reported no five-hour or weekly window".to_string(),
        ));
    }
    Ok(ProviderSnapshot::new(
        Provider::Agy,
        windows,
        fetched_at_unix,
    ))
}

/// Agy asks for a login in prose ("Please sign in…", "Authentication
/// required. Please visit the URL to log in:"), on either stream and
/// sometimes with exit status 0.
fn is_signed_out(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "sign in",
        "not signed in",
        "authentication required",
        "log in",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

fn window_kind(label: &str) -> Option<WindowKind> {
    let lower = label.to_ascii_lowercase();
    if lower.contains("five hour") || lower.contains("5 hour") || lower.contains("5h") {
        Some(WindowKind::FiveHour)
    } else if lower.contains("weekly") || lower.contains("7 day") || lower.contains("7d") {
        Some(WindowKind::Weekly)
    } else {
        None
    }
}

/// Used percent for one row. The direction comes from the label, never from
/// the number: a row that says neither "remaining" nor "used" is rejected
/// rather than guessed, because a guess is wrong by up to 100 points.
fn used_percent(label: &str, percent: &str) -> Result<f64, ProviderError> {
    let value = percent
        .strip_suffix('%')
        .unwrap_or(percent)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        .ok_or_else(|| {
            ProviderError::UnsupportedResponse(format!("unreadable quota percentage {percent:?}"))
        })?;
    let lower = label.to_ascii_lowercase();
    if lower.contains("remaining") {
        Ok(100.0 - value)
    } else if lower.contains("used") {
        Ok(value)
    } else {
        Err(ProviderError::UnsupportedResponse(format!(
            "quota row {label:?} does not say whether it is remaining or used"
        )))
    }
}

/// An environment variable wins (direct CLI runs); otherwise the plugin
/// config file, which is the only channel that reaches Herdr's events and
/// actions. Blank values are unset.
fn setting(env: &str, pref: &str) -> Option<String> {
    std::env::var(env)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| prefs::read(pref))
}

/// Expand a leading `~/` and require an absolute path. A relative path would
/// be resolved against whatever directory Herdr happened to start in.
fn expand(value: &str, real_home: &Path) -> Result<PathBuf, ProviderError> {
    let path = if value == "~" {
        real_home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        real_home.join(rest)
    } else {
        PathBuf::from(value)
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ProviderError::Unavailable(format!(
            "Agy path {value:?} must be absolute or start with ~/"
        )))
    }
}

fn find_on_path(path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?)
        .map(|directory| directory.join("agy"))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED_IN: &str = "Gemini Models\tWeekly Limit Remaining\t97%\t2026-11-30T14:09:17Z\n\
Gemini Models\tFive Hour Limit Remaining\t95%\t2026-11-23T19:09:17Z\n\
Claude and GPT models\tWeekly Limit Remaining\t100%\t2026-11-30T14:21:28Z\n\
Claude and GPT models\tFive Hour Limit Remaining\t100%\t2026-11-23T19:21:28Z\n";

    fn used(snapshot: &ProviderSnapshot, kind: WindowKind) -> f64 {
        snapshot.window(kind).unwrap().used_percent
    }

    #[test]
    fn remaining_percent_is_stored_as_used() {
        let snapshot = parse_quota(SIGNED_IN, 1).unwrap();
        assert_eq!(snapshot.provider, Provider::Agy);
        assert_eq!(used(&snapshot, WindowKind::FiveHour), 5.0);
        assert_eq!(used(&snapshot, WindowKind::Weekly), 3.0);
        assert_eq!(
            snapshot
                .window(WindowKind::Weekly)
                .unwrap()
                .remaining_percent,
            97.0
        );
    }

    #[test]
    fn each_window_shows_the_tighter_pool_with_its_own_reset() {
        let output = "Gemini Models\tFive Hour Limit Remaining\t80%\t2026-11-23T19:09:17Z\n\
Claude and GPT models\tFive Hour Limit Remaining\t10%\t2026-11-23T20:00:00Z\n";
        let snapshot = parse_quota(output, 1).unwrap();
        let window = snapshot.window(WindowKind::FiveHour).unwrap();
        assert_eq!(window.remaining_percent, 10.0);
        assert_eq!(
            window.resets_at,
            ResetAt::parse_rfc3339("2026-11-23T20:00:00Z")
        );
        assert!(snapshot.window(WindowKind::Weekly).is_none());
    }

    #[test]
    fn signed_out_output_is_missing_credentials_not_a_percentage() {
        for output in [
            "Error: Please sign in to view available models.\n",
            "Authentication required. Please visit the URL to log in:\nhttps://example.invalid/login\n",
        ] {
            assert!(matches!(
                parse_quota(output, 1),
                Err(ProviderError::MissingCredentials)
            ));
        }
    }

    #[test]
    fn a_row_without_a_direction_is_rejected_rather_than_guessed() {
        let output = "Gemini Models\tWeekly Limit\t97%\t2026-11-30T14:09:17Z\n";
        assert!(matches!(
            parse_quota(output, 1),
            Err(ProviderError::UnsupportedResponse(_))
        ));
    }

    #[test]
    fn a_used_row_is_not_inverted() {
        let output = "Gemini Models\tWeekly Limit Used\t30%\t2026-11-30T14:09:17Z\n";
        assert_eq!(
            used(&parse_quota(output, 1).unwrap(), WindowKind::Weekly),
            30.0
        );
    }

    #[test]
    fn an_unreadable_or_out_of_range_percentage_fails_closed() {
        for percent in ["lots", "101%", "-1%", "NaN%", ""] {
            let output =
                format!("Gemini Models\tWeekly Limit Remaining\t{percent}\t2026-11-30T14:09:17Z\n");
            assert!(parse_quota(&output, 1).is_err(), "{percent:?} was accepted");
        }
    }

    #[test]
    fn banners_and_unknown_windows_are_ignored_but_not_alone() {
        let output = format!("Antigravity CLI 1.5.15\n\nGemini Models\tDaily Limit Remaining\t50%\t2026-11-24T00:00:00Z\n{SIGNED_IN}");
        let snapshot = parse_quota(&output, 1).unwrap();
        assert_eq!(snapshot.windows.len(), 2);
        assert!(parse_quota("Antigravity CLI 1.5.15\n", 1).is_err());
        assert!(parse_quota("", 1).is_err());
    }

    /// A stand-in `agy` that answers only when called exactly the way the
    /// collector promises: `-p /quota`, the chosen HOME, auto-update off, no
    /// inherited API key, and stdin at end-of-file.
    fn fake_agy(directory: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join("agy");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn source(executable: PathBuf, home: &Path) -> QuotaSource {
        QuotaSource {
            executable,
            home: home.to_path_buf(),
            home_is_configured: true,
        }
    }

    #[test]
    fn fetch_runs_agy_with_the_chosen_home_and_auto_update_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("subscription");
        std::fs::create_dir(&home).unwrap();
        let script = format!(
            r#"[ "$*" = "--print-timeout {PRINT_TIMEOUT} -p /quota" ] || exit 9
[ "$HOME" = "{home}" ] || exit 8
[ "$AGY_CLI_DISABLE_AUTO_UPDATE" = "true" ] || exit 7
[ -z "$GEMINI_API_KEY$HERDR_PLUGIN_STATE_DIR" ] || exit 6
[ -z "$(cat)" ] || exit 5
printf 'Gemini Models\tWeekly Limit Remaining\t97%%\t2026-11-30T14:09:17Z\n'"#,
            home = home.display()
        );
        let agy = fake_agy(directory.path(), &script);
        let source = source(agy, &home);
        let snapshot = fetch(&source, 1).unwrap();
        assert_eq!(
            snapshot
                .window(WindowKind::Weekly)
                .unwrap()
                .remaining_percent,
            97.0
        );
        assert_eq!(snapshot.account_id, Some(source.account_id()));
    }

    #[test]
    fn fetch_reports_a_login_prompt_on_stderr_with_status_zero_as_signed_out() {
        let directory = tempfile::tempdir().unwrap();
        let agy = fake_agy(
            directory.path(),
            "echo 'Authentication required. Please visit the URL to log in:' >&2; exit 0",
        );
        assert!(matches!(
            fetch(&source(agy, directory.path()), 1),
            Err(ProviderError::MissingCredentials)
        ));
    }

    #[test]
    fn fetch_reports_a_failing_cli_without_echoing_its_output() {
        let directory = tempfile::tempdir().unwrap();
        let agy = fake_agy(directory.path(), "echo 'secret-ish detail'; exit 2");
        let error = fetch(&source(agy, directory.path()), 1).unwrap_err();
        assert!(matches!(error, ProviderError::Unavailable(_)));
        assert!(!error.to_string().contains("secret-ish"));
    }

    #[test]
    fn home_defaults_to_the_real_home_and_a_configured_one_is_flagged() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("agy");
        std::fs::write(&bin, "").unwrap();
        let bin = bin.display().to_string();

        let default = QuotaSource::from_parts(None, Some(bin.clone()), home.path(), None).unwrap();
        assert_eq!(default.home, home.path());
        assert!(!default.home_is_configured);

        let agy_home = home.path().join("subscription");
        std::fs::create_dir(&agy_home).unwrap();
        let configured = QuotaSource::from_parts(
            Some("~/subscription".to_string()),
            Some(bin),
            home.path(),
            None,
        )
        .unwrap();
        assert_eq!(configured.home, agy_home);
        assert!(configured.home_is_configured);
        assert_ne!(default.account_id(), configured.account_id());
    }

    #[test]
    fn a_missing_or_relative_configured_home_is_an_error_not_a_fallback() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("agy");
        std::fs::write(&bin, "").unwrap();
        let bin = Some(bin.display().to_string());
        for value in ["~/does-not-exist", "relative/home"] {
            assert!(
                QuotaSource::from_parts(Some(value.to_string()), bin.clone(), home.path(), None)
                    .is_err(),
                "{value} was accepted"
            );
        }
    }

    #[test]
    fn executable_comes_from_path_then_local_bin() {
        let home = tempfile::tempdir().unwrap();
        let on_path = tempfile::tempdir().unwrap();
        std::fs::write(on_path.path().join("agy"), "").unwrap();
        let local_bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        std::fs::write(local_bin.join("agy"), "").unwrap();

        let found = QuotaSource::from_parts(
            None,
            None,
            home.path(),
            Some(on_path.path().as_os_str().to_os_string()),
        )
        .unwrap();
        assert_eq!(found.executable, on_path.path().join("agy"));

        let fallback = QuotaSource::from_parts(None, None, home.path(), None).unwrap();
        assert_eq!(fallback.executable, local_bin.join("agy"));

        let empty = tempfile::tempdir().unwrap();
        assert!(QuotaSource::from_parts(None, None, empty.path(), None).is_err());
    }
}
