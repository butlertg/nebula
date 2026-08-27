use crate::ids::{AgentId, LinkId, ProjectId, TaskId, TerminalId, WorkspaceId, WorktreeId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Never run yet (gray).
    Fresh,
    /// Actively working (yellow).
    Running,
    /// Turn complete (green).
    Finished,
    /// Waiting on the user: permission prompt or question (red).
    NeedsFeedback,
    /// Process died with a nonzero exit while working.
    Terminated,
    /// Daemon restarted while the agent was live; PTY is gone.
    Disconnected,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Fresh => "fresh",
            AgentStatus::Running => "running",
            AgentStatus::Finished => "finished",
            AgentStatus::NeedsFeedback => "needs_feedback",
            AgentStatus::Terminated => "terminated",
            AgentStatus::Disconnected => "disconnected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "fresh" => AgentStatus::Fresh,
            "running" => AgentStatus::Running,
            "finished" => AgentStatus::Finished,
            "needs_feedback" => AgentStatus::NeedsFeedback,
            "terminated" => AgentStatus::Terminated,
            "disconnected" => AgentStatus::Disconnected,
            _ => return None,
        })
    }
}

/// Which agent CLI a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    #[default]
    Claude,
    Codex,
    Cursor,
}

impl AgentKind {
    /// Every kind, for callers that must cover all of them (menus, the
    /// boot-time CLI probe warm) and should fail to compile if one is added.
    pub const ALL: [AgentKind; 3] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Cursor];

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Cursor => "cursor",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "claude" => AgentKind::Claude,
            "codex" => AgentKind::Codex,
            "cursor" => AgentKind::Cursor,
            _ => return None,
        })
    }

    /// Binary the kind launches. Differs from `as_str` only for Cursor,
    /// whose agent CLI ships as `cursor-agent` (`cursor` opens the editor).
    pub fn cli_program(&self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Cursor => "cursor-agent",
        }
    }
}

/// A named group of projects. Each nebula instance has exactly one
/// workspace open and shows only that workspace's projects; the daemon
/// remembers the last one opened as the workspace a fresh instance boots
/// into, not as a scope every client shares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// The workspace this project lives in. Defaults to the built-in
    /// `default` workspace for rows that predate workspaces.
    #[serde(default)]
    pub workspace_id: WorkspaceId,
    pub repo_path: PathBuf,
    pub sort_order: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub id: WorktreeId,
    pub project_id: ProjectId,
    pub path: PathBuf,
    pub branch: String,
    pub is_main: bool,
    /// Pinned worktrees sort into their own PINNED group in the worktrees list.
    #[serde(default)]
    pub pinned: bool,
    pub sort_order: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub worktree_id: WorktreeId,
    pub name: String,
    pub status: AgentStatus,
    pub archived: bool,
    /// Epoch ms of the last archive; 0 = never archived (or archived before
    /// this field existed). Orders the ARCHIVED group newest-first.
    #[serde(default)]
    pub archived_at: i64,
    /// Pinned agents sort into their own PINNED group in the sessions list.
    #[serde(default)]
    pub pinned: bool,
    /// Finished a turn (running or needs-feedback → finished) that no client
    /// has looked at since. The Projects and Worktrees rows count these so
    /// the user knows how many terminals to go read; the pane landing on
    /// the session clears it (`ClientRequest::MarkAgentSeen`). Only ever
    /// true on a finished, unarchived row — leaving `finished` clears it.
    #[serde(default)]
    pub unseen: bool,
    /// Epoch ms of the last status change; 0 = unknown (pre-upgrade rows or
    /// never-run agents). Drives the TUI's RECENT session group.
    #[serde(default)]
    pub status_changed_at: i64,
    #[serde(default)]
    pub kind: AgentKind,
    /// Model the CLI is launched with (claude `--model` / codex `-m`);
    /// None = the CLI's own default. Persisted so respawns keep it.
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort the CLI is launched with (claude `--effort` /
    /// codex `model_reasoning_effort`); None = the CLI's own default.
    #[serde(default)]
    pub effort: Option<String>,
    /// CLI session id used for resume (claude, codex, or cursor, per `kind`).
    pub session_id: Option<String>,
    /// The Claude Cloud session this row launched (`claude --cloud <task>`
    /// prints the id as it creates one). Only cloud rows have it. Restarting
    /// such a row while it has no local `session_id` re-enters the cloud
    /// session — `claude --cloud <id>`, or `claude --teleport <id>` when the
    /// account cannot attach — instead of booting a bare local CLI.
    #[serde(default)]
    pub cloud_session_id: Option<String>,
    pub sort_order: i64,
    /// True when the daemon currently holds a live PTY for this agent.
    pub alive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalTab {
    pub id: TerminalId,
    pub worktree_id: WorktreeId,
    pub name: String,
    pub sort_order: i64,
    /// True when the daemon currently holds a live PTY for this terminal.
    pub alive: bool,
}

/// A URL pinned to a worktree — the pull request, the ticket, the design
/// doc for whatever that checkout is for. Nebula never fetches these; they
/// are bookmarks the user opens in a browser from the Sessions panel. The
/// open pull request shown above them is discovered from git, not stored
/// here (see the TUI's `PullRequest`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: LinkId,
    pub worktree_id: WorktreeId,
    /// Always http(s) — normalized on the way in, so opening one can never
    /// hand the OS a scheme the user didn't intend.
    pub url: String,
    pub sort_order: i64,
}

