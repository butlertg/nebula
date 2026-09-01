//! What a finished run leaves behind to read.
//!
//! Two renderers, both pure so they can be tested without a repo, a daemon,
//! or a clock: [`render_report`] for one run's `report.md`, and
//! [`render_digest`] for the "what happened while I was asleep" page across
//! every run in a window.
//!
//! Local time is stamped here rather than client-side for the same reason
//! `next_run_at` is computed daemon-side: the daemon is the process that
//! knows when things happened, and a TUI on the far end of an ssh hop is in
//! a different timezone as often as not.

use crate::git::DiffSummary;
use chrono::{Local, TimeZone};
use nebula_core::{Task, TaskRun, TaskRunStatus};

/// Files listed individually in a report before it stops and counts the
/// rest. A migration touching four hundred files is a number, not a list.
const MAX_LISTED_FILES: usize = 60;

/// Everything a report needs that is not on the run row itself.
pub struct ReportInput<'a> {
    pub run: &'a TaskRun,
    /// The task as it was when the run ended — the prompts, mostly. None
    /// when the task has been deleted since (the run row outlives the read).
    pub task: Option<&'a Task>,
    pub diff: Option<&'a DiffSummary>,
    /// `summary.md`, if the agent wrote one.
    pub agent_summary: Option<&'a str>,
    pub transcript_bytes: u64,
}

