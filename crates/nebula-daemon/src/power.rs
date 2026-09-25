//! Keeping the host awake for unattended work, and noticing when it slept
//! anyway.
//!
//! Two halves of one problem. [`KeepAwake`] holds a macOS power assertion so
//! a Mac with work in hand does not idle-sleep out from under it: an agent
//! mid-turn when its user walks away, or a 02:00 task, which is only
//! overnight work if the machine is still running at 02:00. [`SleepWatch`] covers the case the
//! assertion cannot stop (a closed lid, a machine on battery, an explicit
//! `pmset sleepnow`) by measuring the gap between the wall clock and the
//! monotonic clock, which stops while the host is suspended. That gap is how
//! long the machine was asleep, and it is time no watchdog may charge to a
//! run: the agent's process was frozen for all of it and picks up exactly
//! where it left off when the host comes back.
//!
//! That the monotonic clock stops is the load-bearing assumption, and on
//! macOS it is not the obvious one: Darwin's `CLOCK_MONOTONIC` *does* keep
//! counting through a suspend, and only `CLOCK_UPTIME_RAW` does not. Rust's
//! `Instant` uses the latter — measured here on a host 16 days up with ~10
//! of them asleep, where `Instant` read 537,133s against `CLOCK_MONOTONIC`'s
//! 1,389,213s and `CLOCK_UPTIME_RAW`'s 537,133s. Linux's `CLOCK_MONOTONIC`,
//! which `Instant` uses there, likewise excludes suspend. If a future `std`
//! ever moved macOS onto `CLOCK_MONOTONIC`, `observe` would simply stop
//! reporting sleep and the watchdog would go back to charging runs for it.
//!
//! Everything here is pure or process-shaped, never time-reading: `observe`
//! takes both clocks as arguments, the same convention `schedule.rs` uses,
//! so a test can walk an eight-hour suspend without waiting for one.

use std::process::Child;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::time::Instant;

/// The smallest wall-vs-monotonic gap read as a suspend rather than as
/// ordinary scheduling jitter or an NTP step. The scheduler ticks every 30s
/// and a tick can be seconds late under load, so the floor sits well clear
/// of that; a suspend shorter than this is not worth forgiving anyway.
pub const SLEEP_FLOOR_MS: i64 = 10_000;

/// Watches the two clocks for the divergence that means "the host was
/// suspended". Fed one observation per scheduler tick.
#[derive(Default)]
pub struct SleepWatch {
    /// The clocks as of the previous observation. None until the first.
    last: Option<(Instant, i64)>,
}

impl SleepWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this tick's clocks and return how long the host spent asleep
    /// since the previous one, in ms — 0 when it did not sleep, when the
    /// gap is under [`SLEEP_FLOOR_MS`], or on the very first observation
    /// (there is nothing to compare against, and a daemon that has just
    /// started has no runs to forgive).
    ///
    /// A wall clock stepped *backwards* reads as a negative gap and is
    /// reported as no sleep at all, rather than as a negative one.
    pub fn observe(&mut self, mono: Instant, wall_ms: i64) -> i64 {
        let slept = match self.last {
            Some((prev_mono, prev_wall)) => {
                let monotonic = mono.saturating_duration_since(prev_mono).as_millis() as i64;
                let wall = wall_ms.saturating_sub(prev_wall);
                let gap = wall - monotonic;
                if gap >= SLEEP_FLOOR_MS {
                    gap
                } else {
                    0
                }
            }
            None => 0,
        };
        self.last = Some((mono, wall_ms));
        slept
    }
}

/// When nebula asks the host to stay awake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepAwakePolicy {
    /// Never hold an assertion; the host sleeps on its own schedule.
    Off,
    /// Hold one only while work is in flight: a task run, or an agent
    /// session in the middle of a turn.
    Runs,
    /// Hold one while work is in flight, and also whenever an enabled task
    /// with a schedule is waiting for its window. The default, because a
    /// window that arrives while the host is asleep does not run until it
    /// wakes, however well the run itself would have survived.
    #[default]
    Scheduled,
}