/// A unit of unattended work defined on a project: a prompt, the agent kind
/// to run it, where to run it, and optionally when. The daemon owns every
/// field below `enabled` — the TUI renders them and never computes them, so
/// it needs no cron parser of its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub name: String,
    /// Delivered to the agent verbatim each iteration — a sentence, or a
    /// slash command / skill invocation like `/code-review`. Typed at the
    /// CLI's input box rather than passed as argv, so a slash command runs
    /// the same way it would for a human.
    pub prompt: String,
    pub kind: AgentKind,
    /// Model and effort the run launches with; None = the CLI's own default,
    /// matching `Agent`.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Cron expression, seconds-first as the `cron` crate parses it.
    /// None = the task only ever runs when asked by hand.
    #[serde(default)]
    pub cron: Option<String>,
    /// How many times the prompt is delivered per run, each one waiting for
    /// the previous turn to end. 1 = no loop. The only stop condition there
    /// is: a run always ends after this many turns.
    pub iterations: u32,
    /// Launch the agent with its CLI's skip-permissions flag, because
    /// nothing is watching to answer a prompt. Off by default.
    #[serde(default)]
    pub unattended: bool,
    pub target: TaskTarget,
    pub enabled: bool,
    /// Epoch ms of the last run's start; 0 = never run.
    #[serde(default)]
    pub last_run_at: i64,
    /// Epoch ms this task is next due, computed from `cron` by the daemon.
    /// 0 = not scheduled (no cron, or disabled).
    #[serde(default)]
    pub next_run_at: i64,
    /// What the last run did, for the pane's status line: "ok", or the
    /// reason it could not start.
    #[serde(default)]
    pub last_outcome: Option<String>,
    /// The session the last run spawned, so the pane can point at it.
    #[serde(default)]
    pub last_agent_id: Option<AgentId>,
    pub created_at: i64,
    pub sort_order: i64,
}

/// Which checkout a task's run happens in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskTarget {
    /// The project's main checkout.
    Root,
    /// One specific worktree. A run whose worktree has been deleted since
    /// fails with that as its outcome rather than falling back to root.
    Worktree(WorktreeId),
    /// A fresh worktree per run, named after the task.
    NewWorktree,
}

impl TaskTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskTarget::Root => "root",
            TaskTarget::Worktree(_) => "worktree",
            TaskTarget::NewWorktree => "new worktree",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Entity {
    Workspace(Workspace),
    Project(Project),
    Worktree(Worktree),
    Agent(Agent),
    Terminal(TerminalTab),
    Link(Link),
    Task(Task),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntityId {
    Workspace(WorkspaceId),
    Project(ProjectId),
    Worktree(WorktreeId),
    Agent(AgentId),
    Terminal(TerminalId),
    Link(LinkId),
    Task(TaskId),
}