/// One run's `report.md`: the morning read, in the order somebody actually
/// wants it — what happened, what changed, then the detail behind both.
pub fn render_report(input: &ReportInput<'_>) -> String {
    let run = input.run;
    let mut out = String::new();
    out.push_str(&format!(
        "# {} — {}\n\n",
        run.task_name,
        stamp(run.started_at)
    ));

    let verdict = match run.status {
        TaskRunStatus::Completed => "✓",
        TaskRunStatus::Running => "…",
        _ => "✗",
    };
    out.push_str(&format!(
        "**{} {}** — {}\n\n",
        verdict,
        run.status.as_str(),
        run.outcome
    ));

    // The agent's own account comes before nebula's bookkeeping: it is the
    // part written by something that knows what the work was for.
    match input.agent_summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(summary) => {
            out.push_str("## What the agent said\n\n");
            out.push_str(summary);
            out.push_str("\n\n");
        }
        None => {
            out.push_str("## What the agent said\n\nNothing — it wrote no summary.md. ");
            out.push_str("The transcript is the only account of its reasoning.\n\n");
        }
    }

    out.push_str(&render_changes(input.diff));

    out.push_str("## The run\n\n");
    out.push_str(&format!("- started: {}\n", stamp(run.started_at)));
    if run.ended_at > 0 {
        out.push_str(&format!(
            "- ended: {} ({})\n",
            stamp(run.ended_at),
            dur_label(run.duration_ms(run.ended_at))
        ));
    } else {
        out.push_str("- ended: still running\n");
    }
    out.push_str(&format!(
        "- iterations: {} of {}\n",
        run.iterations_done, run.iterations_planned
    ));
    if let Some(task) = input.task {
        out.push_str(&format!(
            "- agent: {}{}\n",
            task.kind.as_str(),
            match (task.model.as_deref(), task.effort.as_deref()) {
                (None, None) => String::new(),
                (m, e) => format!(
                    " ({})",
                    [m, e]
                        .iter()
                        .filter_map(|v| *v)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        ));
        if task.unattended {
            out.push_str("- unattended: permission prompts were skipped\n");
        }
    }
    out.push_str(&format!(
        "- checkout: {}{}\n",
        run.worktree_path.display(),
        if run.branch.is_empty() {
            String::new()
        } else {
            format!(" (on {})", run.branch)
        }
    ));
    if let Some(session) = &run.agent_id {
        out.push_str(&format!("- session: {session}\n"));
    }
    out.push('\n');

    out.push_str("## Where the evidence is\n\n");
    match (&run.base_ref, &run.head_ref) {
        (Some(base), Some(head)) => {
            out.push_str(&format!(
                "- the run's diff, in full:\n  ```\n  git -C {} diff {} {}\n  ```\n",
                run.worktree_path.display(),
                short(base),
                short(head)
            ));
            out.push_str(&format!(
                "- refs kept for it: `refs/nebula/runs/{}/base` and `…/head`\n",
                run.id
            ));
        }
        _ => out.push_str(
            "- no diff: the run's before/after refs could not be written \
             (see the daemon log)\n",
        ),
    }
    if let Some(snapshot) = &run.snapshot {
        out.push_str(&format!(
            "- committed on a branch of its own: `{snapshot}` — note that this commit is the \
             whole working tree, so anything else left uncommitted in that checkout is in it \
             too (the diff above is the run's own work)\n"
        ));
    }
    if input.transcript_bytes > 0 {
        out.push_str(&format!(
            "- everything the session printed: `{}` ({})\n",
            run.transcript_path().display(),
            size_label(input.transcript_bytes)
        ));
    }
    out.push('\n');

    if let Some(task) = input.task {
        out.push_str("## What it was asked\n\n");
        out.push_str(&fenced(&task.prompt));
        if let Some(final_prompt) = task.final_prompt.as_deref() {
            if task.iterations > 1 {
                out.push_str("\nOn its last turn, instead:\n\n");
                out.push_str(&fenced(final_prompt));
            }
        }
    }
    out
}

fn render_changes(diff: Option<&DiffSummary>) -> String {
    let mut out = String::new();
    let Some(diff) = diff else {
        out.push_str("## What changed\n\nUnknown — nebula could not diff this run.\n\n");
        return out;
    };
    if diff.files.is_empty() {
        out.push_str("## What changed\n\nNothing that git can see.\n\n");
        return out;
    }
    out.push_str(&format!(
        "## What changed — {}\n\n```\n",
        diffstat_label(diff.files.len() as u32, diff.insertions, diff.deletions)
    ));
    let width = diff
        .files
        .iter()
        .take(MAX_LISTED_FILES)
        .map(|f| f.path.len())
        .max()
        .unwrap_or(0)
        .min(72);
    for file in diff.files.iter().take(MAX_LISTED_FILES) {
        out.push_str(&format!(
            "{:<3} {:<width$}  +{} −{}\n",
            file.status, file.path, file.insertions, file.deletions
        ));
    }
    if diff.files.len() > MAX_LISTED_FILES {
        out.push_str(&format!(
            "… and {} more\n",
            diff.files.len() - MAX_LISTED_FILES
        ));
    }
    out.push_str("```\n\n");
    out
}

/// "12 files +430 −58" — short enough for a task's outcome line, which is
/// where it also gets used.
pub fn diffstat_label(files: u32, insertions: u32, deletions: u32) -> String {
    if files == 0 {
        return "no file changes".to_string();
    }
    format!(
        "{} file{} +{} −{}",
        files,
        if files == 1 { "" } else { "s" },
        insertions,
        deletions
    )
}

/// The overnight page: every run in the window, worst news first, each one
/// pointing at its own report.
pub fn render_digest(runs: &[TaskRun], since_ms: i64, now: i64) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Task runs since {}\n\n_as of {}_\n\n",
        stamp(since_ms),
        stamp(now)
    ));
    if runs.is_empty() {
        out.push_str("No runs in that window.\n");
        return out;
    }

    let count = |s: TaskRunStatus| runs.iter().filter(|r| r.status == s).count();
    let (done, stalled, stopped, failed, running) = (
        count(TaskRunStatus::Completed),
        count(TaskRunStatus::Stalled),
        count(TaskRunStatus::Stopped),
        count(TaskRunStatus::Failed),
        count(TaskRunStatus::Running),
    );
    let files: u32 = runs.iter().map(|r| r.files_changed).sum();
    let insertions: u32 = runs.iter().map(|r| r.insertions).sum();
    let deletions: u32 = runs.iter().map(|r| r.deletions).sum();
    out.push_str(&format!(
        "{} run{} — {} completed, {} stalled, {} stopped early, {} failed to start, \
         {} still going. {} in all.\n\n",
        runs.len(),
        if runs.len() == 1 { "" } else { "s" },
        done,
        stalled,
        stopped,
        failed,
        running,
        diffstat_label(files, insertions, deletions)
    ));

    // Anything that did not simply work is listed first: the point of the
    // page is to be scanned, and a completed run needs no attention.
    let mut ordered: Vec<&TaskRun> = runs.iter().collect();
    ordered.sort_by_key(|r| (digest_rank(r.status), -r.started_at));
    for run in ordered {
        out.push_str(&format!(
            "## {} {} — {}\n\n",
            match run.status {
                TaskRunStatus::Completed => "✓",
                TaskRunStatus::Running => "…",
                _ => "✗",
            },
            run.task_name,
            stamp(run.started_at)
        ));
        out.push_str(&format!(
            "- {} after {} ({} of {} turns)\n",
            run.outcome,
            dur_label(run.duration_ms(now)),
            run.iterations_done,
            run.iterations_planned
        ));
        out.push_str(&format!(
            "- {}{}\n",
            diffstat_label(run.files_changed, run.insertions, run.deletions),
            match &run.snapshot {
                Some(s) => format!(" · `{s}`"),
                None => String::new(),
            }
        ));
        out.push_str(&format!("- report: `{}`\n\n", run.report_path().display()));
    }
    out
}