impl KeepAwakePolicy {
    /// Parse a config value. None for anything unrecognised, so the caller
    /// can fall back to the default rather than silently disabling it.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "never" | "false" => Some(Self::Off),
            "runs" | "run" => Some(Self::Runs),
            "scheduled" | "always" | "true" => Some(Self::Scheduled),
            _ => None,
        }
    }

    /// Should an assertion be held, given what is armed right now?
    /// `working` is any work in flight: a task run, or an agent mid-turn.
    pub fn wants_awake(self, working: bool, tasks_scheduled: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Runs => working,
            Self::Scheduled => working || tasks_scheduled,
        }
    }
}

/// Holds, at most, one power assertion on the host.
///
/// On macOS the assertion is a `caffeinate` child tied to the daemon's own
/// pid with `-w`, so it lets go even if the daemon is killed outright rather
/// than leaving a machine that will not sleep. `-s -i -m` covers system,
/// idle, and disk sleep; the display is deliberately left out, so the screen
/// still goes dark on an overnight run.
///
/// Note what an assertion cannot do: macOS ignores the system-sleep one on
/// battery, and closing the lid sleeps the machine regardless. Preventing
/// those needs `pmset`, which needs root, so the honest reach of this is "an
/// awake Mac on power stays awake". Anything past that is [`SleepWatch`]'s
/// problem.
#[derive(Default)]
pub struct KeepAwake {
    /// The live assertion, on platforms that have one.
    child: Option<Child>,
    /// Why it is held, kept so the state is only logged when it changes.
    reason: Option<String>,
}

impl KeepAwake {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while an assertion is held.
    pub fn held(&self) -> bool {
        self.reason.is_some()
    }

    /// Bring the assertion into line with `want`: `Some(reason)` holds one,
    /// None drops it. Idempotent — called on every scheduler tick — and
    /// self-healing: an assertion whose process has gone away (killed by
    /// hand, or by a `killall caffeinate`) is noticed and taken again.
    pub fn set(&mut self, want: Option<String>) {
        if let Some(child) = self.child.as_mut() {
            // Reaps it too, so a released assertion is not left a zombie.
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                self.child = None;
                self.reason = None;
            }
        }
        match want {
            Some(reason) => {
                if self.held() {
                    // Already awake; only the wording may have changed, and
                    // that is not worth a log line or a respawn.
                    self.reason = Some(reason);
                    return;
                }
                match spawn_assertion() {
                    Ok(child) => {
                        tracing::info!(%reason, "holding the host awake");
                        self.child = child;
                        self.reason = Some(reason);
                    }
                    Err(e) => {
                        // Worth one warning, not one per tick: leave the
                        // state released so the next tick tries again only
                        // after something else changes it.
                        tracing::warn!(error = %e, "could not hold the host awake");
                    }
                }
            }
            None => self.release(),
        }
    }

    /// Drop the assertion if one is held.
    pub fn release(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if self.reason.take().is_some() {
            tracing::info!("released the host's stay-awake assertion");
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.release();
    }
}

