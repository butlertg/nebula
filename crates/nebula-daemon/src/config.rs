//! User settings, read from `paths::config_path()` (JSON). Loaded fresh at
//! each use so edits apply without restarting the daemon. A missing file or
//! unknown fields fall back to defaults; a malformed file is logged and
//! ignored rather than failing the operation that read it.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Run `git init` after AddProject creates a missing directory.
    pub git_init_on_create: bool,
    /// Pre-spawn agent CLIs while the user is still naming the session so
    /// creation feels instant. Costs one idle CLI process per warm slot.
    pub prewarm_agents: bool,
    /// Pre-spawn a worktree's dead sessions when the user's selection rests
    /// on it, so attaching shows an already-booted screen instead of a
    /// booting shell. Costs idle shell/CLI processes for sessions the user
    /// may never open.
    pub prewarm_sessions: bool,
    /// Kill idle session PTYs in worktrees no client is looking at once
    /// they've gone unwatched this long: "1m" | "5m" | "15m" | "30m" | "1h"
    /// ("off" disables reaping entirely; any `<n>s`/`<n>m`/`<n>h` works).
    /// Bounds what prewarming and walked-away-from sessions cost. Pinned
    /// agents, running or feedback-waiting agents, and terminals with a
    /// command running are spared; a reaped session revives on the next
    /// attach or prewarm (agents resume their conversation). Malformed
    /// values fall back to the 5m default.
    pub session_idle_timeout: String,
    /// When to stop the host idle-sleeping so automation can actually run:
    /// "scheduled" (whenever work is in flight *or* an enabled task is
    /// waiting for its window — the default, so an 02:00 task finds the
    /// machine awake at 02:00), "runs" (only while work is in flight: a task
    /// run, or an agent in the middle of a turn), or "off". macOS only, and only as far as a power assertion reaches: the
    /// machine still sleeps on a closed lid, and on battery it ignores the
    /// system-sleep assertion. Malformed values fall back to "scheduled",
    /// and `NEBULA_KEEP_AWAKE` in the daemon's environment overrides this.
    pub keep_awake: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            git_init_on_create: true,
            prewarm_agents: true,
            prewarm_sessions: true,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT.into(),
            keep_awake: DEFAULT_KEEP_AWAKE.into(),
        }
    }
}

/// Fallback for `session_idle_timeout` when the value is malformed.
pub const DEFAULT_SESSION_IDLE_TIMEOUT: &str = "5m";

/// Fallback for `keep_awake` when the value is malformed.
pub const DEFAULT_KEEP_AWAKE: &str = "scheduled";

impl Config {
    pub fn load() -> Self {
        let path = nebula_core::paths::config_path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_else(|err| {
            tracing::warn!("ignoring malformed {}: {err}", path.display());
            Self::default()
        })
    }

    /// `keep_awake` parsed; an unrecognised value falls back to the default
    /// rather than to "off" — a typo must not quietly let the host sleep
    /// through the night's work.
    pub fn keep_awake(&self) -> crate::power::KeepAwakePolicy {
        crate::power::KeepAwakePolicy::parse(&self.keep_awake).unwrap_or_default()
    }

    /// `session_idle_timeout` parsed to a duration; None = reaping disabled.
    pub fn session_idle_timeout(&self) -> Option<std::time::Duration> {
        parse_timeout(&self.session_idle_timeout)
            .unwrap_or_else(|| parse_timeout(DEFAULT_SESSION_IDLE_TIMEOUT).expect("default parses"))
    }
}

/// "off"/"0" → Some(None); "<n>s"/"<n>m"/"<n>h" → Some(Some(d));
/// malformed → None (caller falls back to the default).
#[allow(clippy::option_option)]
fn parse_timeout(s: &str) -> Option<Option<std::time::Duration>> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("off") || s == "0" {
        return Some(None);
    }
    let (digits, unit_secs) = match s.strip_suffix(['s', 'S']) {
        Some(d) => (d, 1),
        None => match s.strip_suffix(['m', 'M']) {
            Some(d) => (d, 60),
            None => (s.strip_suffix(['h', 'H'])?, 3_600),
        },
    };
    let n: u64 = digits.trim().parse().ok()?;
    if n == 0 {
        return Some(None);
    }
    Some(Some(std::time::Duration::from_secs(n * unit_secs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_enable_git_init() {
        assert!(Config::default().git_init_on_create);
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.git_init_on_create);
        let cfg: Config = serde_json::from_str(r#"{"git_init_on_create": false}"#).unwrap();
        assert!(!cfg.git_init_on_create);
    }

    #[test]
    fn defaults_enable_prewarm_and_allow_opt_out() {
        assert!(Config::default().prewarm_agents);
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.prewarm_agents);
        let cfg: Config = serde_json::from_str(r#"{"prewarm_agents": false}"#).unwrap();
        assert!(!cfg.prewarm_agents);
    }

    #[test]
    fn session_idle_timeout_parses_and_falls_back() {
        use std::time::Duration;
        let timeout = |v: &str| {
            let cfg: Config =
                serde_json::from_str(&format!(r#"{{"session_idle_timeout": "{v}"}}"#)).unwrap();
            cfg.session_idle_timeout()
        };
        assert_eq!(timeout("1m"), Some(Duration::from_secs(60)));
        assert_eq!(timeout("5m"), Some(Duration::from_secs(300)));
        assert_eq!(timeout("15m"), Some(Duration::from_secs(900)));
        assert_eq!(timeout("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(timeout("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(
            timeout("2s"),
            Some(Duration::from_secs(2)),
            "seconds for tests"
        );
        assert_eq!(timeout("off"), None);
        assert_eq!(timeout("0"), None);
        // Malformed values fall back to the default, not to disabled.
        assert_eq!(timeout("soon"), Some(Duration::from_secs(300)));
        assert_eq!(
            Config::default().session_idle_timeout(),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn keep_awake_defaults_to_scheduled_and_falls_back_on_a_typo() {
        use crate::power::KeepAwakePolicy;
        let policy = |v: &str| {
            let cfg: Config = serde_json::from_str(&format!(r#"{{"keep_awake": "{v}"}}"#)).unwrap();
            cfg.keep_awake()
        };
        assert_eq!(Config::default().keep_awake(), KeepAwakePolicy::Scheduled);
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.keep_awake(), KeepAwakePolicy::Scheduled);
        assert_eq!(policy("runs"), KeepAwakePolicy::Runs);
        assert_eq!(policy("off"), KeepAwakePolicy::Off);
        assert_eq!(
            policy("nope"),
            KeepAwakePolicy::Scheduled,
            "a typo must not silently disable it"
        );
    }

    #[test]
    fn defaults_enable_session_prewarm_and_allow_opt_out() {
        assert!(Config::default().prewarm_sessions);
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.prewarm_sessions);
        let cfg: Config = serde_json::from_str(r#"{"prewarm_sessions": false}"#).unwrap();
        assert!(!cfg.prewarm_sessions);
    }
}