/// Sort key for the digest: trouble, then unfinished business, then the
/// runs that just worked.
fn digest_rank(status: TaskRunStatus) -> u8 {
    match status {
        TaskRunStatus::Failed => 0,
        TaskRunStatus::Stalled => 1,
        TaskRunStatus::Stopped => 2,
        TaskRunStatus::Running => 3,
        TaskRunStatus::Completed => 4,
    }
}

fn fenced(text: &str) -> String {
    format!("```\n{}\n```\n", text.trim_end())
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// Epoch ms as local wall-clock time, which is the only form a human
/// reading "did the 2am run go?" can check against their own night.
pub fn stamp(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => format!("epoch+{ms}ms"),
    }
}

/// "38s" / "41m 16s" / "6h 02m" — a duration somebody reads once.
pub fn dur_label(ms: i64) -> String {
    let secs = (ms / 1_000).max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3_600, (s % 3_600) / 60),
    }
}

fn size_label(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.1} MiB", b / (KIB * KIB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::FileChange;
    use nebula_core::{AgentId, AgentKind, ProjectId, TaskId, TaskRunId, TaskTarget};
    use std::path::PathBuf;

    fn run() -> TaskRun {
        TaskRun {
            id: TaskRunId("run1".into()),
            task_id: TaskId("t1".into()),
            project_id: ProjectId("p1".into()),
            task_name: "nightly notes".into(),
            agent_id: Some(AgentId("a1".into())),
            started_at: 1_700_000_000_000,
            ended_at: 1_700_000_600_000,
            status: TaskRunStatus::Completed,
            outcome: "ran 3 of 3".into(),
            iterations_planned: 3,
            iterations_done: 3,
            worktree_path: PathBuf::from("/repo"),
            branch: "main".into(),
            base_ref: Some("aaaaaaaaaaaaaaaaaaaa".into()),
            head_ref: Some("bbbbbbbbbbbbbbbbbbbb".into()),
            snapshot: Some("task/nightly-notes/20260831-024119 ae5ecd7".into()),
            files_changed: 2,
            insertions: 30,
            deletions: 4,
            dir: PathBuf::from("/runs/nightly-notes/one"),
        }
    }

    fn task() -> Task {
        Task {
            id: TaskId("t1".into()),
            project_id: ProjectId("p1".into()),
            name: "nightly notes".into(),
            prompt: "tidy the changelog".into(),
            kind: AgentKind::Claude,
            model: Some("opus".into()),
            effort: None,
            cron: Some("0 0 2 * * *".into()),
            iterations: 3,
            unattended: true,
            final_prompt: Some("stop here and summarise".into()),
            stall_timeout_secs: 1800,
            commit_on_finish: true,
            target: TaskTarget::Root,
            enabled: true,
            last_run_at: 0,
            next_run_at: 0,
            last_outcome: None,
            last_agent_id: None,
            created_at: 0,
            sort_order: 0,
        }
    }

    fn diff() -> DiffSummary {
        DiffSummary {
            files: vec![
                FileChange {
                    status: "M".into(),
                    path: "CHANGELOG.md".into(),
                    insertions: 28,
                    deletions: 4,
                },
                FileChange {
                    status: "A".into(),
                    path: "notes/today.md".into(),
                    insertions: 2,
                    deletions: 0,
                },
            ],
            insertions: 30,
            deletions: 4,
        }
    }

    /// The report is the thing the user reads instead of having watched the
    /// run, so every question they'd have asked has to be answered in it.
    #[test]
    fn a_report_answers_what_happened_and_what_changed() {
        let (run, task, diff) = (run(), task(), diff());
        let text = render_report(&ReportInput {
            run: &run,
            task: Some(&task),
            diff: Some(&diff),
            agent_summary: Some("Rewrote the changelog intro.\n"),
            transcript_bytes: 4 * 1024 * 1024,
        });
        assert!(text.contains("# nightly notes"), "{text}");
        assert!(text.contains("completed"), "{text}");
        assert!(text.contains("ran 3 of 3"));
        assert!(text.contains("Rewrote the changelog intro."));
        assert!(text.contains("2 files +30 −4"));
        assert!(text.contains("CHANGELOG.md"));
        assert!(text.contains("git -C /repo diff aaaaaaaaaaaa bbbbbbbbbbbb"));
        assert!(text.contains("task/nightly-notes/20260831-024119 ae5ecd7"));
        assert!(text.contains("4.0 MiB"));
        assert!(text.contains("tidy the changelog"));
        assert!(text.contains("stop here and summarise"));
        assert!(text.contains("10m 00s"), "the duration is stated: {text}");
    }

    /// A run whose agent wrote nothing must not read as a run that changed
    /// nothing, and vice versa — these are the two silences that matter.
    #[test]
    fn a_report_distinguishes_no_summary_from_no_changes() {
        let mut run = run();
        run.files_changed = 0;
        let task = task();
        let text = render_report(&ReportInput {
            run: &run,
            task: Some(&task),
            diff: Some(&DiffSummary::default()),
            agent_summary: None,
            transcript_bytes: 0,
        });
        assert!(text.contains("it wrote no summary.md"), "{text}");
        assert!(text.contains("Nothing that git can see"), "{text}");

        let text = render_report(&ReportInput {
            run: &run,
            task: Some(&task),
            diff: None,
            agent_summary: Some("  "),
            transcript_bytes: 0,
        });
        assert!(
            text.contains("could not diff this run"),
            "an absent diff is not an empty one: {text}"
        );
        assert!(
            text.contains("it wrote no summary.md"),
            "whitespace is not a summary: {text}"
        );
    }

    /// A report for a deleted task still has to render: the row outlives the
    /// task, and the prompt sections are simply absent.
    #[test]
    fn a_report_survives_the_task_being_deleted() {
        let run = run();
        let text = render_report(&ReportInput {
            run: &run,
            task: None,
            diff: None,
            agent_summary: None,
            transcript_bytes: 0,
        });
        assert!(text.contains("# nightly notes"));
        assert!(!text.contains("What it was asked"));
    }

    #[test]
    fn a_long_file_list_is_counted_not_printed() {
        let files: Vec<FileChange> = (0..100)
            .map(|i| FileChange {
                status: "M".into(),
                path: format!("src/file{i}.rs"),
                insertions: 1,
                deletions: 1,
            })
            .collect();
        let diff = DiffSummary {
            insertions: 100,
            deletions: 100,
            files,
        };
        let run = run();
        let text = render_report(&ReportInput {
            run: &run,
            task: None,
            diff: Some(&diff),
            agent_summary: None,
            transcript_bytes: 0,
        });
        assert!(text.contains("… and 40 more"), "{text}");
        assert!(!text.contains("src/file99.rs"));
    }

    /// The digest is scanned, not read: trouble has to be at the top even
    /// when it happened first.
    #[test]
    fn the_digest_leads_with_what_went_wrong() {
        let mut ok = run();
        ok.task_name = "worked".into();
        ok.started_at = 1_700_000_900_000;
        let mut bad = run();
        bad.task_name = "stalled one".into();
        bad.id = TaskRunId("run2".into());
        bad.status = TaskRunStatus::Stalled;
        bad.outcome = "stalled: no turn ended in 30m at iteration 2 of 3".into();
        bad.iterations_done = 2;
        bad.started_at = 1_700_000_000_000;

        let text = render_digest(&[ok, bad], 1_699_999_000_000, 1_700_001_000_000);
        let stalled_at = text.find("stalled one").expect("listed");
        let worked_at = text.find("worked").expect("listed");
        assert!(stalled_at < worked_at, "{text}");
        assert!(text.contains("2 runs — 1 completed, 1 stalled"), "{text}");
        assert!(
            text.contains("4 files +60 −8"),
            "totals across runs: {text}"
        );
        assert!(text.contains("report: `/runs/nightly-notes/one/report.md`"));
    }

    #[test]
    fn an_empty_digest_says_so_rather_than_rendering_a_blank_page() {
        let text = render_digest(&[], 0, 1_700_000_000_000);
        assert!(text.contains("No runs in that window."), "{text}");
    }

    #[test]
    fn durations_read_as_durations() {
        assert_eq!(dur_label(0), "0s");
        assert_eq!(dur_label(38_000), "38s");
        assert_eq!(dur_label(2_476_000), "41m 16s");
        assert_eq!(dur_label(21_720_000), "6h 02m");
        assert_eq!(dur_label(-5), "0s");
    }

    #[test]
    fn a_diffstat_label_is_singular_when_it_should_be() {
        assert_eq!(diffstat_label(0, 0, 0), "no file changes");
        assert_eq!(diffstat_label(1, 2, 0), "1 file +2 −0");
        assert_eq!(diffstat_label(9, 2, 3), "9 files +2 −3");
    }
}