/// Take a platform assertion. Ok(None) means the platform has none to take,
/// which is still a held assertion as far as the state machine is concerned
/// — there is nothing more nebula can do about sleep there, and pretending
/// otherwise would respawn nothing on every tick.
#[cfg(target_os = "macos")]
fn spawn_assertion() -> std::io::Result<Option<Child>> {
    Command::new("caffeinate")
        .args([
            "-s",
            "-i",
            "-m",
            // Let go when this daemon does, however it goes.
            "-w",
            &std::process::id().to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Some)
}

#[cfg(not(target_os = "macos"))]
fn spawn_assertion() -> std::io::Result<Option<Child>> {
    // No portable equivalent worth shelling out for: `systemd-inhibit` is
    // not universal and does not cover a laptop's own suspend policy.
    tracing::debug!("no stay-awake assertion is available on this platform");
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;

    /// The whole detection, stated as the case it exists for: both clocks
    /// advance together while the host is running, and the wall clock runs
    /// away from the monotonic one across a suspend by exactly the time
    /// spent asleep.
    #[test]
    fn a_suspend_is_the_gap_between_the_two_clocks() {
        let mut watch = SleepWatch::new();
        let mono = Instant::now();
        let wall = 1_772_323_200_000;

        assert_eq!(watch.observe(mono, wall), 0, "nothing to compare against");
        // A normal tick: 30s on both clocks.
        let mono = mono + Duration::from_secs(30);
        assert_eq!(watch.observe(mono, wall + 30_000), 0, "an ordinary tick");
        // The lid closes for eight hours: the monotonic clock records the
        // 30s tick that fired on wake, the wall clock records the night.
        let mono = mono + Duration::from_secs(30);
        let slept = watch.observe(mono, wall + 30_000 + 8 * HOUR + 30_000);
        assert_eq!(slept, 8 * HOUR, "the night is the gap, not the tick");
        // And the tick after wake is ordinary again.
        let mono = mono + Duration::from_secs(30);
        assert_eq!(
            watch.observe(mono, wall + 30_000 + 8 * HOUR + 60_000),
            0,
            "the suspend is reported once, not on every later tick"
        );
    }

    /// Tick lag is not a suspend. A scheduler tick that lands a few seconds
    /// late under load must not hand every in-flight run free watchdog time.
    #[test]
    fn ordinary_jitter_is_not_read_as_sleep() {
        let mut watch = SleepWatch::new();
        let mono = Instant::now();
        let wall = 1_772_323_200_000;
        watch.observe(mono, wall);
        let mono = mono + Duration::from_secs(30);
        // 9s of clock skew, just under the floor.
        assert_eq!(watch.observe(mono, wall + 30_000 + 9_000), 0);
        // And 11s, just over it, is.
        let mono = mono + Duration::from_secs(30);
        assert_eq!(watch.observe(mono, wall + 60_000 + 9_000 + 11_000), 11_000);
    }

    /// A wall clock stepped backwards (an NTP correction, a user setting the
    /// date) must read as no sleep rather than as a negative one, which
    /// would run a run's watchdog *forward*.
    #[test]
    fn a_backwards_clock_is_not_negative_sleep() {
        let mut watch = SleepWatch::new();
        let mono = Instant::now();
        let wall = 1_772_323_200_000;
        watch.observe(mono, wall);
        let mono = mono + Duration::from_secs(30);
        assert_eq!(watch.observe(mono, wall - HOUR), 0);
    }

    #[test]
    fn the_policy_says_who_arms_the_assertion() {
        use KeepAwakePolicy::*;
        // (work in flight, tasks scheduled)
        assert!(!Off.wants_awake(true, true), "off is off");
        assert!(Runs.wants_awake(true, false));
        assert!(
            !Runs.wants_awake(false, true),
            "a task merely waiting for 02:00 does not hold the host up under `runs`"
        );
        assert!(
            Scheduled.wants_awake(false, true),
            "this is the point of it"
        );
        assert!(Scheduled.wants_awake(true, false));
        assert!(
            !Scheduled.wants_awake(false, false),
            "nothing armed, no hold"
        );
    }

    #[test]
    fn the_policy_parses_what_a_person_would_write() {
        assert_eq!(
            KeepAwakePolicy::parse("scheduled"),
            Some(KeepAwakePolicy::Scheduled)
        );
        assert_eq!(
            KeepAwakePolicy::parse(" Runs "),
            Some(KeepAwakePolicy::Runs)
        );
        assert_eq!(KeepAwakePolicy::parse("OFF"), Some(KeepAwakePolicy::Off));
        assert_eq!(
            KeepAwakePolicy::parse("sometimes"),
            None,
            "falls back, not off"
        );
        assert_eq!(KeepAwakePolicy::default(), KeepAwakePolicy::Scheduled);
    }

    /// Holding is idempotent and releasing is complete — the assertion must
    /// not accumulate a `caffeinate` per tick, and must actually let go.
    #[test]
    fn the_assertion_is_taken_once_and_really_released() {
        let mut awake = KeepAwake::new();
        assert!(!awake.held());
        awake.set(Some("a task run is in flight".into()));
        assert!(awake.held());
        let first = awake.child.as_ref().map(|c| c.id());
        awake.set(Some("2 tasks are scheduled".into()));
        assert!(awake.held(), "still held");
        assert_eq!(
            awake.child.as_ref().map(|c| c.id()),
            first,
            "a second reason must not spawn a second assertion"
        );
        awake.set(None);
        assert!(!awake.held());
        assert!(
            awake.child.is_none(),
            "the process is gone, not just forgotten"
        );
    }
}
