//! The daemon's world: persisted entity tree + live PTY sessions, and the
//! operations the IPC surface exposes over them.

use crate::git;
use crate::hooks::{self, HookEnv};
use crate::pty::{PtyEvent, PtySession, SpawnSpec};
use crate::report::{self, ReportInput};
use crate::status::{AgentStatusMachine, Effect, HookEvent};
use crate::store::Store;
use anyhow::{bail, Context, Result};
use chrono::TimeZone;
use nebula_core::{
    paths, Agent, AgentId, AgentKind, AgentStatus, EnterOutcome, Entity, EntityId, Link, LinkId,
    Project, ProjectId, RunArtifact, ServerEvent, SessionRef, Task, TaskId, TaskRun, TaskRunId,
    TaskRunStatus, TaskSpec, TaskTarget, TerminalId, TerminalTab, Workspace, WorkspaceId, Worktree,
    WorktreeId, MAX_CLOUD_PROMPT_BYTES, MAX_TASK_ITERATIONS,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// A warm agent CLI older than this is reaped — it holds memory and its
/// conversation context grows stale.
const PREWARM_MAX_AGE: Duration = Duration::from_secs(15 * 60);
/// A live same-spec warm CLI older than this is recycled (killed and
/// re-booted fresh) when its slot is re-requested, instead of being kept.
/// Clients keep-warm the selected worktree on a cadence shorter than
/// `PREWARM_MAX_AGE - PREWARM_RECYCLE_AGE`, so a slot they still care about
/// is always refreshed before the reaper can empty it.
const PREWARM_RECYCLE_AGE: Duration = Duration::from_secs(10 * 60);
/// Hook events buffered on a warm session before its row exists (oldest
/// dropped beyond this).
const PREWARM_HOOK_BUFFER_CAP: usize = 64;

/// How long a freshly spawned agent CLI is given to draw its input box
/// before a task's first prompt is pasted at it. The CLIs boot a full TUI;
/// bytes written before it is listening are simply lost. Overridable so the
/// e2e doesn't have to wait it out.
const TASK_PROMPT_DELAY_MS: u64 = 2_500;
/// Gap between pasting a prompt and pressing Enter. The CLIs process a
/// bracketed paste asynchronously, and a submit that arrives in the same
/// read as the paste lands as a newline inside the text instead.
const SUBMIT_GAP: Duration = Duration::from_millis(250);
/// Ceiling on how long a run may sit before its *first* turn even starts.
/// Much shorter than the task's own watchdog, because this is a different
/// failure: a turn that is running can legitimately take half an hour, but a
/// turn that has not started means the CLI never accepted the prompt at all.
/// The common cause is Claude Code's "is this a project you trust?" dialog,
/// which a checkout it has not seen before opens with — it swallows the
/// paste, answers no hook, and would otherwise burn the whole window.
const FIRST_TURN_TIMEOUT_SECS: u32 = 120;

fn first_prompt_delay() -> Duration {
    let ms = std::env::var("NEBULA_TASK_PROMPT_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(TASK_PROMPT_DELAY_MS);
    Duration::from_millis(ms)
}

/// A task run in flight: which task it came from, how many of its prompts
/// have been claimed, and how many there are in total.
struct LoopState {
    task_id: TaskId,
    /// The row and directory this run is recording itself into. Carried on
    /// the loop so every stop path — completion, stall, question, dead
    /// session — finishes the same record without having to guess which of a
    /// task's runs it is ending.
    run_id: TaskRunId,
    /// Iterations claimed, incremented as a delivery is scheduled rather
    /// than when its bytes land. A write that fails therefore costs an
    /// iteration instead of repeating one — the right way round, since the
    /// only way it fails is a PTY that has gone away.
    delivered: u32,
    total: u32,
    /// True between a delivery's paste and its submit. A turn-end arriving
    /// in that window belongs to the previous turn (or is a duplicate Stop):
    /// it cannot be the end of a turn the prompt in flight hasn't started
    /// yet, so it is ignored rather than consuming the next iteration.
    in_flight: bool,
    /// Epoch ms of the last thing this run did — a prompt delivered or a
    /// turn ended. The watchdog measures from here, so a long but healthy
    /// turn is never mistaken for a wedged one.
    last_progress_at: i64,
}

/// The transient, per-launch half of a spawn — everything that is not a
/// persisted `Agent` field. Grouped rather than passed as three more
/// positional arguments, all of which would be `None`/`false` at most call
/// sites.
#[derive(Default)]
struct SpawnOpts<'a> {
    /// Makes this a Claude Cloud launch (initial dispatch, or an
    /// attach/teleport of the session it created).
    cloud: Option<CloudLaunch<'a>>,
    /// A first turn the CLI submits on its own (the relocation notice a
    /// `nebula worktree` respawn opens with).
    initial_prompt: Option<&'a str>,
    /// Launch with the CLI's skip-permissions flag because nothing is
    /// watching to answer a prompt (a task marked unattended).
    unattended: bool,
    /// Write everything the child prints to this file as well as the ring.
    /// Only task runs ask for one — see `pty::transcript`.
    transcript: Option<PathBuf>,
}

/// Argv-shaping flags that aren't properties of the Agent row.
#[derive(Debug, Clone, Copy, Default)]
struct LaunchFlags {
    /// Append nebula's worktree guidance to Claude's system prompt.
    guidance: bool,
    /// Skip permission prompts: `--dangerously-skip-permissions` for Claude,
    /// which is the only kind that doesn't already skip them.
    unattended: bool,
}

impl LaunchFlags {
    /// A session a person is sitting in front of: guidance on, permissions
    /// enforced. Production builds always spell the flags out at the one
    /// call site that has them; this is the shorthand the argv tests use.
    #[cfg(test)]
    fn interactive() -> Self {
        Self {
            guidance: true,
            unattended: false,
        }
    }
}

pub(crate) struct CreateAgentSpec {
    pub worktree: WorktreeId,
    pub name: String,
    pub kind: AgentKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub auto_title: bool,
    pub cloud_prompt: Option<String>,
}

/// A pre-spawned agent CLI waiting to be adopted by the next CreateAgent for
/// the same (worktree, kind). The PTY lives in the normal sessions map under
/// a pre-generated agent id, so its NEBULA_AGENT_ID env is already the id
/// the adopted row will use. Hook events that arrive before the row exists
/// (SessionStart carries the resume session id) are buffered here and
/// replayed at adoption.
struct PrewarmEntry {
    agent_id: AgentId,
    spawned_at: Instant,
    /// Model/effort the warm CLI booted with; a CreateAgent asking for a
    /// different spec can't adopt it (the CLI is already running the wrong
    /// model), so the entry is discarded instead.
    model: Option<String>,
    effort: Option<String>,
    buffered_hooks: Vec<(HookEvent, Option<String>)>,
}

/// Wall-clock epoch ms, matching the store's `status_changed_at` stamps.
fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct Daemon {
    sessions: Mutex<HashMap<SessionRef, Arc<PtySession>>>,
    status_machines: Mutex<HashMap<AgentId, AgentStatusMachine>>,
    pub hook_env: HookEnv,
    /// Shared with the hook HTTP server, which reads agent rows to decide
    /// auto-title injection.
    pub store: Arc<Store>,
    /// Entity/status deltas fanned out to every subscribed client.
    pub events: broadcast::Sender<ServerEvent>,
    pub shutdown: tokio_util::sync::CancellationToken,
    /// Serializes worktree create/delete with the background auto-sync so
    /// a checkout is never adopted twice while its row is mid-insert.
    worktree_ops: tokio::sync::Mutex<()>,
    /// Warm agent CLIs awaiting adoption, at most one per (worktree, kind).
    prewarmed: Mutex<HashMap<(WorktreeId, AgentKind), PrewarmEntry>>,
    /// Cached `command -v` results per CLI so a missing binary doesn't get
    /// re-probed (login shell spawn) on every prewarm request.
    cli_probes: Mutex<HashMap<AgentKind, (bool, Instant)>>,
    /// How many client connections are attached per session — a session
    /// with attachments (and its whole worktree) is "in view" and exempt
    /// from idle reaping.
    attach_counts: Mutex<HashMap<SessionRef, usize>>,
    /// When each live session was last "looked at": spawned, prewarmed,
    /// attached, or covered by the in-view sweep refresh. The idle reaper
    /// kills sessions whose stamp ages past `session_idle_timeout`.
    session_interest: Mutex<HashMap<SessionRef, Instant>>,
    /// Last hook-reported cwd per agent, recorded only for payloads that
    /// passed the foreign-session gate. An agent that walks into a checkout
    /// nebula hasn't adopted yet leaves its cwd here, so the worktree sync
    /// can finish the re-home once the row exists.
    last_cwd: Mutex<HashMap<AgentId, PathBuf>>,
    /// Agents that ran `nebula worktree` and are waiting for their turn to
    /// end: the row already sits under the target worktree while the PTY
    /// still runs in the old checkout. Drained by `complete_pending_move`
    /// on the turn-end hook (kill + respawn resumed in the target), cleared
    /// by any other spawn of the agent, and consulted by the cwd reparent so
    /// the old checkout's cwd can't drag the row back in the meantime.
    pending_moves: Mutex<HashMap<AgentId, Worktree>>,
    /// Task runs waiting for their agent's turn to end so the next iteration
    /// can be pasted in. In memory on purpose: a daemon restart abandons an
    /// in-flight loop rather than re-prompting a session whose count it has
    /// lost. Keyed by the run's agent, since that is what the turn-end hook
    /// names.
    task_loops: Mutex<HashMap<AgentId, LoopState>>,
}

impl Daemon {
    pub fn new(store: Arc<Store>, hook_env: HookEnv) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            status_machines: Mutex::new(HashMap::new()),
            hook_env,
            store,
            events,
            shutdown: tokio_util::sync::CancellationToken::new(),
            worktree_ops: tokio::sync::Mutex::new(()),
            prewarmed: Mutex::new(HashMap::new()),
            cli_probes: Mutex::new(HashMap::new()),
            attach_counts: Mutex::new(HashMap::new()),
            session_interest: Mutex::new(HashMap::new()),
            last_cwd: Mutex::new(HashMap::new()),
            pending_moves: Mutex::new(HashMap::new()),
            task_loops: Mutex::new(HashMap::new()),
        })
    }

    // ---- status machine plumbing ----

    /// Feed one hook (or synthetic) event through the agent's status machine
    /// and apply the resulting effects (persist + broadcast).
    pub fn apply_hook_event(
        &self,
        agent_id: &AgentId,
        event: HookEvent,
        session_id: Option<String>,
    ) {
        enum Outcome {
            Effects(Vec<Effect>),
            UnknownAgent(HookEvent, Option<String>),
        }
        let outcome = {
            let mut machines = self.status_machines.lock().unwrap();
            match machines.entry(agent_id.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => Outcome::Effects(
                    e.into_mut()
                        .handle(event, session_id.as_deref(), Instant::now()),
                ),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    // Lazily seed from the persisted row.
                    match self.store.get_agent(agent_id) {
                        Ok(Some(agent)) => Outcome::Effects(
                            slot.insert(AgentStatusMachine::new(agent.status, agent.session_id))
                                .handle(event, session_id.as_deref(), Instant::now()),
                        ),
                        _ => Outcome::UnknownAgent(event, session_id),
                    }
                }
            }
        };
        match outcome {
            Outcome::Effects(effects) => self.apply_status_effects(agent_id, effects),
            // Ids with no row are prewarmed sessions (buffer for replay at
            // adoption) or stale env / deleted agents (dropped, as before).
            Outcome::UnknownAgent(event, session_id) => {
                self.buffer_prewarm_hook(agent_id, event, session_id)
            }
        }
    }

    fn buffer_prewarm_hook(
        &self,
        agent_id: &AgentId,
        event: HookEvent,
        session_id: Option<String>,
    ) {
        let mut pool = self.prewarmed.lock().unwrap();
        if let Some(entry) = pool.values_mut().find(|e| &e.agent_id == agent_id) {
            if entry.buffered_hooks.len() >= PREWARM_HOOK_BUFFER_CAP {
                entry.buffered_hooks.remove(0);
            }
            entry.buffered_hooks.push((event, session_id));
        }
    }

    /// Deferred-finish recheck across all machines (runs on a timer).
    pub fn tick_status_machines(&self) {
        let now = Instant::now();
        let ticked: Vec<(AgentId, Vec<Effect>)> = {
            let mut machines = self.status_machines.lock().unwrap();
            machines
                .iter_mut()
                .map(|(id, m)| (id.clone(), m.tick(now)))
                .collect()
        };
        for (id, effects) in ticked {
            self.apply_status_effects(&id, effects);
        }
    }

    fn apply_status_effects(&self, agent_id: &AgentId, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::SetStatus(status) => {
                    let (changed_at, unseen) = match self.store.set_agent_status(agent_id, status) {
                        Ok(stamped) => stamped,
                        Err(e) => {
                            tracing::warn!(error = %e, "persist status failed");
                            (epoch_ms(), false)
                        }
                    };
                    self.broadcast(ServerEvent::StatusChanged {
                        agent: agent_id.clone(),
                        status,
                        changed_at,
                        unseen,
                    });
                }
                Effect::SaveSessionId(sid) => {
                    if let Err(e) = self.store.set_agent_session_id(agent_id, Some(&sid)) {
                        tracing::warn!(error = %e, "persist session id failed");
                    }
                }
            }
        }
    }

    pub fn broadcast(&self, ev: ServerEvent) {
        let _ = self.events.send(ev);
    }

    pub fn session(&self, sref: &SessionRef) -> Option<Arc<PtySession>> {
        self.sessions.lock().unwrap().get(sref).cloned()
    }

    pub fn is_alive(&self, sref: &SessionRef) -> bool {
        self.sessions.lock().unwrap().contains_key(sref)
    }

    /// (session, child pid) for every live PTY — the metrics reading's input.
    pub fn session_pids(&self) -> Vec<(SessionRef, u32)> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(sref, s)| s.child_pid.map(|pid| (sref.clone(), pid)))
            .collect()
    }

    pub fn remove_session(&self, sref: &SessionRef) -> Option<Arc<PtySession>> {
        self.session_interest.lock().unwrap().remove(sref);
        self.sessions.lock().unwrap().remove(sref)
    }

    pub fn kill_session(&self, sref: &SessionRef) {
        if let Some(s) = self.remove_session(sref) {
            s.kill();
        }
    }

    pub fn kill_all(&self) {
        for (_, s) in self.sessions.lock().unwrap().drain() {
            s.kill();
        }
    }

    // ---- attach tracking & idle reaping ----

    /// A client attached to `sref` (the server dedupes re-attaches per
    /// connection). While any attachment exists, the session — and its
    /// whole worktree — counts as "in view".
    pub fn note_attached(&self, sref: &SessionRef) {
        *self
            .attach_counts
            .lock()
            .unwrap()
            .entry(sref.clone())
            .or_insert(0) += 1;
        self.touch_session(sref);
    }

    /// A client detached from `sref` (or its connection dropped). Restamps
    /// the session so the idle clock starts at "stopped looking", not at
    /// spawn time.
    pub fn note_detached(&self, sref: &SessionRef) {
        let mut counts = self.attach_counts.lock().unwrap();
        if let Some(n) = counts.get_mut(sref) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(sref);
            }
        }
        drop(counts);
        self.touch_session(sref);
    }

    /// Stamp `sref` as just-looked-at for the idle reaper.
    fn touch_session(&self, sref: &SessionRef) {
        self.session_interest
            .lock()
            .unwrap()
            .insert(sref.clone(), Instant::now());
    }

    /// Kill idle sessions in worktrees no client is looking at, per
    /// `session_idle_timeout` — this bounds what prewarming and
    /// walked-away-from sessions cost. "In view" = the worktree holding any
    /// attached session; in-view sessions get their stamps refreshed
    /// instead, so the full timeout starts only when the user leaves.
    /// Spared regardless of age: pinned agents (the user's "never kill
    /// this" mark — a running schedule or background job is invisible to
    /// the status machine), agents that are running or waiting on feedback,
    /// terminals with a command running, and prewarm-pool sessions
    /// (`reap_prewarmed` owns those). A reaped session revives on the next
    /// attach or prewarm; agents resume their conversation.
    pub fn reap_idle_sessions(self: &Arc<Self>) {
        let Some(timeout) = crate::config::Config::load().session_idle_timeout() else {
            return;
        };
        let sessions: Vec<(SessionRef, Arc<PtySession>)> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let attached: std::collections::HashSet<SessionRef> =
            self.attach_counts.lock().unwrap().keys().cloned().collect();
        let viewed_worktrees: std::collections::HashSet<WorktreeId> = attached
            .iter()
            .filter_map(|sref| self.session_worktree(sref))
            .collect();
        let now = Instant::now();
        for (sref, session) in sessions {
            // No store row = prewarm-pool session (or deleted mid-sweep).
            let Some(worktree_id) = self.session_worktree(&sref) else {
                continue;
            };
            if attached.contains(&sref) || viewed_worktrees.contains(&worktree_id) {
                self.touch_session(&sref);
                continue;
            }
            let age = {
                let mut interest = self.session_interest.lock().unwrap();
                // A missing stamp (session predating the map) starts aging now.
                now.duration_since(*interest.entry(sref.clone()).or_insert(now))
            };
            if age < timeout {
                continue;
            }
            let spared = match &sref {
                SessionRef::Agent(id) => match self.store.get_agent(id).ok().flatten() {
                    // Pinned = the user marked it worth keeping (schedules,
                    // loops, long jobs the status can't see) — never reap.
                    Some(agent) => {
                        agent.pinned
                            || matches!(
                                agent.status,
                                AgentStatus::Running | AgentStatus::NeedsFeedback
                            )
                    }
                    // Row vanished mid-sweep: its delete kills the PTY anyway.
                    None => true,
                },
                SessionRef::Terminal(_) => shell_has_children(&session),
            };
            if spared {
                continue;
            }
            tracing::info!(session = ?sref, idle_secs = age.as_secs(), "reaping idle session");
            self.kill_session(&sref);
            let upsert = match &sref {
                SessionRef::Agent(id) => self.agent_entity(id).map(Entity::Agent),
                SessionRef::Terminal(id) => self.terminal_entity(id).map(Entity::Terminal),
            };
            if let Ok(entity) = upsert {
                self.broadcast(ServerEvent::EntityUpserted { entity });
            }
        }
    }

    /// The worktree a session's row lives under; None when the row is gone
    /// or never existed (prewarm pool).
    fn session_worktree(&self, sref: &SessionRef) -> Option<WorktreeId> {
        match sref {
            SessionRef::Agent(id) => self
                .store
                .get_agent(id)
                .ok()
                .flatten()
                .map(|a| a.worktree_id),
            SessionRef::Terminal(id) => self
                .store
                .get_terminal(id)
                .ok()
                .flatten()
                .map(|t| t.worktree_id),
        }
    }

    // ---- snapshot ----

    pub fn snapshot(&self) -> Result<ServerEvent> {
        let (projects, worktrees, mut agents, mut terminals) = self.store.load_tree()?;
        {
            let sessions = self.sessions.lock().unwrap();
            for a in &mut agents {
                a.alive = sessions.contains_key(&SessionRef::Agent(a.id.clone()));
            }
            for t in &mut terminals {
                t.alive = sessions.contains_key(&SessionRef::Terminal(t.id.clone()));
            }
        }
        Ok(ServerEvent::Snapshot {
            workspaces: self.store.load_workspaces()?,
            active_workspace: self.store.active_workspace_id()?,
            projects,
            worktrees,
            agents,
            terminals,
            links: self.store.load_links()?,
            tasks: self.store.load_tasks()?,
            pr_seen: self.store.load_pr_seen()?,
            ui_state: self.store.load_ui_state()?,
        })
    }

    fn agent_entity(&self, id: &AgentId) -> Result<Agent> {
        let mut agent = self.store.get_agent(id)?.context("agent not found")?;
        agent.alive = self.is_alive(&SessionRef::Agent(id.clone()));
        Ok(agent)
    }

    fn terminal_entity(&self, id: &TerminalId) -> Result<TerminalTab> {
        let mut term = self.store.get_terminal(id)?.context("terminal not found")?;
        term.alive = self.is_alive(&SessionRef::Terminal(id.clone()));
        Ok(term)
    }

    // ---- workspaces ----

    /// Validated, trimmed workspace name, checked for collisions (excluding
    /// `except` on renames).
    fn checked_workspace_name(&self, name: &str, except: Option<&WorkspaceId>) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            bail!("workspace name is empty");
        }
        if let Some(existing) = self.store.workspace_by_name(name)? {
            if Some(&existing) != except {
                bail!("a workspace named '{name}' already exists");
            }
        }
        Ok(name.to_string())
    }

    /// Create a workspace. Does not open it — `workspace open` stays a
    /// separate, explicit step.
    pub fn add_workspace(self: &Arc<Self>, name: &str) -> Result<EntityId> {
        let name = self.checked_workspace_name(name, None)?;
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name,
        };
        self.store.insert_workspace(&workspace)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Workspace(workspace.clone()),
        });
        Ok(EntityId::Workspace(workspace.id))
    }

    pub fn rename_workspace(self: &Arc<Self>, id: &WorkspaceId, name: &str) -> Result<()> {
        let mut workspace = self
            .store
            .get_workspace(id)?
            .context("workspace not found")?;
        workspace.name = self.checked_workspace_name(name, Some(id))?;
        self.store.rename_workspace(id, &workspace.name)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Workspace(workspace),
        });
        Ok(())
    }

    /// Delete a workspace. Only empty ones go — its projects are the user's
    /// to move or remove first — and never the last one. Deleting the
    /// remembered default moves that default to a survivor; clients still
    /// scoped to it re-scope themselves off the EntityRemoved.
    pub fn remove_workspace(self: &Arc<Self>, id: &WorkspaceId) -> Result<()> {
        self.store
            .get_workspace(id)?
            .context("workspace not found")?;
        let projects = self.store.count_workspace_projects(id)?;
        if projects > 0 {
            bail!(
                "workspace still has {projects} project{} — remove them first",
                if projects == 1 { "" } else { "s" }
            );
        }
        if self.store.count_workspaces()? <= 1 {
            bail!("cannot delete the last workspace");
        }
        if self.store.active_workspace_id()? == *id {
            let fallback = self
                .store
                .load_workspaces()?
                .into_iter()
                .find(|w| &w.id != id)
                .context("no workspace left to open")?;
            self.store.set_active_workspace(&fallback.id)?;
        }
        self.store.delete_workspace(id)?;
        self.broadcast(ServerEvent::EntityRemoved {
            id: EntityId::Workspace(id.clone()),
        });
        Ok(())
    }

    /// Remember `id` as the workspace a fresh client opens into. Which
    /// workspace a *live* client is looking at is that client's own state —
    /// see `ClientRequest::OpenWorkspace` — so this deliberately notifies
    /// nobody: one instance switching must leave the others where they are.
    pub fn set_default_workspace(self: &Arc<Self>, id: &WorkspaceId) -> Result<()> {
        self.store
            .get_workspace(id)?
            .context("workspace not found")?;
        if self.store.active_workspace_id()? == *id {
            return Ok(()); // already the default
        }
        self.store.set_active_workspace(id)?;
        Ok(())
    }

    // ---- projects ----

    /// Register a repo as a project. `workspace` is the caller's own scope
    /// (a TUI that switched with OpenWorkspace); `None` — a one-shot
    /// `nebula add`, or a client still on whatever it booted into — means
    /// the remembered default. A scope naming a workspace that has since
    /// been deleted falls back the same way rather than failing the add.
    pub async fn add_project(
        self: &Arc<Self>,
        path: &Path,
        name: Option<String>,
        create_missing: bool,
        workspace: Option<WorkspaceId>,
    ) -> Result<EntityId> {
        if create_missing && !path.exists() {
            tokio::fs::create_dir_all(path)
                .await
                .with_context(|| format!("create {}", path.display()))?;
            if crate::config::Config::load().git_init_on_create {
                git::init(path).await?;
            }
        }
        // "not a git repository" is the right explanation only when git ran and
        // said no — if git itself is missing, that message blames the wrong
        // thing, so let git.rs's own diagnosis through untouched.
        let toplevel = git::repo_toplevel(path).await.map_err(|e| {
            if git::is_missing(&e) {
                e
            } else {
                e.context(format!("{} is not a git repository", path.display()))
            }
        })?;
        // `--show-toplevel` answers with the checkout it was run in, so inside a
        // linked worktree it names the worktree rather than the repo. A project
        // is the repo: root it at the main checkout, which `git worktree list`
        // always puts first. Adding from inside a worktree used to name the
        // project after that worktree and leave its ⌂ root row pointing at a
        // directory the project did not own.
        let entries = git::list_worktrees(&toplevel)
            .await
            .with_context(|| format!("list checkouts of {}", toplevel.display()))?;
        let repo_path = match entries.first() {
            Some(main) => main.path.clone(),
            // git listing no checkout at all for a path it just called a work
            // tree would leave the root unknowable; refuse rather than seed a
            // project with no rows, which is how a project loses its root row.
            None => bail!("git listed no checkout for {}", toplevel.display()),
        };
        // New projects land in the caller's own workspace; the same repo
        // may be added to any number of workspaces, just not twice to one.
        let workspace_id = match workspace {
            Some(id) if self.store.get_workspace(&id)?.is_some() => id,
            _ => self.store.active_workspace_id()?,
        };
        if self
            .store
            .project_in_workspace(&repo_path, &workspace_id)?
            .is_some()
        {
            bail!(
                "project already added to this workspace: {}",
                repo_path.display()
            );
        }
        let name = name.unwrap_or_else(|| {
            repo_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".into())
        });
        let project = Project {
            id: ProjectId::generate(),
            name,
            workspace_id,
            repo_path: repo_path.clone(),
            sort_order: self.store.next_project_sort_order()?,
        };
        self.store.insert_project(&project)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Project(project.clone()),
        });

        // Main checkout is modeled as a worktree row; adopt pre-existing
        // worktrees too so `nebula` matches reality on day one. Root-ness is
        // the path test the reconcile uses, not insert order — the two agreeing
        // is what keeps `repo_path` and the ⌂ root row the same directory.
        for entry in entries {
            let worktree = Worktree {
                id: WorktreeId::generate(),
                project_id: project.id.clone(),
                is_main: entry.path == repo_path,
                path: entry.path.clone(),
                branch: entry.branch,
                pinned: false,
                sort_order: 0,
            };
            self.store.insert_worktree(&worktree)?;
            self.broadcast(ServerEvent::EntityUpserted {
                entity: Entity::Worktree(worktree),
            });
        }
        Ok(EntityId::Project(project.id))
    }

    pub fn remove_project(self: &Arc<Self>, id: &ProjectId) -> Result<()> {
        // Kill any live sessions under this project first.
        let (_, worktrees, agents, terminals) = self.store.load_tree()?;
        let wt_ids: Vec<WorktreeId> = worktrees
            .into_iter()
            .filter(|w| &w.project_id == id)
            .map(|w| w.id)
            .collect();
        for a in agents.iter().filter(|a| wt_ids.contains(&a.worktree_id)) {
            self.kill_session(&SessionRef::Agent(a.id.clone()));
        }
        for t in terminals.iter().filter(|t| wt_ids.contains(&t.worktree_id)) {
            self.kill_session(&SessionRef::Terminal(t.id.clone()));
        }
        self.kill_prewarmed_in(&wt_ids);
        // Removing a project only forgets it in nebula — never touches disk.
        self.store.delete_project(id)?;
        self.broadcast(ServerEvent::EntityRemoved {
            id: EntityId::Project(id.clone()),
        });
        Ok(())
    }

    /// Move a project `delta` rows in the displayed list (clamped at the
    /// edges). Sort orders are rewritten to the display index for every
    /// project, which also normalizes legacy all-zero orders on first use.
    pub fn move_project(self: &Arc<Self>, id: &ProjectId, delta: i64) -> Result<()> {
        let (all_projects, _, _, _) = self.store.load_tree()?;
        // Reorders happen within the project's workspace — the list clients
        // actually see. Other workspaces' rows keep their sort orders; the
        // rewrite below only renumbers this workspace's slice, which stays
        // correctly interleaved because clients filter before ordering.
        let workspace = all_projects
            .iter()
            .find(|p| &p.id == id)
            .map(|p| p.workspace_id.clone());
        let mut projects: Vec<Project> = all_projects
            .into_iter()
            .filter(|p| Some(&p.workspace_id) == workspace.as_ref())
            .collect();
        let Some(pos) = projects.iter().position(|p| &p.id == id) else {
            bail!("project not found");
        };
        let target = (pos as i64 + delta).clamp(0, projects.len() as i64 - 1) as usize;
        if target == pos {
            return Ok(());
        }
        let moved = projects.remove(pos);
        projects.insert(target, moved);
        for (index, project) in projects.iter_mut().enumerate() {
            let sort_order = index as i64;
            if project.sort_order == sort_order {
                continue;
            }
            project.sort_order = sort_order;
            self.store.set_project_position(project)?;
            self.broadcast(ServerEvent::EntityUpserted {
                entity: Entity::Project(project.clone()),
            });
        }
        Ok(())
    }

    // ---- worktrees ----

    pub async fn create_worktree(
        self: &Arc<Self>,
        project_id: &ProjectId,
        branch: &str,
        base: Option<&str>,
    ) -> Result<EntityId> {
        if branch.trim().is_empty() {
            bail!("branch name is empty");
        }
        let _ops = self.worktree_ops.lock().await;
        let project = self
            .store
            .get_project(project_id)?
            .context("project not found")?;
        let path = git::add_worktree(&project.repo_path, branch, base).await?;
        let worktree = Worktree {
            id: WorktreeId::generate(),
            project_id: project_id.clone(),
            path,
            branch: branch.to_string(),
            is_main: false,
            pinned: false,
            sort_order: 0,
        };
        self.store.insert_worktree(&worktree)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Worktree(worktree.clone()),
        });
        Ok(EntityId::Worktree(worktree.id))
    }

    pub async fn delete_worktree(self: &Arc<Self>, id: &WorktreeId, force: bool) -> Result<()> {
        let _ops = self.worktree_ops.lock().await;
        let worktree = self.store.get_worktree(id)?.context("worktree not found")?;
        if worktree.is_main {
            bail!("cannot delete the main checkout — remove the project instead");
        }
        let project = self
            .store
            .get_project(&worktree.project_id)?
            .context("project not found")?;

        // Kill sessions living in this worktree.
        let (_, _, agents, terminals) = self.store.load_tree()?;
        for a in agents.iter().filter(|a| &a.worktree_id == id) {
            self.kill_session(&SessionRef::Agent(a.id.clone()));
        }
        for t in terminals.iter().filter(|t| &t.worktree_id == id) {
            self.kill_session(&SessionRef::Terminal(t.id.clone()));
        }
        self.kill_prewarmed_in(std::slice::from_ref(id));

        git::remove_worktree(&project.repo_path, &worktree.path, force).await?;
        self.store.delete_worktree(id)?;
        self.broadcast(ServerEvent::EntityRemoved {
            id: EntityId::Worktree(id.clone()),
        });
        Ok(())
    }

    pub fn set_worktree_pinned(self: &Arc<Self>, id: &WorktreeId, pinned: bool) -> Result<()> {
        self.store.set_worktree_pinned(id, pinned)?;
        let worktree = self.store.get_worktree(id)?.context("worktree not found")?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Worktree(worktree),
        });
        Ok(())
    }

    /// Reconcile a project's worktree rows with `git worktree list` so
    /// checkouts made outside nebula (an agent running `git worktree add`,
    /// manual CLI use) appear without a restart. Adopts unknown checkouts;
    /// refreshes the branch on known rows after an in-place checkout;
    /// drops rows whose checkout vanished — except the main row and rows
    /// that still have sessions, which the user must delete deliberately.
    pub async fn sync_project_worktrees(self: &Arc<Self>, project: &Project) -> Result<()> {
        let adopted = {
            let _ops = self.worktree_ops.lock().await;
            self.reconcile_project_worktrees(project).await?
        };
        // Outside the ops lock: the replay only touches agent rows, and a
        // just-adopted checkout is exactly where a session that ran
        // `git worktree add` itself already lives.
        if adopted {
            self.reparent_agents_by_last_cwd(project);
        }
        Ok(())
    }

    /// The reconcile half of `sync_project_worktrees`. Returns whether any
    /// checkout was newly adopted.
    async fn reconcile_project_worktrees(self: &Arc<Self>, project: &Project) -> Result<bool> {
        let mut adopted = false;
        let entries = git::list_worktrees(&project.repo_path).await?;
        // git lists the main checkout first, and that — not the order rows
        // happened to be inserted in — is what makes a row the ⌂ root row.
        // Deriving it here every pass repairs a project whose rows were seeded
        // before the root was known, and keeps root-ness following the repo
        // when the checkouts underneath it change.
        let main_path = entries.first().map(|e| e.path.clone());
        let is_root = |path: &Path| main_path.as_deref() == Some(path);
        let (_, worktrees, agents, terminals) = self.store.load_tree()?;
        let ours: Vec<&Worktree> = worktrees
            .iter()
            .filter(|w| w.project_id == project.id)
            .collect();
        for entry in &entries {
            if let Some(known) = ours.iter().find(|w| w.path == entry.path) {
                // Branch switched in place (checkout on the root or inside a
                // linked worktree): refresh the stored name so the row tracks
                // reality instead of the branch at adoption time.
                let root = is_root(&entry.path);
                if known.branch != entry.branch || known.is_main != root {
                    if known.branch != entry.branch {
                        self.store
                            .update_worktree_branch(&known.id, &entry.branch)?;
                    }
                    if known.is_main != root {
                        self.store.set_worktree_main(&known.id, root)?;
                    }
                    let mut updated = (*known).clone();
                    updated.branch = entry.branch.clone();
                    updated.is_main = root;
                    self.broadcast(ServerEvent::EntityUpserted {
                        entity: Entity::Worktree(updated),
                    });
                }
                continue;
            }
            let worktree = Worktree {
                id: WorktreeId::generate(),
                project_id: project.id.clone(),
                is_main: is_root(&entry.path),
                path: entry.path.clone(),
                branch: entry.branch.clone(),
                pinned: false,
                sort_order: 0,
            };
            self.store.insert_worktree(&worktree)?;
            adopted = true;
            self.broadcast(ServerEvent::EntityUpserted {
                entity: Entity::Worktree(worktree),
            });
        }
        for w in ours {
            // The main checkout is always somewhere in git's list, so a row
            // that isn't there is a linked checkout that went away — including
            // one still carrying an `is_main` from before root-ness was
            // derived, which no longer earns the row a reprieve.
            if entries.iter().any(|e| e.path == w.path) {
                continue;
            }
            let occupied = agents.iter().any(|a| a.worktree_id == w.id)
                || terminals.iter().any(|t| t.worktree_id == w.id);
            if occupied {
                continue;
            }
            self.store.delete_worktree(&w.id)?;
            self.broadcast(ServerEvent::EntityRemoved {
                id: EntityId::Worktree(w.id.clone()),
            });
        }
        Ok(adopted)
    }

    // ---- agents ----

    pub(crate) async fn create_agent(self: &Arc<Self>, spec: CreateAgentSpec) -> Result<EntityId> {
        let CreateAgentSpec {
            worktree: worktree_id,
            name,
            kind,
            model,
            effort,
            auto_title,
            cloud_prompt,
        } = spec;
        let cloud_prompt = match cloud_prompt {
            Some(_) if kind != AgentKind::Claude => {
                bail!("cloud launch is only supported for Claude")
            }
            Some(prompt) => {
                let prompt = prompt.trim().to_string();
                if prompt.is_empty() {
                    bail!("Claude Cloud needs a task");
                }
                if prompt.contains('\0') {
                    bail!("Claude Cloud task cannot contain NUL bytes");
                }
                if prompt.len() > MAX_CLOUD_PROMPT_BYTES {
                    bail!(
                        "Claude Cloud task is too long (max {} KiB)",
                        MAX_CLOUD_PROMPT_BYTES / 1024
                    );
                }
                Some(prompt)
            }
            None => None,
        };
        let worktree = self
            .store
            .get_worktree(&worktree_id)?
            .context("worktree not found")?;
        // A warm session for this (worktree, kind) hands over its PTY and
        // its pre-generated id — the CLI booted while the user typed the
        // name, so the create feels instant.
        let adopted = cloud_prompt
            .is_none()
            .then(|| self.take_prewarmed(&worktree_id, kind, &model, &effort))
            .flatten();
        // Only the cold path needs asking: an adopted warm session is proof
        // the CLI runs. Without this, a missing CLI still "succeeds" — the
        // login shell prints `command not found` into a PTY that dies at
        // once, leaving a dead row that looks identical to a fresh one.
        if adopted.is_none() && !self.cli_available_for_create(kind).await {
            bail!("{}", cli_missing_message(kind));
        }
        let agent = Agent {
            id: adopted
                .as_ref()
                .map(|e| e.agent_id.clone())
                .unwrap_or_else(AgentId::generate),
            worktree_id,
            name: if name.trim().is_empty() {
                "agent".into()
            } else {
                name.trim().to_string()
            },
            status: AgentStatus::Fresh,
            archived: false,
            archived_at: 0,
            pinned: false,
            unseen: false,
            kind,
            model,
            effort,
            session_id: None,
            cloud_session_id: None,
            sort_order: 0,
            status_changed_at: epoch_ms(),
            alive: false,
        };
        self.store
            .insert_agent_with_auto_title(&agent, auto_title)?;
        if adopted.is_none() {
            // Cold path: boot the CLI right away.
            let spawned = self.spawn_agent_session_with(
                &agent,
                &worktree,
                80,
                24,
                SpawnOpts {
                    cloud: cloud_prompt.as_deref().map(CloudLaunch::Create),
                    ..SpawnOpts::default()
                },
            );
            self.rollback_agent_on_spawn_error(&agent.id, spawned)?;
        }
        let mut broadcast_agent = agent.clone();
        broadcast_agent.alive = true;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(broadcast_agent),
        });
        if let Some(entry) = adopted {
            // Now that the row exists, replay the hooks the warm CLI fired
            // before adoption (SessionStart stores the resume session id).
            for (event, sid) in entry.buffered_hooks {
                self.apply_hook_event(&agent.id, event, sid);
            }
        }
        Ok(EntityId::Agent(agent.id))
    }

    fn rollback_agent_on_spawn_error<T>(&self, id: &AgentId, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(spawn_error) => {
                if let Err(rollback_error) = self.store.delete_agent(id) {
                    return Err(spawn_error.context(format!(
                        "agent spawn failed and its database rollback also failed: {rollback_error:#}"
                    )));
                }
                Err(spawn_error)
            }
        }
    }

    // ---- prewarm pool ----

    /// Pre-spawn an agent CLI for (worktree, kind) so the next create adopts
    /// an already-booted session. Fail-soft by design: a disabled config,
    /// missing CLI, or spawn error just means the create stays cold.
    pub async fn prewarm_agent(
        self: &Arc<Self>,
        worktree_id: &WorktreeId,
        kind: AgentKind,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<()> {
        if !crate::config::Config::load().prewarm_agents {
            return Ok(());
        }
        let Some(worktree) = self.store.get_worktree(worktree_id)? else {
            return Ok(());
        };
        let stale = {
            // One warm slot per key; keep a live, young one with the same
            // spec, replace a dead, wrong-spec, or aging one (recycling
            // before the reaper hits keeps a re-requested slot gap-free).
            let mut pool = self.prewarmed.lock().unwrap();
            if let Some(entry) = pool.get(&(worktree_id.clone(), kind)) {
                if self.is_alive(&SessionRef::Agent(entry.agent_id.clone()))
                    && entry.model == model
                    && entry.effort == effort
                    && entry.spawned_at.elapsed() < PREWARM_RECYCLE_AGE
                {
                    return Ok(());
                }
                pool.remove(&(worktree_id.clone(), kind))
            } else {
                None
            }
        };
        if let Some(old) = stale {
            self.kill_session(&SessionRef::Agent(old.agent_id));
        }
        if !self.cli_available(kind).await {
            tracing::debug!(kind = kind.as_str(), "prewarm skipped: CLI not installed");
            return Ok(());
        }
        let agent = Agent {
            id: AgentId::generate(),
            worktree_id: worktree_id.clone(),
            name: "prewarm".into(),
            status: AgentStatus::Fresh,
            archived: false,
            archived_at: 0,
            pinned: false,
            unseen: false,
            kind,
            model: model.clone(),
            effort: effort.clone(),
            session_id: None,
            cloud_session_id: None,
            sort_order: 0,
            status_changed_at: 0,
            alive: false,
        };
        self.spawn_agent_session(&agent, &worktree, 80, 24)?;
        tracing::info!(agent = %agent.id, kind = kind.as_str(), worktree = %worktree.branch, "prewarmed agent session");
        let replaced = self.prewarmed.lock().unwrap().insert(
            (worktree_id.clone(), kind),
            PrewarmEntry {
                agent_id: agent.id,
                spawned_at: Instant::now(),
                model,
                effort,
                buffered_hooks: Vec::new(),
            },
        );
        // Two racing prewarms for the same key: the loser's session would
        // otherwise leak as an orphan CLI process.
        if let Some(old) = replaced {
            self.kill_session(&SessionRef::Agent(old.agent_id));
        }
        Ok(())
    }

    /// Pop the warm entry for (worktree, kind) if its PTY is still running
    /// and it booted with the requested model/effort. A dead entry (CLI
    /// missing/crashed while warm) is dropped, a wrong-spec one is killed;
    /// either way the caller falls back to a cold spawn.
    fn take_prewarmed(
        &self,
        worktree_id: &WorktreeId,
        kind: AgentKind,
        model: &Option<String>,
        effort: &Option<String>,
    ) -> Option<PrewarmEntry> {
        let entry = self
            .prewarmed
            .lock()
            .unwrap()
            .remove(&(worktree_id.clone(), kind))?;
        if !self.is_alive(&SessionRef::Agent(entry.agent_id.clone())) {
            return None;
        }
        if entry.model != *model || entry.effort != *effort {
            self.kill_session(&SessionRef::Agent(entry.agent_id));
            return None;
        }
        Some(entry)
    }

    /// Drop warm sessions that died or sat unclaimed past the max age
    /// (runs on the daemon's periodic tick).
    pub fn reap_prewarmed(&self) {
        let doomed: Vec<AgentId> = {
            let mut pool = self.prewarmed.lock().unwrap();
            let expired: Vec<_> = pool
                .iter()
                .filter(|(_, e)| {
                    e.spawned_at.elapsed() > PREWARM_MAX_AGE
                        || !self.is_alive(&SessionRef::Agent(e.agent_id.clone()))
                })
                .map(|(k, _)| k.clone())
                .collect();
            expired
                .into_iter()
                .filter_map(|k| pool.remove(&k))
                .map(|e| e.agent_id)
                .collect()
        };
        for id in doomed {
            tracing::debug!(agent = %id, "reaping prewarmed session");
            self.kill_session(&SessionRef::Agent(id));
        }
    }

    /// Kill warm sessions homed in any of these worktrees (worktree delete,
    /// project remove — their store rows are gone or going).
    fn kill_prewarmed_in(&self, worktree_ids: &[WorktreeId]) {
        let doomed: Vec<AgentId> = {
            let mut pool = self.prewarmed.lock().unwrap();
            let keys: Vec<_> = pool
                .keys()
                .filter(|(w, _)| worktree_ids.contains(w))
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| pool.remove(&k))
                .map(|e| e.agent_id)
                .collect()
        };
        for id in doomed {
            self.kill_session(&SessionRef::Agent(id));
        }
    }

    /// Is the kind's CLI on the user's PATH (as their login shell sees it)?
    /// Cached: hits for an hour, misses for a minute so a just-installed CLI
    /// gets picked up quickly. Probe trouble (timeout, spawn error) fails
    /// open — a doomed warm spawn is still graceful.
    async fn cli_available(&self, kind: AgentKind) -> bool {
        if std::env::var("NEBULA_AGENT_CMD").is_ok() {
            return true; // test override is spawned verbatim
        }
        const OK_TTL: Duration = Duration::from_secs(3600);
        const FAIL_TTL: Duration = Duration::from_secs(60);
        {
            let probes = self.cli_probes.lock().unwrap();
            if let Some((ok, at)) = probes.get(&kind) {
                if at.elapsed() < if *ok { OK_TTL } else { FAIL_TTL } {
                    return *ok;
                }
            }
        }
        self.probe_cli(kind).await
    }

    /// Fill the availability cache for every kind at boot, off the request
    /// loop. Without it the first CreateAgent of a session pays a full
    /// login-shell probe (~1s with a heavy ~/.zshrc) before it can answer.
    pub async fn warm_cli_probes(self: &Arc<Self>) {
        for kind in AgentKind::ALL {
            self.cli_available(kind).await;
        }
    }

    /// Same question, asked on behalf of a create the user just triggered.
    /// A cached *hit* is trusted; a cached *miss* is re-probed, so someone who
    /// installs the CLI and immediately retries isn't told for another minute
    /// that it's missing. Misses are rare, so this costs nothing in practice.
    async fn cli_available_for_create(&self, kind: AgentKind) -> bool {
        self.cli_available(kind).await || self.probe_cli(kind).await
    }

    /// Uncached `command -v` through the user's login shell; caches the answer.
    async fn probe_cli(&self, kind: AgentKind) -> bool {
        let check = format!("command -v '{}' >/dev/null 2>&1", kind.cli_program());
        let mut probe = tokio::process::Command::new(user_shell());
        probe
            .args(["-l", "-i", "-c", &check])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // A timed-out probe must die with the dropped future, not linger.
            .kill_on_drop(true);
        // Own session: the interactive shell must not reach the daemon's
        // controlling terminal (--foreground runs have one). zsh's job-control
        // init opens /dev/tty and makes itself the foreground process group,
        // SIGTTIN-stopping whatever TUI owns that terminal.
        unsafe {
            probe.pre_exec(|| match nix::unistd::setsid() {
                Ok(_) => Ok(()),
                Err(errno) => Err(std::io::Error::from_raw_os_error(errno as i32)),
            });
        }
        let status = tokio::time::timeout(Duration::from_secs(5), probe.status()).await;
        match status {
            Ok(Ok(status)) => {
                let ok = status.success();
                self.cli_probes
                    .lock()
                    .unwrap()
                    .insert(kind, (ok, Instant::now()));
                ok
            }
            _ => true,
        }
    }

    pub fn rename_agent(self: &Arc<Self>, id: &AgentId, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            bail!("name is empty");
        }
        self.store.rename_agent(id, name.trim())?;
        let agent = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(agent),
        });
        Ok(())
    }

    /// Agent-initiated one-shot title (`nebula rename` inside the session's
    /// CLI). Applies only while the auto-title is still pending; afterwards
    /// it reports the standing title as an error so the CLI (and the model
    /// reading its output) knows nothing changed.
    pub fn auto_rename_agent(self: &Arc<Self>, id: &AgentId, name: &str) -> Result<()> {
        let title = sanitize_title(name);
        if title.is_empty() {
            bail!("title is empty");
        }
        let agent = self.store.get_agent(id)?.context("agent not found")?;
        if !self.store.rename_agent_if_auto_pending(id, &title)? {
            bail!(
                "session already has a title ({:?}); leaving it unchanged — a user-set \
                 title is only replaced with `nebula rename --force`",
                agent.name
            );
        }
        let agent = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(agent),
        });
        Ok(())
    }

    /// Re-home an agent row under another worktree of the same project. A
    /// live PTY still runs — and its hooks still report a cwd — inside the
    /// old checkout, so left alone `reparent_agent_by_cwd` would snap the
    /// row straight back on the next hook event: kill it and respawn resumed
    /// in the target so the process and the row agree. A respawn failure
    /// degrades to a dead session the next attach/prewarm revives via
    /// `ensure_session`.
    pub fn move_agent(self: &Arc<Self>, id: &AgentId, worktree_id: &WorktreeId) -> Result<()> {
        let agent = self.store.get_agent(id)?.context("agent not found")?;
        if &agent.worktree_id == worktree_id {
            return Ok(());
        }
        let target = self.sibling_worktree(&agent, worktree_id)?;
        self.pending_moves.lock().unwrap().remove(id);
        let sref = SessionRef::Agent(id.clone());
        let was_alive = self.session(&sref).is_some();
        if was_alive {
            self.kill_session(&sref);
        }
        // A deliberate move invalidates the remembered hook cwd: it still
        // points at the old checkout, and the next worktree sync would
        // replay it straight back over the user's choice.
        self.last_cwd.lock().unwrap().remove(id);
        self.store.set_agent_worktree(id, worktree_id)?;
        if was_alive {
            if let Err(e) = self.spawn_agent_session(&agent, &target, 80, 24) {
                tracing::warn!(agent = %id, error = %e, "respawn after move failed");
            }
        }
        let agent = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(agent),
        });
        Ok(())
    }

    /// The worktree `worktree_id`, checked to belong to the same project as
    /// `agent`'s current one — the only kind of move a row can make.
    fn sibling_worktree(&self, agent: &Agent, worktree_id: &WorktreeId) -> Result<Worktree> {
        let target = self
            .store
            .get_worktree(worktree_id)?
            .context("worktree not found")?;
        let current = self
            .store
            .get_worktree(&agent.worktree_id)?
            .context("worktree not found")?;
        if target.project_id != current.project_id {
            bail!("target worktree belongs to a different project");
        }
        Ok(target)
    }

    /// `nebula worktree <branch>`, run by the agent inside its own session.
    /// The row moves under `branch`'s worktree of the same project now —
    /// created when the project has no checkout for that branch yet — and
    /// a live PTY follows once its turn ends (`complete_pending_move`),
    /// because the CLI running this command *is* that PTY's foreground
    /// tool call: killing it here would cut the turn off mid-answer.
    pub async fn enter_worktree(
        self: &Arc<Self>,
        id: &AgentId,
        branch: &str,
        base: Option<&str>,
    ) -> Result<(Worktree, EnterOutcome)> {
        let branch = branch.trim();
        if branch.is_empty() {
            bail!("branch name is empty");
        }
        let agent = self.store.get_agent(id)?.context("agent not found")?;
        if agent.archived {
            bail!("agent is archived");
        }
        let current = self
            .store
            .get_worktree(&agent.worktree_id)?
            .context("worktree not found")?;
        let (_, worktrees, _, _) = self.store.load_tree()?;
        let existing = worktrees
            .into_iter()
            .find(|w| w.project_id == current.project_id && w.branch == branch);
        let target = match existing {
            Some(w) => w,
            None => {
                let created = self
                    .create_worktree(&current.project_id, branch, base)
                    .await?;
                let EntityId::Worktree(new_id) = created else {
                    bail!("worktree creation returned a non-worktree entity");
                };
                self.store
                    .get_worktree(&new_id)?
                    .context("worktree not found")?
            }
        };
        if target.id == current.id {
            return Ok((target, EnterOutcome::AlreadyThere));
        }
        let alive = self.session(&SessionRef::Agent(id.clone())).is_some();
        // Same invalidation as `move_agent`: every cwd this process reports
        // until it respawns is the old checkout's.
        self.last_cwd.lock().unwrap().remove(id);
        if alive {
            self.pending_moves
                .lock()
                .unwrap()
                .insert(id.clone(), target.clone());
        }
        self.store.set_agent_worktree(id, &target.id)?;
        let entity = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(entity),
        });
        let outcome = if alive {
            EnterOutcome::Relocating
        } else {
            EnterOutcome::NextLaunch
        };
        Ok((target, outcome))
    }

    /// The turn an agent ran `nebula worktree` in has ended: make the
    /// process match its row. Kill it and respawn it resumed in the target,
    /// with a prompt naming the checkout it now runs in so the conversation
    /// carries straight on (Claude takes that prompt as an argument; codex
    /// and cursor resume silent and wait for the user). Gated on the
    /// turn-end hooks — Stop, and the idle notification a Stop-less end
    /// still fires — so a Bash hook from the same turn never triggers it.
    pub fn complete_pending_move(self: &Arc<Self>, id: &AgentId, event: &HookEvent) {
        let turn_over = match event {
            HookEvent::Stop => true,
            HookEvent::Notification { notification_type } => {
                notification_type.as_deref() == Some("idle_prompt")
            }
            _ => false,
        };
        if !turn_over {
            return;
        }
        let Some(target) = self.pending_moves.lock().unwrap().remove(id) else {
            return;
        };
        let agent = match self.store.get_agent(id) {
            Ok(Some(agent)) if !agent.archived && agent.worktree_id == target.id => agent,
            // Archived, deleted, or moved elsewhere by hand since: the
            // row's current home wins, nothing to relocate into.
            _ => return,
        };
        let sref = SessionRef::Agent(id.clone());
        if self.session(&sref).is_none() {
            // Died since (or the user closed it): the next launch boots in
            // the target on its own, only without the relocation notice.
            return;
        }
        tracing::info!(agent = %id, to = %target.branch, "relocating session into its worktree");
        self.kill_session(&sref);
        self.last_cwd.lock().unwrap().remove(id);
        let prompt = relocation_prompt(&target);
        if let Err(e) = self.spawn_agent_session_with(
            &agent,
            &target,
            80,
            24,
            SpawnOpts {
                initial_prompt: Some(&prompt),
                ..SpawnOpts::default()
            },
        ) {
            tracing::warn!(agent = %id, error = %e, "respawn after worktree relocation failed");
        }
        if let Ok(entity) = self.agent_entity(id) {
            self.broadcast(ServerEvent::EntityUpserted {
                entity: Entity::Agent(entity),
            });
        }
    }

    /// Whether `id` is between `enter_worktree` and its respawn.
    #[cfg(test)]
    fn relocation_pending(&self, id: &AgentId) -> bool {
        self.pending_moves.lock().unwrap().contains_key(id)
    }

    /// Row-only re-home: store update plus broadcast, never the PTY. The
    /// hook-cwd reparent uses this — there the process already runs in the
    /// target checkout and only the row is stale, so killing it would
    /// interrupt a live conversation for nothing.
    fn move_agent_row(self: &Arc<Self>, id: &AgentId, worktree_id: &WorktreeId) -> Result<()> {
        self.store.set_agent_worktree(id, worktree_id)?;
        let agent = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(agent),
        });
        Ok(())
    }

    /// A hook payload reported the agent CLI's working directory. When that
    /// directory sits inside a *different* worktree of the same project (the
    /// session entered a worktree it created mid-conversation), re-home the
    /// agent row so the tree reflects where the work actually happens.
    /// Fail-soft: any error leaves the row where it is.
    pub fn reparent_agent_by_cwd(
        self: &Arc<Self>,
        agent_id: &AgentId,
        cwd: &str,
        payload_session_id: Option<&str>,
        captures_session: bool,
    ) {
        if let Err(e) =
            self.try_reparent_agent_by_cwd(agent_id, cwd, payload_session_id, captures_session)
        {
            tracing::warn!(agent = %agent_id, error = %e, "cwd reparent failed");
        }
    }

    fn try_reparent_agent_by_cwd(
        self: &Arc<Self>,
        agent_id: &AgentId,
        cwd: &str,
        payload_session_id: Option<&str>,
        captures_session: bool,
    ) -> Result<()> {
        let Some(agent) = self.store.get_agent(agent_id)? else {
            self.last_cwd.lock().unwrap().remove(agent_id);
            return Ok(());
        };
        if agent.archived {
            self.last_cwd.lock().unwrap().remove(agent_id);
            return Ok(());
        }
        // Mid-relocation the row already sits under the target while the
        // process still reports the old checkout — ignore it until the
        // respawn lands there.
        if self.pending_moves.lock().unwrap().contains_key(agent_id) {
            return Ok(());
        }
        // Same foreign-session rule as the status machine: a payload from a
        // different CLI session only counts when the event (re)establishes
        // session ownership (UserPromptSubmit / SessionStart).
        if !captures_session {
            if let (Some(mine), Some(theirs)) = (agent.session_id.as_deref(), payload_session_id) {
                if mine != theirs {
                    return Ok(());
                }
            }
        }
        let cwd = canonical_or_raw(Path::new(cwd));
        // Remembered even when it resolves to nothing: an agent that just ran
        // `git worktree add` and stepped into the result reports a cwd nebula
        // has no row for yet, and the worktree sync replays this to finish the
        // re-home the moment that row is adopted.
        self.last_cwd
            .lock()
            .unwrap()
            .insert(agent_id.clone(), cwd.clone());
        self.reparent_agent_to_cwd(&agent, &cwd)
    }

    /// Move `agent`'s row under the worktree owning `cwd` when that is a
    /// different worktree of the same project. `cwd` must already be
    /// canonicalized.
    fn reparent_agent_to_cwd(self: &Arc<Self>, agent: &Agent, cwd: &Path) -> Result<()> {
        let Some(current) = self.store.get_worktree(&agent.worktree_id)? else {
            return Ok(());
        };
        let (_, worktrees, _, _) = self.store.load_tree()?;
        // Deepest worktree of the same project containing cwd — nested
        // layouts (checkouts under the repo root) must not resolve to the
        // root row just because the root path is also a prefix.
        let target = worktrees
            .into_iter()
            .filter(|w| w.project_id == current.project_id)
            .map(|w| {
                let canonical = canonical_or_raw(&w.path);
                (w, canonical)
            })
            .filter(|(_, canonical)| cwd.starts_with(canonical))
            .max_by_key(|(_, canonical)| canonical.components().count());
        if let Some((worktree, _)) = target {
            if worktree.id != agent.worktree_id {
                tracing::info!(
                    agent = %agent.id,
                    from = %current.branch,
                    to = %worktree.branch,
                    "agent re-homed by hook cwd"
                );
                self.move_agent_row(&agent.id, &worktree.id)?;
            }
        }
        Ok(())
    }

    /// Replay remembered hook cwds for `project`'s agents. Runs after the
    /// worktree sync adopts checkouts: a session that creates a worktree and
    /// enters it reports the new cwd (often on the very next `Stop`) before
    /// the row exists, and without this replay its row would sit under the
    /// old checkout until the user's next prompt.
    fn reparent_agents_by_last_cwd(self: &Arc<Self>, project: &Project) {
        let known: Vec<(AgentId, PathBuf)> = {
            let map = self.last_cwd.lock().unwrap();
            map.iter().map(|(id, p)| (id.clone(), p.clone())).collect()
        };
        for (agent_id, cwd) in known {
            let agent = match self.store.get_agent(&agent_id) {
                Ok(Some(agent)) => agent,
                Ok(None) => {
                    self.last_cwd.lock().unwrap().remove(&agent_id);
                    continue;
                }
                Err(e) => {
                    tracing::warn!(agent = %agent_id, error = %e, "cwd replay lookup failed");
                    continue;
                }
            };
            if agent.archived {
                continue;
            }
            let in_project = matches!(
                self.store.get_worktree(&agent.worktree_id),
                Ok(Some(w)) if w.project_id == project.id
            );
            if !in_project {
                continue;
            }
            if let Err(e) = self.reparent_agent_to_cwd(&agent, &cwd) {
                tracing::warn!(agent = %agent_id, error = %e, "cwd replay reparent failed");
            }
        }
    }

    pub fn archive_agent(self: &Arc<Self>, id: &AgentId) -> Result<()> {
        self.kill_session(&SessionRef::Agent(id.clone()));
        self.store.set_agent_archived(id, true)?;
        let agent = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(agent),
        });
        Ok(())
    }

    pub fn unarchive_agent(self: &Arc<Self>, id: &AgentId) -> Result<()> {
        self.store.set_agent_archived(id, false)?;
        let agent = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(agent),
        });
        Ok(())
    }

    pub fn set_agent_pinned(self: &Arc<Self>, id: &AgentId, pinned: bool) -> Result<()> {
        self.store.set_agent_pinned(id, pinned)?;
        let agent = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(agent),
        });
        Ok(())
    }

    /// A client put this agent's session on screen: its unseen-finish flag
    /// (`Agent::unseen`) is cleared, and every subscriber gets the row so
    /// their counts drop together. Nothing is sent when the flag was
    /// already clear — re-attaching to a session you've read is free.
    pub fn mark_agent_seen(&self, id: &AgentId) -> Result<()> {
        if self.store.mark_agent_seen(id)? {
            let agent = self.agent_entity(id)?;
            self.broadcast(ServerEvent::EntityUpserted {
                entity: Entity::Agent(agent),
            });
        }
        Ok(())
    }

    pub fn delete_agent(self: &Arc<Self>, id: &AgentId) -> Result<()> {
        self.kill_session(&SessionRef::Agent(id.clone()));
        self.last_cwd.lock().unwrap().remove(id);
        self.pending_moves.lock().unwrap().remove(id);
        self.store.delete_agent(id)?;
        self.broadcast(ServerEvent::EntityRemoved {
            id: EntityId::Agent(id.clone()),
        });
        Ok(())
    }

    pub async fn restart_agent(self: &Arc<Self>, id: &AgentId) -> Result<()> {
        let agent = self.store.get_agent(id)?.context("agent not found")?;
        if agent.archived {
            bail!("agent is archived — unarchive it first");
        }
        // A Cloud row that never became a local session has nothing to
        // resume here: a plain restart would boot a bare CLI with no link
        // to the work. Re-enter the cloud session instead. Once a teleport
        // has produced a local session id, restarts resume that.
        if agent.cloud_session_id.is_some() && agent.session_id.is_none() {
            return self.attach_cloud_agent(id).await;
        }
        let worktree = self
            .store
            .get_worktree(&agent.worktree_id)?
            .context("worktree not found")?;
        self.kill_session(&SessionRef::Agent(id.clone()));
        self.spawn_agent_session(&agent, &worktree, 80, 24)?;
        let mut broadcast_agent = agent.clone();
        broadcast_agent.alive = true;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(broadcast_agent),
        });
        Ok(())
    }

    /// Re-enter the Claude Cloud session a row launched. The live attach
    /// (`claude --cloud <id>`) is tried first; on an account without that
    /// rollout the CLI refuses and dies, and the fallback armed by the
    /// spawn teleports the session into a local one instead (same branch
    /// and transcript, minus the live link). Either CLI switches the
    /// checkout to the cloud branch — and teleport refuses a dirty tree
    /// outright — so a row still sitting in the main checkout is first
    /// re-homed into a worktree of its own; the user's checkout is never
    /// the one that gets switched.
    pub async fn attach_cloud_agent(self: &Arc<Self>, id: &AgentId) -> Result<()> {
        let agent = self.store.get_agent(id)?.context("agent not found")?;
        if agent.archived {
            bail!("agent is archived — unarchive it first");
        }
        let Some(cloud_id) = agent.cloud_session_id.clone() else {
            bail!("session was not launched in Claude Cloud");
        };
        let mut worktree = self
            .store
            .get_worktree(&agent.worktree_id)?
            .context("worktree not found")?;
        if worktree.is_main {
            let branch = cloud_worktree_branch(&cloud_id);
            let EntityId::Worktree(target) = self
                .create_worktree(&worktree.project_id, &branch, None)
                .await?
            else {
                bail!("worktree create returned a non-worktree entity");
            };
            worktree = self
                .store
                .get_worktree(&target)?
                .context("worktree not found")?;
            // Same invalidation as a deliberate move: the remembered hook
            // cwd points at the old checkout and would sync the row back.
            self.last_cwd.lock().unwrap().remove(id);
            self.store.set_agent_worktree(id, &target)?;
            tracing::info!(agent = %id, branch, "cloud row re-homed into its own worktree");
        }
        self.kill_session(&SessionRef::Agent(id.clone()));
        self.spawn_agent_session_with(
            &agent,
            &worktree,
            80,
            24,
            SpawnOpts {
                cloud: Some(CloudLaunch::Attach(&cloud_id)),
                ..SpawnOpts::default()
            },
        )?;
        let entity = self.agent_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(entity),
        });
        Ok(())
    }

    /// `claude --cloud <id>` on an account without the attach rollout
    /// prints "Attaching to an existing cloud session is not enabled for
    /// your account." and exits. The refusal is *read* off the output
    /// (`pty::cloud`), not inferred from the exit: a deliberate kill of an
    /// attach that worked looks identical by exit code and must not spawn
    /// anything. Once the refused child is gone, the same row is respawned
    /// as `claude --teleport <id>` in the same worktree.
    fn arm_cloud_attach_fallback(
        self: &Arc<Self>,
        agent: Agent,
        worktree: Worktree,
        session: Arc<PtySession>,
        cloud_id: String,
        cols: u16,
        rows: u16,
    ) {
        let daemon = self.clone();
        let mut rx = session.events.subscribe();
        tokio::spawn(async move {
            let mut rejected = false;
            loop {
                match rx.recv().await {
                    Ok(PtyEvent::CloudAttachRejected) => rejected = true,
                    Ok(PtyEvent::Exited { .. }) => break,
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            if !rejected {
                return;
            }
            // Archived, deleted, or moved inside the window: leave it be.
            match daemon.store.get_agent(&agent.id) {
                Ok(Some(current)) if !current.archived && current.worktree_id == worktree.id => {}
                _ => return,
            }
            tracing::info!(agent = %agent.id, "cloud attach refused — teleporting the session locally");
            match daemon.spawn_agent_session_with(
                &agent,
                &worktree,
                cols,
                rows,
                SpawnOpts {
                    cloud: Some(CloudLaunch::Teleport(&cloud_id)),
                    ..SpawnOpts::default()
                },
            ) {
                Ok(_) => {
                    if let Ok(entity) = daemon.agent_entity(&agent.id) {
                        daemon.broadcast(ServerEvent::EntityUpserted {
                            entity: Entity::Agent(entity),
                        });
                    }
                }
                Err(e) => tracing::warn!(agent = %agent.id, error = %e, "teleport spawn failed"),
            }
        });
    }

    // ---- terminals ----

    pub fn create_terminal(
        self: &Arc<Self>,
        worktree_id: &WorktreeId,
        name: Option<String>,
    ) -> Result<EntityId> {
        let worktree = self
            .store
            .get_worktree(worktree_id)?
            .context("worktree not found")?;
        let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
            let n = self.store.count_terminals(worktree_id).unwrap_or(0);
            format!("term-{}", n + 1)
        });
        let terminal = TerminalTab {
            id: TerminalId::generate(),
            worktree_id: worktree_id.clone(),
            name,
            sort_order: 0,
            alive: false,
        };
        self.store.insert_terminal(&terminal)?;
        self.spawn_terminal_session(&terminal, &worktree, 80, 24)?;
        let mut broadcast_term = terminal.clone();
        broadcast_term.alive = true;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Terminal(broadcast_term),
        });
        Ok(EntityId::Terminal(terminal.id))
    }

    pub fn rename_terminal(self: &Arc<Self>, id: &TerminalId, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            bail!("name is empty");
        }
        self.store.rename_terminal(id, name.trim())?;
        let term = self.terminal_entity(id)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Terminal(term),
        });
        Ok(())
    }

    pub fn close_terminal(self: &Arc<Self>, id: &TerminalId) -> Result<()> {
        self.kill_session(&SessionRef::Terminal(id.clone()));
        self.store.delete_terminal(id)?;
        self.broadcast(ServerEvent::EntityRemoved {
            id: EntityId::Terminal(id.clone()),
        });
        Ok(())
    }

    // ---- links ----

    pub fn create_link(self: &Arc<Self>, worktree_id: &WorktreeId, url: &str) -> Result<EntityId> {
        let url = normalize_url(url)?;
        self.store
            .get_worktree(worktree_id)?
            .context("worktree not found")?;
        let link = Link {
            id: LinkId::generate(),
            worktree_id: worktree_id.clone(),
            url,
            sort_order: self.store.next_link_sort_order(worktree_id)?,
        };
        self.store.insert_link(&link)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Link(link.clone()),
        });
        Ok(EntityId::Link(link.id))
    }

    pub fn update_link(self: &Arc<Self>, id: &LinkId, url: &str) -> Result<()> {
        let url = normalize_url(url)?;
        self.store.set_link_url(id, &url)?;
        let link = self.store.get_link(id)?.context("link not found")?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Link(link),
        });
        Ok(())
    }

    pub fn delete_link(self: &Arc<Self>, id: &LinkId) -> Result<()> {
        self.store.delete_link(id)?;
        self.broadcast(ServerEvent::EntityRemoved {
            id: EntityId::Link(id.clone()),
        });
        Ok(())
    }

    // ---- tasks ----

    /// Define a task. The cron is validated here rather than at the edge so
    /// every path into the store gets the same guard, and a typo comes back
    /// as an Error instead of becoming a task that silently never fires.
    pub fn create_task(self: &Arc<Self>, spec: TaskSpec) -> Result<EntityId> {
        let spec = self.vet_task_spec(spec)?;
        let now = epoch_ms();
        let mut task = Task {
            id: TaskId::generate(),
            project_id: spec.project.clone(),
            name: spec.name,
            prompt: spec.prompt,
            kind: spec.kind,
            model: spec.model,
            effort: spec.effort,
            cron: spec.cron,
            iterations: spec.iterations,
            unattended: spec.unattended,
            final_prompt: spec.final_prompt,
            stall_timeout_secs: spec.stall_timeout_secs,
            commit_on_finish: spec.commit_on_finish,
            target: spec.target,
            enabled: spec.enabled,
            last_run_at: 0,
            next_run_at: 0,
            last_outcome: None,
            last_agent_id: None,
            created_at: now,
            sort_order: self.store.next_task_sort_order(&spec.project)?,
        };
        // A new task's first window is measured from now, not from epoch 0 —
        // otherwise every cron looks like it missed decades of runs.
        task.next_run_at = self.compute_next_run(&task, now, now);
        self.store.insert_task(&task)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Task(task.clone()),
        });
        Ok(EntityId::Task(task.id))
    }

    pub fn update_task(self: &Arc<Self>, id: &TaskId, spec: TaskSpec) -> Result<()> {
        let spec = self.vet_task_spec(spec)?;
        let existing = self.store.get_task(id)?.context("task not found")?;
        let mut task = Task {
            project_id: spec.project,
            name: spec.name,
            prompt: spec.prompt,
            kind: spec.kind,
            model: spec.model,
            effort: spec.effort,
            cron: spec.cron,
            iterations: spec.iterations,
            unattended: spec.unattended,
            final_prompt: spec.final_prompt,
            stall_timeout_secs: spec.stall_timeout_secs,
            commit_on_finish: spec.commit_on_finish,
            target: spec.target,
            enabled: spec.enabled,
            ..existing
        };
        self.store.update_task(&task)?;
        // The edit may have changed the cron, the enabled flag, or both, so
        // the due stamp is always recomputed — from now, since an edited
        // schedule shouldn't inherit the old one's pending window.
        let now = epoch_ms();
        task.next_run_at = self.compute_next_run(&task, now, now);
        self.store.set_task_next_run(&task.id, task.next_run_at)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Task(task),
        });
        Ok(())
    }

    pub fn set_task_enabled(self: &Arc<Self>, id: &TaskId, enabled: bool) -> Result<()> {
        self.store.set_task_enabled(id, enabled)?;
        let mut task = self.store.get_task(id)?.context("task not found")?;
        let now = epoch_ms();
        task.next_run_at = self.compute_next_run(&task, now, now);
        self.store.set_task_next_run(id, task.next_run_at)?;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Task(task),
        });
        Ok(())
    }

    pub fn delete_task(self: &Arc<Self>, id: &TaskId) -> Result<()> {
        // Runs already in flight lose their loop: the session stays, it just
        // stops being re-prompted. Killing it would throw away a turn's work.
        self.task_loops
            .lock()
            .unwrap()
            .retain(|_, state| &state.task_id != id);
        self.store.delete_task(id)?;
        self.broadcast(ServerEvent::EntityRemoved {
            id: EntityId::Task(id.clone()),
        });
        Ok(())
    }

    /// Normalize and check everything a client can set. An empty cron string
    /// means "manual only" rather than a parse error — that is how the TUI
    /// clears a schedule.
    fn vet_task_spec(&self, mut spec: TaskSpec) -> Result<TaskSpec> {
        self.store
            .get_project(&spec.project)?
            .context("project not found")?;
        spec.name = spec.name.trim().to_string();
        if spec.name.is_empty() {
            spec.name = "task".into();
        }
        spec.prompt = spec.prompt.trim().to_string();
        if spec.prompt.is_empty() {
            bail!("a task needs a prompt to send");
        }
        if spec.prompt.len() > MAX_CLOUD_PROMPT_BYTES {
            bail!(
                "task prompt is too long (max {} KiB)",
                MAX_CLOUD_PROMPT_BYTES / 1024
            );
        }
        spec.cron = match spec.cron {
            Some(c) if c.trim().is_empty() => None,
            Some(c) => {
                let c = c.trim().to_string();
                crate::schedule::validate(&c)?;
                Some(c)
            }
            None => None,
        };
        // Same treatment as `prompt`: an all-whitespace wrap-up is no
        // wrap-up, and it must fit down the same pipe.
        spec.final_prompt = match spec.final_prompt {
            Some(p) if p.trim().is_empty() => None,
            Some(p) => {
                let p = p.trim().to_string();
                if p.len() > MAX_CLOUD_PROMPT_BYTES {
                    bail!(
                        "task wrap-up prompt is too long (max {} KiB)",
                        MAX_CLOUD_PROMPT_BYTES / 1024
                    );
                }
                Some(p)
            }
            None => None,
        };
        if spec.iterations == 0 {
            bail!("a task runs at least one iteration");
        }
        if spec.iterations > MAX_TASK_ITERATIONS {
            bail!("a task runs at most {MAX_TASK_ITERATIONS} iterations");
        }
        if let TaskTarget::Worktree(id) = &spec.target {
            self.store
                .get_worktree(id)?
                .context("the task's worktree no longer exists")?;
        }
        Ok(spec)
    }

    /// The due stamp for a task: 0 whenever it can't fire on its own
    /// (disabled, or no cron), otherwise the cron's next window measured
    /// from `since` with the missed-window rule applied.
    fn compute_next_run(&self, task: &Task, since: i64, now: i64) -> i64 {
        let Some(cron) = task.cron.as_deref() else {
            return 0;
        };
        if !task.enabled {
            return 0;
        }
        match crate::schedule::next_due_ms(cron, since, now) {
            Ok(Some(due)) => due,
            // Already validated on the way in, so a failure here means a row
            // hand-edited in the DB or an expression with no future left.
            Ok(None) => 0,
            Err(e) => {
                tracing::warn!(task = %task.id, error = %e, "task cron no longer parses");
                0
            }
        }
    }

    /// One scheduler sweep: start every task whose window has come around.
    /// Called from its own interval loop in `lib.rs`.
    pub async fn tick_scheduler(self: &Arc<Self>) {
        let now = epoch_ms();
        // Before launching anything new, retire anything that has quietly
        // died. A run only advances on a turn-end signal, so a wedged CLI is
        // invisible until something goes looking — this is that something.
        self.sweep_stalled_runs();
        let Ok(tasks) = self.store.load_tasks() else {
            return;
        };
        for task in tasks {
            // All three conditions, not just the stamp: a manual task has no
            // schedule to be due for however its stamp reads, and a disabled
            // one is off. `compute_next_run` already zeroes the stamp in both
            // cases, so this is the belt to that braces — a row hand-edited
            // in the DB must not be able to launch an agent every 30s.
            if !task.enabled
                || task.cron.is_none()
                || !crate::schedule::is_due(task.next_run_at, now)
            {
                continue;
            }
            // Re-stamp before running, not after: a run that fails must not
            // leave the task due forever, re-firing on every tick.
            let next = self.compute_next_run(&task, task.next_run_at, now);
            if let Err(e) = self.store.set_task_next_run(&task.id, next) {
                tracing::warn!(task = %task.id, error = %e, "could not stamp next run");
                continue;
            }
            // A run still in flight owns the checkout. Starting a second
            // one would put two agents in the same files — worse than a
            // missed window, so the window is what gets dropped. The stamp
            // above already moved, so this fires once and then waits.
            if self.run_in_flight(&task.id) {
                tracing::info!(task = %task.id, name = %task.name, "skipping: previous run still going");
                let _ = self.store.set_task_run_state(
                    &task.id,
                    task.last_run_at,
                    "skipped: previous run still going",
                    task.last_agent_id.as_ref(),
                );
                self.rebroadcast_task(&task.id);
                continue;
            }
            tracing::info!(task = %task.id, name = %task.name, "scheduled task is due");
            if let Err(e) = self.run_task(&task.id).await {
                tracing::warn!(task = %task.id, error = %e, "scheduled task failed to start");
            }
        }
    }

    /// Does this task already have a run going? The loop table is the only
    /// record of that — `last_outcome` says "running" for a run whose
    /// session died three hours ago.
    fn run_in_flight(&self, id: &TaskId) -> bool {
        self.task_loops
            .lock()
            .unwrap()
            .values()
            .any(|s| &s.task_id == id)
    }

    /// End every run whose turn has not ended inside its task's watchdog.
    /// The session is left alone deliberately: it is the evidence, and
    /// killing it would throw away whatever the turn did manage to do.
    fn sweep_stalled_runs(self: &Arc<Self>) {
        let now = epoch_ms();
        let in_flight: Vec<(AgentId, TaskId, u32, i64)> = self
            .task_loops
            .lock()
            .unwrap()
            .iter()
            .map(|(a, s)| {
                (
                    a.clone(),
                    s.task_id.clone(),
                    s.delivered,
                    s.last_progress_at,
                )
            })
            .collect();
        for (agent, task_id, delivered, since) in in_flight {
            let Ok(Some(task)) = self.store.get_task(&task_id) else {
                // Task deleted out from under a live run: `delete_task`
                // already dropped the loop, so there is nothing to end.
                continue;
            };
            // 0 means wait forever, and it has to mean that for both
            // windows below — it is the setting that says "do not touch it".
            if task.stall_timeout_secs == 0 {
                continue;
            }
            // `Fresh` means no hook has ever arrived for this session: the
            // prompt was typed, but the CLI never took it as one.
            let never_started = matches!(
                self.store.get_agent(&agent),
                Ok(Some(a)) if a.status == AgentStatus::Fresh
            );
            let window = if never_started {
                task.stall_timeout_secs.min(FIRST_TURN_TIMEOUT_SECS)
            } else {
                task.stall_timeout_secs
            };
            if now.saturating_sub(since) < window as i64 * 1_000 {
                continue;
            }
            tracing::warn!(
                task = %task.id, agent = %agent, delivered, never_started,
                "run stalled past its watchdog"
            );
            let summary = if never_started {
                format!(
                    "stalled: no turn started in {} — a checkout the CLI has not seen \
                     before opens with a trust prompt that swallows the task's first prompt",
                    mins_label(window)
                )
            } else {
                format!(
                    "stalled: no turn ended in {} at iteration {} of {}",
                    mins_label(window),
                    delivered,
                    task.iterations
                )
            };
            self.end_task_run(&agent, &task, summary, TaskRunStatus::Stalled);
        }
    }

    /// Start a task now: resolve its checkout, spawn a session for it, and
    /// register the loop that will keep feeding it prompts until its
    /// iterations run out. Ignores the cron and the enabled flag — this is
    /// also what `RunTaskNow` calls.
    pub async fn run_task(self: &Arc<Self>, id: &TaskId) -> Result<AgentId> {
        let task = self.store.get_task(id)?.context("task not found")?;
        let started = epoch_ms();
        // The record exists before anything can go wrong, so "it never even
        // started" is a run in the history rather than a silence in it. Every
        // later step updates this row; nothing else inserts one.
        let mut run = new_run_record(&task, started);
        if let Err(e) = self.store.insert_task_run(&run) {
            // Not fatal: a run that cannot be recorded is still a run worth
            // doing, and the task row's outcome still reports it.
            tracing::warn!(task = %task.id, error = %e, "could not open a run record");
        }
        self.broadcast(ServerEvent::TaskRunUpserted { run: run.clone() });
        match self.start_task_run(&task, &mut run).await {
            Ok(agent_id) => {
                run.agent_id = Some(agent_id.clone());
                let _ = self.store.update_task_run(&run);
                self.store
                    .set_task_run_state(&task.id, started, "running", Some(&agent_id))?;
                self.rebroadcast_task(&task.id);
                self.broadcast(ServerEvent::TaskRunUpserted { run });
                Ok(agent_id)
            }
            Err(e) => {
                // The failure is the run's outcome, so it shows in the pane
                // rather than only in the daemon log.
                let outcome = format!("failed: {e:#}");
                let _ = self
                    .store
                    .set_task_run_state(&task.id, started, &outcome, None);
                run.ended_at = epoch_ms();
                run.status = TaskRunStatus::Failed;
                run.outcome = outcome;
                let _ = self.store.update_task_run(&run);
                self.write_run_report(&run, Some(&task), None).await;
                self.rebroadcast_task(&task.id);
                self.broadcast(ServerEvent::TaskRunUpserted { run });
                Err(e)
            }
        }
    }

    /// Runs whose daemon died under them. Called once at boot: a row that
    /// still says "running" is lying, and a task whose newest run reads
    /// running forever is one the user will never trust again.
    pub fn reconcile_unfinished_runs(self: &Arc<Self>) {
        let Ok(runs) = self.store.unfinished_task_runs() else {
            return;
        };
        for mut run in runs {
            tracing::info!(run = %run.id, task = %run.task_id, "closing a run the daemon outlived");
            run.ended_at = epoch_ms();
            run.status = TaskRunStatus::Stopped;
            run.outcome = format!(
                "stopped: the daemon restarted at iteration {} of {}",
                run.iterations_done.max(1),
                run.iterations_planned
            );
            let _ = self.store.update_task_run(&run);
            // The task row is only corrected when it is still showing this
            // run — a later run's outcome must not be overwritten by an old
            // one being tidied up.
            if let Ok(Some(task)) = self.store.get_task(&run.task_id) {
                if task.last_outcome.as_deref() == Some("running")
                    && task.last_run_at == run.started_at
                {
                    let _ = self.store.set_task_run_state(
                        &task.id,
                        task.last_run_at,
                        &run.outcome,
                        run.agent_id.as_ref(),
                    );
                }
            }
        }
    }

    async fn start_task_run(self: &Arc<Self>, task: &Task, run: &mut TaskRun) -> Result<AgentId> {
        let worktree = self.resolve_task_worktree(task).await?;
        if !self.cli_available_for_create(task.kind).await {
            bail!("{}", cli_missing_message(task.kind));
        }
        run.worktree_path = worktree.path.clone();
        run.branch = worktree.branch.clone();
        // The base ref is written before the agent starts, so the diff at the
        // end is the run's own work and not whatever the checkout was already
        // carrying. Best effort: a repo git cannot snapshot still gets a run,
        // it just gets one whose report says it has no diff.
        match git::snapshot_ref(
            &worktree.path,
            &run_ref(&run.id, "base"),
            &format!("[nebula] task `{}` — the tree before the run", task.name),
        )
        .await
        {
            Ok(sha) => run.base_ref = Some(sha),
            Err(e) => {
                tracing::warn!(task = %task.id, error = %e, "could not record the run's base tree")
            }
        }
        let _ = self.store.update_task_run(run);

        let agent = Agent {
            id: AgentId::generate(),
            worktree_id: worktree.id.clone(),
            name: task.name.clone(),
            status: AgentStatus::Fresh,
            archived: false,
            archived_at: 0,
            // A task's session is invisible to the status machine between
            // turns and must outlive `session_idle_timeout` — which is
            // exactly what pinning already means (see `reap_idle_sessions`).
            pinned: true,
            unseen: false,
            kind: task.kind,
            model: task.model.clone(),
            effort: task.effort.clone(),
            session_id: None,
            cloud_session_id: None,
            sort_order: 0,
            status_changed_at: epoch_ms(),
            alive: false,
        };
        // No auto-title: the row is named after the task on purpose, and a
        // rename would lose which task it came from.
        self.store.insert_agent_with_auto_title(&agent, false)?;
        let spawned = self.spawn_agent_session_with(
            &agent,
            &worktree,
            80,
            24,
            SpawnOpts {
                unattended: task.unattended,
                // Nobody is watching, so the scrollback ring is not a record
                // of anything — the file is.
                transcript: Some(run.transcript_path()),
                ..SpawnOpts::default()
            },
        );
        self.rollback_agent_on_spawn_error(&agent.id, spawned)?;
        self.task_loops.lock().unwrap().insert(
            agent.id.clone(),
            LoopState {
                task_id: task.id.clone(),
                run_id: run.id.clone(),
                delivered: 1,
                total: task.iterations,
                in_flight: true,
                last_progress_at: epoch_ms(),
            },
        );
        run.iterations_done = 1;
        let mut broadcast_agent = agent.clone();
        broadcast_agent.alive = true;
        self.broadcast(ServerEvent::EntityUpserted {
            entity: Entity::Agent(broadcast_agent),
        });
        // Iteration 1 goes through the same paste-and-submit path as every
        // later one, so a slash command runs the way it would for a human
        // and codex/cursor (which take no initial prompt at all) work too.
        self.deliver_task_prompt(&agent.id, task, 1, first_prompt_delay(), Some(run));
        Ok(agent.id)
    }

    /// The checkout a run happens in.
    async fn resolve_task_worktree(self: &Arc<Self>, task: &Task) -> Result<Worktree> {
        let (_, worktrees, _, _) = self.store.load_tree()?;
        match &task.target {
            TaskTarget::Root => worktrees
                .into_iter()
                .find(|w| w.project_id == task.project_id && w.is_main)
                .context("the project has no main checkout"),
            TaskTarget::Worktree(id) => self
                .store
                .get_worktree(id)?
                .filter(|w| w.project_id == task.project_id)
                .context("the task's worktree no longer exists"),
            TaskTarget::NewWorktree => {
                let branch = task_run_branch(task);
                // A previous run's checkout is reused rather than piling up
                // one worktree per night.
                if let Some(existing) = worktrees
                    .into_iter()
                    .find(|w| w.project_id == task.project_id && w.branch == branch)
                {
                    return Ok(existing);
                }
                let created = self
                    .create_worktree(&task.project_id, &branch, None)
                    .await?;
                let EntityId::Worktree(new_id) = created else {
                    bail!("worktree creation returned a non-worktree entity");
                };
                self.store
                    .get_worktree(&new_id)?
                    .context("worktree not found")
            }
        }
    }

    /// A turn ended: feed the session its next iteration, or retire the loop.
    /// Gated on the same two turn-end signals as `complete_pending_move` —
    /// a session sitting on a permission prompt never reaches either, so a
    /// blocked run stalls instead of being hammered.
    pub fn continue_task_loop(self: &Arc<Self>, id: &AgentId, event: &HookEvent) {
        // Checked first: a run that has just parked on a question will never
        // reach any of the turn-end signals below, so the loop has to be
        // retired here or not at all.
        if self.abandon_unattended_run_on_question(id, event) {
            return;
        }
        let turn_over = match event {
            HookEvent::Stop => true,
            HookEvent::Notification { notification_type } => {
                notification_type.as_deref() == Some("idle_prompt")
            }
            _ => false,
        };
        if !turn_over {
            return;
        }
        // The whole decision is made under one lock, so two turn-end hooks
        // racing can't both claim the same iteration.
        let next = {
            let mut loops = self.task_loops.lock().unwrap();
            let Some(state) = loops.get_mut(id) else {
                return;
            };
            let run_id = state.run_id.clone();
            if state.in_flight {
                return;
            }
            // A turn ended, so the run is demonstrably alive — restart the
            // watchdog's clock whichever branch we take below.
            state.last_progress_at = epoch_ms();
            if state.delivered >= state.total {
                let task_id = state.task_id.clone();
                // Left in place: `end_task_run` removes it, and it is what
                // tells the finisher which record to close.
                (task_id, None, run_id)
            } else {
                state.delivered += 1;
                state.in_flight = true;
                (state.task_id.clone(), Some(state.delivered), run_id)
            }
        };
        let (task_id, iteration, run_id) = next;
        let Ok(Some(task)) = self.store.get_task(&task_id) else {
            // Task deleted mid-run: leave the session alone, just stop.
            self.task_loops.lock().unwrap().remove(id);
            return;
        };
        match iteration {
            Some(n) => {
                tracing::info!(agent = %id, task = %task_id, iteration = n, "delivering next task iteration");
                // Progress goes on the record as it happens: a run killed
                // mid-flight is then honest about how far it got even if the
                // finisher never runs.
                let run = self
                    .store
                    .get_task_run(&run_id)
                    .ok()
                    .flatten()
                    .map(|mut r| {
                        r.iterations_done = n;
                        let _ = self.store.update_task_run(&r);
                        r
                    });
                self.deliver_task_prompt(id, &task, n, Duration::from_millis(0), run.as_ref());
            }
            None => {
                tracing::info!(agent = %id, task = %task_id, "task run finished its iterations");
                let summary = format!("ran {} of {}", task.iterations, task.iterations);
                self.end_task_run(id, &task, summary, TaskRunStatus::Completed);
            }
        }
    }

    /// An unattended run that stops to ask something is finished: nobody is
    /// there to answer, and a CLI sitting on a dialog emits no turn-end, so
    /// the loop would otherwise wait for the watchdog to notice half an hour
    /// later. Returns true when it ended the run.
    fn abandon_unattended_run_on_question(
        self: &Arc<Self>,
        id: &AgentId,
        event: &HookEvent,
    ) -> bool {
        let asked = match event {
            // Survives `--dangerously-skip-permissions` only when the CLI
            // decided the call was too dangerous to auto-approve.
            HookEvent::Notification { notification_type } => {
                notification_type.as_deref() == Some("permission_prompt")
            }
            // The flag does not cover this one at all: asking the user a
            // question is a tool, not a permission.
            HookEvent::PreToolUse { tool_name } => tool_name.as_deref() == Some("AskUserQuestion"),
            _ => false,
        };
        if !asked {
            return false;
        }
        let Some((task_id, delivered)) = self
            .task_loops
            .lock()
            .unwrap()
            .get(id)
            .map(|s| (s.task_id.clone(), s.delivered))
        else {
            return false;
        };
        let Ok(Some(task)) = self.store.get_task(&task_id) else {
            return false;
        };
        // An attended task asking a question is the system working: the user
        // is watching the pane and can answer it.
        if !task.unattended {
            return false;
        }
        tracing::info!(agent = %id, task = %task_id, "unattended run asked for input; ending it");
        let summary = format!(
            "stopped: asked for input at iteration {} of {}",
            delivered, task.iterations
        );
        self.end_task_run(id, &task, summary, TaskRunStatus::Stopped);
        true
    }

    /// The session behind a run is gone. Whatever the run was going to do
    /// next, it is not going to do it — retire the loop so the task stops
    /// reading "running" forever.
    fn abandon_task_run_on_exit(self: &Arc<Self>, id: &AgentId, exit_code: Option<i32>) {
        let Some((task_id, delivered)) = self
            .task_loops
            .lock()
            .unwrap()
            .get(id)
            .map(|s| (s.task_id.clone(), s.delivered))
        else {
            return;
        };
        let Ok(Some(task)) = self.store.get_task(&task_id) else {
            self.task_loops.lock().unwrap().remove(id);
            return;
        };
        tracing::info!(agent = %id, task = %task_id, exit_code, "run's session exited");
        // No code at all means the PTY went away without one (killed, or
        // the daemon reaped it) — "exited" without a number is the honest
        // way to say that.
        let how = match exit_code {
            Some(c) => format!("session exited ({c})"),
            None => "session exited".to_string(),
        };
        let summary = format!(
            "stopped: {} at iteration {} of {}",
            how, delivered, task.iterations
        );
        self.end_task_run(id, &task, summary, TaskRunStatus::Stopped);
    }

    /// The one way a run stops — completion, stall, question, or a dead
    /// session all come through here. Drops the loop first (so a late
    /// turn-end cannot revive it), records the outcome, and then, off the hot
    /// path, works out what the run actually did.
    ///
    /// A run whose loop is already gone has already ended: returning here is
    /// what keeps two racing stop paths from writing two outcomes.
    fn end_task_run(
        self: &Arc<Self>,
        agent: &AgentId,
        task: &Task,
        summary: String,
        status: TaskRunStatus,
    ) {
        let Some(state) = self.task_loops.lock().unwrap().remove(agent) else {
            return;
        };
        let _ = self
            .store
            .set_task_run_state(&task.id, task.last_run_at, &summary, Some(agent));
        self.rebroadcast_task(&task.id);
        // Off the hot path: this shells out to git several times and writes a
        // file, and the hook drain that usually calls us is holding up the
        // next turn of every other session.
        let daemon = self.clone();
        let agent = agent.clone();
        let task = task.clone();
        tokio::spawn(async move {
            daemon
                .finish_run_record(
                    state.run_id,
                    state.delivered,
                    &agent,
                    &task,
                    summary,
                    status,
                )
                .await;
        });
    }

    /// Everything a finished run leaves behind: the head ref, the diff
    /// against the base recorded at the start, the optional snapshot branch,
    /// the report, and a task outcome line that now carries the diffstat.
    async fn finish_run_record(
        self: &Arc<Self>,
        run_id: TaskRunId,
        delivered: u32,
        agent: &AgentId,
        task: &Task,
        summary: String,
        status: TaskRunStatus,
    ) {
        let Ok(Some(mut run)) = self.store.get_task_run(&run_id) else {
            // No record to finish (an old run from before the table, or a
            // failed insert): the task row's outcome is already written, and
            // the snapshot below is the only other thing owed.
            if task.commit_on_finish {
                if let Ok(Some(where_)) = self.snapshot_task_run(agent, task).await {
                    let _ = self.store.set_task_run_state(
                        &task.id,
                        task.last_run_at,
                        &format!("{summary} · {where_}"),
                        Some(agent),
                    );
                    self.rebroadcast_task(&task.id);
                }
            }
            return;
        };
        run.ended_at = epoch_ms();
        run.status = status;
        run.outcome = summary.clone();
        run.iterations_done = delivered;
        run.agent_id = Some(agent.clone());

        let repo = run.worktree_path.clone();
        let mut diff = None;
        if repo.is_dir() {
            match git::snapshot_ref(
                &repo,
                &run_ref(&run.id, "head"),
                &format!("[nebula] task `{}` — the tree after the run", task.name),
            )
            .await
            {
                Ok(sha) => run.head_ref = Some(sha),
                Err(e) => {
                    tracing::warn!(run = %run.id, error = %e, "could not record the run's head tree")
                }
            }
            if let (Some(base), Some(head)) = (run.base_ref.clone(), run.head_ref.clone()) {
                match git::diff_summary(&repo, &base, &head).await {
                    Ok(d) => {
                        run.files_changed = d.files.len() as u32;
                        run.insertions = d.insertions;
                        run.deletions = d.deletions;
                        diff = Some(d);
                    }
                    Err(e) => tracing::warn!(run = %run.id, error = %e, "run diff failed"),
                }
            }
        } else {
            tracing::warn!(run = %run.id, path = %repo.display(), "the run's checkout is gone");
        }

        if task.commit_on_finish {
            match self.snapshot_task_run(agent, task).await {
                Ok(Some(where_)) => run.snapshot = Some(where_),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(task = %task.id, error = %e, "run snapshot failed");
                    run.snapshot = Some(format!("commit failed: {e:#}"));
                }
            }
        }

        self.write_run_report(&run, Some(task), diff.as_ref()).await;
        let _ = self.store.update_task_run(&run);

        // The outcome line grows the two things a person scanning the pane
        // wants next: how much changed, and where it was put.
        let mut line = summary;
        if run.files_changed > 0 {
            line.push_str(&format!(
                " · {}",
                report::diffstat_label(run.files_changed, run.insertions, run.deletions)
            ));
        }
        match run.snapshot.as_deref() {
            Some(where_) => line.push_str(&format!(" · {where_}")),
            None if task.commit_on_finish => line.push_str(" · nothing to commit"),
            None => {}
        }
        let _ = self
            .store
            .set_task_run_state(&task.id, task.last_run_at, &line, Some(agent));
        self.rebroadcast_task(&task.id);
        self.broadcast(ServerEvent::TaskRunUpserted { run });
    }

    /// Render `report.md` into the run's directory. Best effort by design:
    /// nothing about a run should fail because its write-up could not be
    /// saved, but the daemon log says so when it could not.
    async fn write_run_report(
        self: &Arc<Self>,
        run: &TaskRun,
        task: Option<&Task>,
        diff: Option<&git::DiffSummary>,
    ) {
        let agent_summary = tokio::fs::read_to_string(run.summary_path()).await.ok();
        let transcript_bytes = tokio::fs::metadata(run.transcript_path())
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let text = report::render_report(&ReportInput {
            run,
            task,
            diff,
            agent_summary: agent_summary.as_deref(),
            transcript_bytes,
        });
        if let Err(e) = tokio::fs::create_dir_all(&run.dir).await {
            tracing::warn!(run = %run.id, error = %e, "could not create the run directory");
            return;
        }
        if let Err(e) = tokio::fs::write(run.report_path(), text).await {
            tracing::warn!(run = %run.id, error = %e, "could not write the run report");
        }
    }

    /// Past runs, newest first. `task` None spans every task.
    pub fn list_task_runs(
        &self,
        task: Option<&TaskId>,
        since_ms: i64,
        limit: u32,
    ) -> Result<Vec<TaskRun>> {
        self.store.list_task_runs(task, since_ms, limit)
    }

    /// One of a run's files, read off disk. The transcript is tail-capped on
    /// the way out: a client asking for it wants to see how the run ended,
    /// not to be sent thirty megabytes of screen repaints.
    pub async fn read_run_artifact(&self, id: &TaskRunId, part: RunArtifact) -> Result<String> {
        let run = self
            .store
            .get_task_run(id)?
            .context("no run by that id — it may have been deleted with its task")?;
        let path = run.dir.join(part.file_name());
        match part {
            RunArtifact::Transcript => {
                let raw = tokio::fs::read(&path)
                    .await
                    .with_context(|| format!("no transcript at {}", path.display()))?;
                Ok(tail_text(&raw, ARTIFACT_TAIL_BYTES))
            }
            _ => tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("no {} at {}", part.file_name(), path.display())),
        }
    }

    /// The overnight page across every task.
    pub fn task_run_digest(&self, since_ms: i64) -> Result<String> {
        let runs = self.store.list_task_runs(None, since_ms, 0)?;
        Ok(report::render_digest(&runs, since_ms, epoch_ms()))
    }

    /// Capture the run's checkout on `task/<slug>/<stamp>`. Returns the
    /// branch and short hash, or None when the run changed nothing.
    async fn snapshot_task_run(
        self: &Arc<Self>,
        agent: &AgentId,
        task: &Task,
    ) -> Result<Option<String>> {
        let row = self
            .store
            .get_agent(agent)?
            .context("the run's session row is gone")?;
        let worktree = self
            .store
            .get_worktree(&row.worktree_id)?
            .context("the run's checkout is gone")?;
        let branch = task_snapshot_branch(task);
        let message = format!(
            "[nebula] task `{}`\n\nWorking tree as the run left it.",
            task.name
        );
        let hash = crate::git::snapshot_branch(&worktree.path, &branch, &message).await?;
        Ok(hash.map(|h| format!("{branch} {h}")))
    }

    /// Type one iteration's prompt at the agent's input box and submit it.
    ///
    /// There is no other way in: the CLI owns its own TUI, so a prompt is
    /// keystrokes. The text goes as a bracketed paste (the same bytes the
    /// TUI sends for ⌘V, so the child knows it is a paste and doesn't
    /// interpret a newline inside it), then a beat later the submit key on
    /// its own — the CLIs need the paste to settle before Enter means "run
    /// this" rather than "insert a newline".
    fn deliver_task_prompt(
        self: &Arc<Self>,
        id: &AgentId,
        task: &Task,
        iteration: u32,
        delay: Duration,
        run: Option<&TaskRun>,
    ) {
        let sref = SessionRef::Agent(id.clone());
        // The last turn is also asked to leave an account of itself. This is
        // the only summary written by something that knows what the work was
        // for — the diff says what changed, not why.
        let summary_path = run.map(|r| r.summary_path());
        let text = task_prompt_text(task, iteration, summary_path.as_deref());
        let daemon = self.clone();
        let agent_id = id.clone();
        let task_id = task.id.clone();
        tokio::spawn(async move {
            if delay > Duration::ZERO {
                tokio::time::sleep(delay).await;
            }
            let Some(session) = daemon.session(&sref) else {
                tracing::warn!(agent = %agent_id, task = %task_id, "session died before its prompt landed");
                daemon.task_loops.lock().unwrap().remove(&agent_id);
                return;
            };
            let mut paste = b"\x1b[200~".to_vec();
            paste.extend_from_slice(text.as_bytes());
            paste.extend_from_slice(b"\x1b[201~");
            if let Err(e) = session.write_input(&paste) {
                tracing::warn!(agent = %agent_id, error = %e, "could not paste the task prompt");
            } else {
                // The submit is its own write a beat later: the CLIs process
                // a bracketed paste asynchronously, and an Enter arriving in
                // the same read lands as a newline inside the text.
                tokio::time::sleep(SUBMIT_GAP).await;
                if let Err(e) = session.write_input(b"\r") {
                    tracing::warn!(agent = %agent_id, error = %e, "could not submit the task prompt");
                }
            }
            // The window closes either way: the iteration was claimed before
            // this task was spawned, and leaving it open would stall the loop
            // on the next turn end.
            if let Some(state) = daemon.task_loops.lock().unwrap().get_mut(&agent_id) {
                state.in_flight = false;
                // Delivering counts as progress: the watchdog is measuring
                // the turn that starts now, not the one that just ended.
                state.last_progress_at = epoch_ms();
            }
        });
    }

    fn rebroadcast_task(self: &Arc<Self>, id: &TaskId) {
        if let Ok(Some(task)) = self.store.get_task(id) {
            self.broadcast(ServerEvent::EntityUpserted {
                entity: Entity::Task(task),
            });
        }
    }

    /// Iterations delivered so far for a run, for the tests.
    #[cfg(test)]
    fn task_loop_progress(&self, id: &AgentId) -> Option<(u32, u32)> {
        self.task_loops
            .lock()
            .unwrap()
            .get(id)
            .map(|s| (s.delivered, s.total))
    }

    // ---- attach / spawn ----

    /// Get the live session for an entity, lazily (re)spawning its PTY when
    /// none is running (restored agents, closed shells).
    pub fn ensure_session(
        self: &Arc<Self>,
        sref: &SessionRef,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<PtySession>> {
        if let Some(s) = self.session(sref) {
            return Ok(s);
        }
        match sref {
            SessionRef::Agent(id) => {
                let agent = self.store.get_agent(id)?.context("agent not found")?;
                if agent.archived {
                    bail!("agent is archived — unarchive it first");
                }
                let worktree = self
                    .store
                    .get_worktree(&agent.worktree_id)?
                    .context("worktree not found")?;
                let session = self.spawn_agent_session(&agent, &worktree, cols, rows)?;
                let mut broadcast_agent = agent;
                broadcast_agent.alive = true;
                self.broadcast(ServerEvent::EntityUpserted {
                    entity: Entity::Agent(broadcast_agent),
                });
                Ok(session)
            }
            SessionRef::Terminal(id) => {
                let term = self.store.get_terminal(id)?.context("terminal not found")?;
                let worktree = self
                    .store
                    .get_worktree(&term.worktree_id)?
                    .context("worktree not found")?;
                let session = self.spawn_terminal_session(&term, &worktree, cols, rows)?;
                let mut broadcast_term = term;
                broadcast_term.alive = true;
                self.broadcast(ServerEvent::EntityUpserted {
                    entity: Entity::Terminal(broadcast_term),
                });
                Ok(session)
            }
        }
    }

    /// Boot every dead, non-archived session under `worktree_id` (agents and
    /// terminals) so a later Attach replays an already-running screen.
    /// Already-alive sessions pass through ensure_session untouched; one
    /// session failing to spawn (missing CLI, deleted checkout) is logged
    /// and doesn't stop the rest.
    pub fn prewarm_worktree_sessions(
        self: &Arc<Self>,
        worktree_id: &WorktreeId,
        cols: u16,
        rows: u16,
    ) {
        if !crate::config::Config::load().prewarm_sessions {
            return;
        }
        let Ok((_, _, agents, terminals)) = self.store.load_tree() else {
            return;
        };
        let srefs = agents
            .iter()
            .filter(|a| &a.worktree_id == worktree_id && !a.archived)
            .map(|a| SessionRef::Agent(a.id.clone()))
            .chain(
                terminals
                    .iter()
                    .filter(|t| &t.worktree_id == worktree_id)
                    .map(|t| SessionRef::Terminal(t.id.clone())),
            );
        for sref in srefs {
            // The prewarm doubles as a "user is looking here" signal for
            // the idle reaper, for alive sessions as much as fresh spawns.
            self.touch_session(&sref);
            if let Err(e) = self.ensure_session(&sref, cols, rows) {
                tracing::debug!(session = ?sref, error = %e, "session prewarm failed");
            }
        }
    }

    fn spawn_agent_session(
        self: &Arc<Self>,
        agent: &Agent,
        worktree: &Worktree,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<PtySession>> {
        self.spawn_agent_session_with(agent, worktree, cols, rows, SpawnOpts::default())
    }

    /// The general spawn. Everything in `opts` is intentionally transient:
    /// later restarts/resumes follow the persisted Agent fields — a Cloud
    /// row's `cloud_session_id` routes a restart back through
    /// `attach_cloud_agent`, everything else takes the plain local-session
    /// path, and a task's unattended launch is re-derived from its Task row.
    fn spawn_agent_session_with(
        self: &Arc<Self>,
        agent: &Agent,
        worktree: &Worktree,
        cols: u16,
        rows: u16,
        opts: SpawnOpts<'_>,
    ) -> Result<Arc<PtySession>> {
        let SpawnOpts {
            cloud,
            initial_prompt,
            unattended,
            transcript,
        } = opts;
        // Whatever spawns this agent, it runs in `worktree` from here: a
        // relocation still pending for it has been overtaken.
        self.pending_moves.lock().unwrap().remove(&agent.id);
        // Managed status hooks; a failure here degrades to "no status
        // updates", never blocks the spawn.
        let install_result = match agent.kind {
            AgentKind::Claude => hooks::installer::install_claude_hooks(&worktree.path),
            // Codex's hooks live in its home, not the worktree, so one
            // trust approval covers every worktree (see installer docs);
            // any per-worktree copy an older nebula left is pruned.
            AgentKind::Codex => {
                hooks::installer::install_codex_hooks(&hooks::installer::codex_home())
                    .and_then(|()| hooks::installer::prune_codex_worktree_hooks(&worktree.path))
            }
            // Cursor also gets the managed auto-title project rule — its
            // hook dialect has no context-injection channel.
            AgentKind::Cursor => hooks::installer::install_cursor_hooks(&worktree.path)
                .and_then(|()| hooks::installer::install_cursor_title_rule(&worktree.path)),
        };
        if let Err(e) = install_result {
            tracing::warn!(error = %e, cwd = %worktree.path.display(), "hook install failed");
        }

        // NEBULA_AGENT_CMD overrides for tests; default is the kind's CLI.
        let cmd_override = std::env::var("NEBULA_AGENT_CMD").ok();
        let (program, args, resumed) = match cloud {
            Some(launch) => claude_cloud_spawn_command(
                launch,
                agent.model.as_deref(),
                agent.effort.as_deref(),
                cmd_override.as_deref(),
            ),
            None => agent_spawn_command_with(
                agent.kind,
                agent.session_id.as_deref(),
                agent.model.as_deref(),
                agent.effort.as_deref(),
                cmd_override.as_deref(),
                initial_prompt,
                LaunchFlags {
                    guidance: true,
                    unattended,
                },
            ),
        };
        // Run the agent through the user's login+interactive shell so it sees
        // the same env as a Terminal.app tab (~/.zprofile, ~/.zshrc,
        // path_helper) instead of the daemon's inherited-at-boot env.
        // Overrides (tests) stay verbatim.
        let (program, args) = if cmd_override.is_some() {
            (program, args)
        } else {
            login_shell_wrap(&user_shell(), &program, &args)
        };

        let spec = SpawnSpec {
            program,
            args,
            cwd: worktree.path.clone(),
            env: vec![
                ("NEBULA_AGENT_ID".into(), agent.id.to_string()),
                (
                    "NEBULA_API_URL".into(),
                    format!("http://127.0.0.1:{}", self.hook_env.port),
                ),
                ("NEBULA_API_TOKEN".into(), self.hook_env.token.clone()),
            ],
            scrub_env: scrubbed_env_names(),
            cols,
            rows,
            transcript,
        };
        let sref = SessionRef::Agent(agent.id.clone());
        let session = PtySession::spawn(sref, spec)?;
        self.install_session(session.clone());
        if resumed {
            self.arm_resume_fallback(agent.clone(), worktree.clone(), session.clone(), cols, rows);
        }
        match cloud {
            // The create prints the session id and (on accounts without
            // the attach rollout) exits at once: capture it off the output.
            Some(CloudLaunch::Create(_)) => session.arm_cloud_scan(),
            Some(CloudLaunch::Attach(id)) => {
                session.arm_cloud_scan();
                self.arm_cloud_attach_fallback(
                    agent.clone(),
                    worktree.clone(),
                    session.clone(),
                    id.to_string(),
                    cols,
                    rows,
                );
            }
            Some(CloudLaunch::Teleport(_)) | None => {}
        }
        Ok(session)
    }

    /// A resumed session (`claude --resume` / `codex resume` /
    /// `cursor-agent --resume`) dies fast when
    /// it is stale/deleted — fall back to a fresh session instead of leaving
    /// a dead pane.
    fn arm_resume_fallback(
        self: &Arc<Self>,
        agent: Agent,
        worktree: Worktree,
        session: Arc<PtySession>,
        cols: u16,
        rows: u16,
    ) {
        let daemon = self.clone();
        let mut rx = session.events.subscribe();
        tokio::spawn(async move {
            let early_exit = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    match rx.recv().await {
                        Ok(PtyEvent::Exited { exit_code }) => return exit_code.unwrap_or(1) != 0,
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return false,
                    }
                }
            })
            .await;
            if early_exit != Ok(true) {
                return;
            }
            // A deliberate kill looks identical to a failed resume from here:
            // the agent may have been archived or deleted inside the window —
            // never resurrect those.
            match daemon.store.get_agent(&agent.id) {
                Ok(Some(current)) if !current.archived => {}
                _ => return,
            }
            tracing::info!(agent = %agent.id, "resume failed fast — respawning fresh");
            let _ = daemon.store.set_agent_session_id(&agent.id, None);
            let mut fresh = agent.clone();
            fresh.session_id = None;
            if let Ok(_session) = daemon.spawn_agent_session(&fresh, &worktree, cols, rows) {
                let mut broadcast_agent = fresh;
                broadcast_agent.alive = true;
                daemon.broadcast(ServerEvent::EntityUpserted {
                    entity: Entity::Agent(broadcast_agent),
                });
            }
        });
    }

    fn spawn_terminal_session(
        self: &Arc<Self>,
        terminal: &TerminalTab,
        worktree: &Worktree,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<PtySession>> {
        // `-l` makes it a login shell, matching Terminal.app: zsh then sources
        // /etc/zprofile (path_helper), ~/.zprofile, and ~/.zshrc.
        let spec = SpawnSpec {
            program: user_shell(),
            args: vec!["-l".into()],
            cwd: worktree.path.clone(),
            env: vec![],
            scrub_env: scrubbed_env_names(),
            cols,
            rows,
            transcript: None,
        };
        let sref = SessionRef::Terminal(terminal.id.clone());
        let session = PtySession::spawn(sref, spec)?;
        self.install_session(session.clone());
        Ok(session)
    }

    fn install_session(self: &Arc<Self>, session: Arc<PtySession>) {
        self.touch_session(&session.sref);
        self.sessions
            .lock()
            .unwrap()
            .insert(session.sref.clone(), session.clone());
        self.watch_for_exit(session);
    }

    /// Once the child dies: drop it from the registry, feed the status
    /// machine (agents), and tell subscribers the entity is no longer alive.
    fn watch_for_exit(self: &Arc<Self>, session: Arc<PtySession>) {
        let daemon = self.clone();
        let mut rx = session.events.subscribe();
        let sref = session.sref.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(PtyEvent::Exited { exit_code }) => {
                        // Deliberate kills (archive/restart/delete) remove the
                        // entry first — only a *natural* death of the still-
                        // registered session drives status, so a restart never
                        // flags the fresh PTY's agent as terminated.
                        let was_registered = {
                            let mut sessions = daemon.sessions.lock().unwrap();
                            match sessions.get(&sref) {
                                Some(current) if Arc::ptr_eq(current, &session) => {
                                    sessions.remove(&sref);
                                    true
                                }
                                _ => false,
                            }
                        };
                        if was_registered {
                            daemon.session_interest.lock().unwrap().remove(&sref);
                        }
                        if !was_registered {
                            break;
                        }
                        tracing::info!(session = ?sref, exit_code, "session exited");
                        if let SessionRef::Agent(id) = &sref {
                            daemon.apply_hook_event(
                                id,
                                HookEvent::SessionEnded { exit_code },
                                None,
                            );
                            // A task run whose session died is over. Without
                            // this the loop entry outlives the PTY and the
                            // task reads "running" until somebody looks.
                            daemon.abandon_task_run_on_exit(id, exit_code);
                        }
                        let upsert = match &sref {
                            SessionRef::Agent(id) => daemon.agent_entity(id).map(Entity::Agent),
                            SessionRef::Terminal(id) => {
                                daemon.terminal_entity(id).map(Entity::Terminal)
                            }
                        };
                        if let Ok(entity) = upsert {
                            daemon.broadcast(ServerEvent::EntityUpserted { entity });
                        }
                        break;
                    }
                    // The CLI's own busy/idle bit, read off its output. It is
                    // the only end-of-turn news after a user cancel: Claude
                    // Code fires no Stop for an interrupted turn, and
                    // suppresses the idle notification because the user just
                    // pressed a key. See `pty::progress`.
                    Ok(PtyEvent::Progress { busy }) => {
                        if let SessionRef::Agent(id) = &sref {
                            daemon.apply_hook_event(id, HookEvent::Progress { busy }, None);
                        }
                    }
                    // The Cloud session this row launched, read off the
                    // `claude --cloud` output. Persisted at once — the child
                    // is typically gone within milliseconds of printing it —
                    // and re-broadcast so the row grows its `cloud` badge and
                    // its attach menu entry.
                    Ok(PtyEvent::CloudSession { id: cloud_id }) => {
                        if let SessionRef::Agent(id) = &sref {
                            match daemon.store.set_agent_cloud_session_id(id, Some(&cloud_id)) {
                                Ok(()) => {
                                    tracing::info!(agent = %id, cloud_session = %cloud_id, "cloud session id captured");
                                    if let Ok(agent) = daemon.agent_entity(id) {
                                        daemon.broadcast(ServerEvent::EntityUpserted {
                                            entity: Entity::Agent(agent),
                                        });
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(agent = %id, error = %e, "cloud session id not persisted")
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // A fire-hosing child can push progress edges off the
                        // broadcast queue. The scanner itself never lags, so
                        // reconcile from its current reading rather than
                        // leaving the status stuck on a dropped edge.
                        if let (SessionRef::Agent(id), Some(busy)) =
                            (&sref, session.progress_busy())
                        {
                            daemon.apply_hook_event(id, HookEvent::Progress { busy }, None);
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

/// Program + args for an agent PTY. An override (tests) is used verbatim —
/// no resume args. Otherwise the kind picks the CLI and its resume shape:
/// `claude --resume <sid>` and `cursor-agent --resume <sid>` (flag) vs
/// `codex resume <sid>` (subcommand, so resume args must lead). Codex and
/// cursor always get their skip-permissions flag (`--yolo` / `--force`),
/// appended after the resume args — same convention as Mission Control.
/// Model/effort choices follow: `claude --model m --effort e`,
/// `codex -m m -c model_reasoning_effort=e` (cursor has neither knob).
/// Claude then gets nebula's worktree guidance appended to its system
/// prompt, and an `initial_prompt` — the relocation notice a `nebula
/// worktree` respawn opens with — goes last, as Claude's positional prompt
/// (codex and cursor take none; their resumes wait for the user).
///
/// The plain shape, as every restart/resume spawns it: no initial prompt,
/// guidance on. Tests assert against this; the daemon calls the full form.
#[cfg(test)]
fn agent_spawn_command(
    kind: AgentKind,
    session_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cmd_override: Option<&str>,
) -> (String, Vec<String>, bool) {
    agent_spawn_command_with(
        kind,
        session_id,
        model,
        effort,
        cmd_override,
        None,
        LaunchFlags::interactive(),
    )
}

/// What nebula appends to Claude's system prompt: how to take a "do this
/// in a worktree" request through nebula (`Daemon::enter_worktree`) instead
/// of Claude's own EnterWorktree tool, whose checkout lands under
/// `<repo>/.claude/worktrees/` on a `worktree-*` branch — a layout the
/// worktree list only adopts after the fact, and not where a nebula user
/// keeps their worktrees. Claude only: codex and cursor have no
/// system-prompt flag, and no EnterWorktree to steer away from.
pub const CLAUDE_WORKTREE_GUIDANCE: &str = "[nebula] This session runs inside nebula, which \
manages this project's git worktrees. When the user asks you to work in a worktree (\"do this in a \
worktree\", \"in a new worktree\", \"branch this off in its own checkout\"), do not use the \
EnterWorktree tool and do not run `git worktree add` yourself. Run this shell command instead, \
exactly once:\n\n  nebula worktree <name>\n\nwhere <name> is the branch name the user gave, or a \
short kebab-case name for the task (`nebula worktree` with no name invents one; `--base <ref>` picks \
the start point). nebula creates the worktree, associates this session with it, and relocates the \
session into it once your current turn ends. So when the command succeeds, end your turn at once: \
tell the user in one line that the session is moving into the worktree, and make no further tool \
calls or edits — you will be resumed inside the worktree with a prompt to carry on there. If the \
command fails, report the error and carry on in the current checkout.";

/// The prompt a relocated Claude session is resumed with: it names the
/// checkout the process now runs in and asks for the work to pick back up
/// there, so the user never has to type "continue".
fn relocation_prompt(worktree: &Worktree) -> String {
    format!(
        "[nebula] This session now runs inside the worktree `{}` at {} — your working \
         directory is that checkout. Continue the user's most recent request there.",
        worktree.branch,
        worktree.path.display()
    )
}

/// Branch a `NewWorktree` task runs in. Derived from the task's name so a
/// nightly job keeps returning to the same checkout instead of leaving one
/// worktree per run behind, and prefixed so it is obvious in `git branch`
/// where it came from.
fn task_run_branch(task: &Task) -> String {
    format!("task-{}", task_slug(task))
}

/// What one iteration actually types at the agent.
///
/// The last turn of a loop can carry a different prompt: max iterations is
/// still the only exit, but the run gets to spend its final turn landing the
/// work rather than being cut off mid-thought. A single-turn task has no turn
/// to spare, so it never wraps up.
fn task_prompt_text(task: &Task, iteration: u32, summary_path: Option<&Path>) -> String {
    let wrap_up =
        task.iterations > 1 && iteration == task.iterations && task.final_prompt.is_some();
    let body = match (wrap_up, task.final_prompt.as_deref()) {
        (true, Some(f)) => f,
        _ => task.prompt.as_str(),
    };
    let mut text = if task.iterations > 1 {
        format!(
            "[nebula] Task `{}` — iteration {} of {}{}.\n{}",
            task.name,
            iteration,
            task.iterations,
            if wrap_up { " (wrap-up)" } else { "" },
            body
        )
    } else {
        format!("[nebula] Task `{}`.\n{}", task.name, body)
    };
    // Only on the way out, and only once: an instruction repeated every turn
    // would have the agent rewriting its summary instead of doing the work,
    // and one asked for on turn 2 of 5 would describe a third of a run.
    if iteration >= task.iterations {
        if let Some(path) = summary_path {
            text.push_str(&format!(
                "\n\nBefore you finish, write a short account of this run to `{}` — what you \
                 changed, what you decided, and anything left undone. Markdown, 20 lines at \
                 most. Nobody watched this run; that file is what they will read instead.",
                path.display()
            ));
        }
    }
    text
}

/// How much of a run's transcript a client is sent when it asks. An
/// overnight run's file is measured in megabytes of screen repaints; the tail
/// is the part that says how it ended, and anything more belongs to `less`.
const ARTIFACT_TAIL_BYTES: usize = 256 * 1024;

/// A fresh run record, before anything is known about it beyond which task
/// it came from and when it started.
fn new_run_record(task: &Task, started: i64) -> TaskRun {
    let id = TaskRunId::generate();
    TaskRun {
        dir: run_dir(task, started, &id),
        id,
        task_id: task.id.clone(),
        project_id: task.project_id.clone(),
        task_name: task.name.clone(),
        agent_id: None,
        started_at: started,
        ended_at: 0,
        status: TaskRunStatus::Running,
        outcome: "running".to_string(),
        iterations_planned: task.iterations,
        iterations_done: 0,
        worktree_path: PathBuf::new(),
        branch: String::new(),
        base_ref: None,
        head_ref: None,
        snapshot: None,
        files_changed: 0,
        insertions: 0,
        deletions: 0,
    }
}

/// Where a run keeps its files: `<data>/task-runs/<slug>/<stamp>-<tail>/`.
/// Stamp first so a directory listing is chronological, and the id's tail
/// after it so two runs of the same task in the same second cannot collide.
fn run_dir(task: &Task, started: i64, id: &TaskRunId) -> PathBuf {
    let stamp = match chrono::Local.timestamp_millis_opt(started).single() {
        Some(t) => t.format("%Y%m%d-%H%M%S").to_string(),
        None => started.to_string(),
    };
    let tail: String = id
        .as_str()
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    paths::task_runs_dir()
        .join(task_slug(task))
        .join(format!("{stamp}-{}", tail.to_ascii_lowercase()))
}

/// The refs a run's before and after trees are kept under. Outside
/// `refs/heads` so they never clutter a branch list, and named after the run
/// so `git for-each-ref refs/nebula/runs` is a readable index of them.
fn run_ref(id: &TaskRunId, part: &str) -> String {
    format!("refs/nebula/runs/{id}/{part}")
}

/// The last `cap` bytes of a file, as text, saying so when it truncated.
/// Byte-sliced rather than char-sliced, so the cut can land mid-codepoint —
/// `from_utf8_lossy` absorbs that, and this is a terminal dump either way.
fn tail_text(raw: &[u8], cap: usize) -> String {
    if raw.len() <= cap {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let skipped = raw.len() - cap;
    format!(
        "[nebula] …{skipped} earlier bytes not shown; the whole file is on disk\n{}",
        String::from_utf8_lossy(&raw[skipped..])
    )
}

/// Branch a finished run is captured on. Stamped rather than reused: each
/// night is its own reviewable commit, and nothing a previous run recorded
/// can be overwritten by a later one.
fn task_snapshot_branch(task: &Task) -> String {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    format!("task/{}/{}", task_slug(task), stamp)
}

/// A task name reduced to something git will take as a branch component.
fn task_slug(task: &Task) -> String {
    let slug: String = task
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').replace("--", "-");
    if slug.is_empty() {
        // Names can be entirely punctuation; the id keeps the branch unique
        // and still traceable back to the row.
        task.id.as_str().to_ascii_lowercase()
    } else {
        slug
    }
}

/// "30m" / "2h" — a watchdog window, for an outcome line the user reads.
fn mins_label(secs: u32) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3_600),
    }
}

fn agent_spawn_command_with(
    kind: AgentKind,
    session_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cmd_override: Option<&str>,
    initial_prompt: Option<&str>,
    flags: LaunchFlags,
) -> (String, Vec<String>, bool) {
    if let Some(cmd) = cmd_override {
        let mut parts = cmd.split_whitespace().map(String::from).collect::<Vec<_>>();
        if parts.is_empty() {
            parts.push(kind.cli_program().into());
        }
        let program = parts.remove(0);
        return (program, parts, false);
    }
    let program = kind.cli_program().to_string();
    let (mut args, resumed) = match (kind, session_id) {
        (AgentKind::Claude, Some(sid)) => (vec!["--resume".to_string(), sid.to_string()], true),
        (AgentKind::Codex, Some(sid)) => (vec!["resume".to_string(), sid.to_string()], true),
        (AgentKind::Cursor, Some(sid)) => (vec!["--resume".to_string(), sid.to_string()], true),
        (_, None) => (Vec::new(), false),
    };
    // Codex and cursor always skip permission prompts (nebula has never
    // been able to answer one for them). Claude only does so when the launch
    // is explicitly unattended — a task marked as such, where a prompt would
    // simply stall the run forever with nobody there to see it.
    match kind {
        AgentKind::Codex => args.push("--yolo".to_string()),
        AgentKind::Cursor => args.push("--force".to_string()),
        AgentKind::Claude => {
            if flags.unattended {
                args.push("--dangerously-skip-permissions".to_string());
            }
        }
    }
    match kind {
        AgentKind::Claude => {
            if let Some(m) = model {
                args.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(e) = effort {
                args.extend(["--effort".to_string(), e.to_string()]);
            }
            if flags.guidance {
                args.extend([
                    "--append-system-prompt".to_string(),
                    CLAUDE_WORKTREE_GUIDANCE.to_string(),
                ]);
            }
            if let Some(p) = initial_prompt {
                args.push(p.to_string());
            }
        }
        AgentKind::Codex => {
            if let Some(m) = model {
                args.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(e) = effort {
                args.extend(["-c".to_string(), format!("model_reasoning_effort={e}")]);
            }
        }
        AgentKind::Cursor => {}
    }
    (program, args, resumed)
}

/// How a Claude PTY enters the Cloud: dispatch a fresh task, attach live
/// to the session it created, or teleport that session into a local one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloudLaunch<'a> {
    Create(&'a str),
    Attach(&'a str),
    Teleport(&'a str),
}

/// Branch (and so directory) of the worktree a Cloud row is re-homed into
/// before attaching: the CLI checks the cloud branch out on top of it, so
/// the name only has to be stable per session and safe for git.
fn cloud_worktree_branch(cloud_id: &str) -> String {
    let suffix: String = cloud_id
        .trim_start_matches("session_")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let tail = suffix.len().saturating_sub(8);
    format!("cloud-{}", &suffix[tail..])
}

/// Cloud launches are one-shot variations of the normal fresh-Claude
/// command. Keeping them a wrapper leaves every resume/restart caller on
/// the persisted local-session contract, and makes the no-override argument
/// shape directly unit-testable. No worktree guidance either: the Cloud
/// sandbox has no nebula CLI to follow it with. Values bind with `=`
/// (`--cloud=<task>`, `--cloud=<id>`, `--teleport=<id>`): both flags take an
/// *optional* value, so a separate argv item that starts with `--` would be
/// parsed as another Claude flag.
fn claude_cloud_spawn_command(
    launch: CloudLaunch<'_>,
    model: Option<&str>,
    effort: Option<&str>,
    cmd_override: Option<&str>,
) -> (String, Vec<String>, bool) {
    let (program, mut args, resumed) = agent_spawn_command_with(
        AgentKind::Claude,
        None,
        model,
        effort,
        cmd_override,
        None,
        // Cloud launches carry no worktree guidance (the cloud runner owns
        // the checkout) and never skip permissions locally.
        LaunchFlags::default(),
    );
    if cmd_override.is_none() {
        let flag = match launch {
            CloudLaunch::Create(task) => format!("--cloud={task}"),
            CloudLaunch::Attach(id) => format!("--cloud={id}"),
            CloudLaunch::Teleport(id) => format!("--teleport={id}"),
        };
        args.insert(0, flag);
    }
    (program, args, resumed)
}

/// Normalize an agent-supplied title: control characters become spaces,
/// whitespace collapses, and over-long titles are cut — models occasionally
/// hand over a whole sentence no matter what the instruction says.
fn sanitize_title(raw: &str) -> String {
    const MAX_CHARS: usize = 60;
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut title = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.chars().count() > MAX_CHARS {
        title = title.chars().take(MAX_CHARS).collect();
        title.truncate(title.trim_end().len());
    }
    title
}

/// Canonicalize for path containment tests, falling back to the raw path
/// when it doesn't resolve (deleted checkout, not-yet-created dir). macOS
/// symlinks (`/tmp` → `/private/tmp`) otherwise break `starts_with`.
fn canonical_or_raw(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Does this terminal's shell have any child processes (a command or job
/// still running)? An unknown child pid or a failed probe counts as busy —
/// never kill what can't be inspected.
fn shell_has_children(session: &PtySession) -> bool {
    let Some(pid) = session.child_pid else {
        return true;
    };
    !matches!(
        std::process::Command::new("pgrep")
            .arg("-P")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
        Ok(status) if !status.success()
    )
}

/// Canonical form of a user-typed link. Pasting a URL out of a browser is
/// the common case, but people also type `github.com/o/r/pull/7`, so a
/// scheme-less value gets https://. Anything else — another scheme, or no
/// host at all — is refused rather than stored: the TUI hands these to
/// `open(1)`, and only http(s) may ever reach it.
fn normalize_url(url: &str) -> Result<String> {
    let url = url.trim();
    if url.is_empty() {
        bail!("link URL is empty");
    }
    if url.contains(char::is_whitespace) {
        bail!("link URL contains whitespace");
    }
    let normalized = match url.split_once("://") {
        Some(("http" | "https", _)) => url.to_string(),
        Some((scheme, _)) => bail!("only http(s) links are supported (got {scheme}://)"),
        // Scheme-less: a bare host is a URL people type; a bare word is not.
        None => {
            let host = url.split(['/', '?', '#']).next().unwrap_or_default();
            if !host.contains('.') || host.starts_with('.') || host.ends_with('.') {
                bail!("not a URL: {url}");
            }
            format!("https://{url}")
        }
    };
    // Reject "https://" and friends: a scheme with nothing behind it.
    if normalized
        .split_once("://")
        .is_none_or(|(_, rest)| rest.is_empty())
    {
        bail!("not a URL: {url}");
    }
    Ok(normalized)
}

fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// Why a create was refused when the agent CLI isn't installed. One line —
/// the TUI shows it in the footer flash, which truncates. Unlike git (which
/// the daemon runs with its own inherited PATH), agent CLIs are spawned
/// through the user's login shell, so a fresh install is picked up on the
/// next try with no daemon restart.
fn cli_missing_message(kind: AgentKind) -> String {
    format!(
        "{} was not found on your PATH — install it, then try again.",
        kind.cli_program()
    )
}

/// Wrap `program args…` in a login + interactive shell (`$SHELL -l -i -c
/// 'exec …'`) so the child gets the user's real environment — ~/.zprofile
/// and ~/.zshrc on zsh — rather than the daemon's. `exec` keeps the child
/// as the PTY's direct process (exit codes and signals pass through).
fn login_shell_wrap(shell: &str, program: &str, args: &[String]) -> (String, Vec<String>) {
    let mut cmdline = String::from("exec");
    for part in std::iter::once(program).chain(args.iter().map(String::as_str)) {
        cmdline.push_str(" '");
        cmdline.push_str(&part.replace('\'', "'\\''"));
        cmdline.push('\'');
    }
    (
        shell.to_string(),
        vec!["-l".into(), "-i".into(), "-c".into(), cmdline],
    )
}

/// Env vars that must never leak into plain terminals (and are re-set
/// explicitly for agent PTYs).
pub fn scrubbed_env_names() -> Vec<String> {
    vec![
        "NEBULA_AGENT_ID".into(),
        "NEBULA_API_URL".into(),
        "NEBULA_API_TOKEN".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude argv: `args`, then nebula's appended worktree guidance.
    fn guided(args: &[&str]) -> Vec<String> {
        args.iter()
            .map(|s| s.to_string())
            .chain([
                "--append-system-prompt".to_string(),
                CLAUDE_WORKTREE_GUIDANCE.to_string(),
            ])
            .collect()
    }

    #[test]
    fn spawn_command_per_kind_resume_shapes() {
        // Fresh sessions: bare CLI (Claude plus its system-prompt guidance).
        assert_eq!(
            agent_spawn_command(AgentKind::Claude, None, None, None, None),
            ("claude".into(), guided(&[]), false)
        );
        // Codex/cursor always run in skip-permissions mode.
        assert_eq!(
            agent_spawn_command(AgentKind::Codex, None, None, None, None),
            ("codex".into(), vec!["--yolo".to_string()], false)
        );
        // Cursor's agent CLI is `cursor-agent`, not `cursor` (the editor).
        assert_eq!(
            agent_spawn_command(AgentKind::Cursor, None, None, None, None),
            ("cursor-agent".into(), vec!["--force".to_string()], false)
        );
        // Claude resumes with a flag; codex with a subcommand (order matters).
        assert_eq!(
            agent_spawn_command(AgentKind::Claude, Some("sid-1"), None, None, None),
            ("claude".into(), guided(&["--resume", "sid-1"]), true)
        );
        // Skip-permissions flags trail the resume args.
        assert_eq!(
            agent_spawn_command(AgentKind::Codex, Some("sid-2"), None, None, None),
            (
                "codex".into(),
                vec![
                    "resume".to_string(),
                    "sid-2".to_string(),
                    "--yolo".to_string()
                ],
                true
            )
        );
        assert_eq!(
            agent_spawn_command(AgentKind::Cursor, Some("sid-3"), None, None, None),
            (
                "cursor-agent".into(),
                vec![
                    "--resume".to_string(),
                    "sid-3".to_string(),
                    "--force".to_string()
                ],
                true
            )
        );
        // Override wins for both kinds and never gets resume args.
        assert_eq!(
            agent_spawn_command(
                AgentKind::Claude,
                Some("sid"),
                None,
                None,
                Some("/bin/sh -i")
            ),
            ("/bin/sh".into(), vec!["-i".to_string()], false)
        );
        assert_eq!(
            agent_spawn_command(AgentKind::Codex, Some("sid"), None, None, Some("/bin/sh")),
            ("/bin/sh".into(), vec![], false)
        );
    }

    #[test]
    fn spawn_command_model_and_effort_flags() {
        // Claude gets --model/--effort; either alone works.
        assert_eq!(
            agent_spawn_command(AgentKind::Claude, None, Some("opus"), Some("high"), None),
            (
                "claude".into(),
                guided(&["--model", "opus", "--effort", "high"]),
                false
            )
        );
        assert_eq!(
            agent_spawn_command(AgentKind::Claude, None, None, Some("max"), None),
            ("claude".into(), guided(&["--effort", "max"]), false)
        );
        // Codex takes --model plus a config override for effort, after --yolo.
        assert_eq!(
            agent_spawn_command(AgentKind::Codex, None, Some("gpt-5.5"), Some("high"), None),
            (
                "codex".into(),
                vec![
                    "--yolo".to_string(),
                    "--model".to_string(),
                    "gpt-5.5".to_string(),
                    "-c".to_string(),
                    "model_reasoning_effort=high".to_string()
                ],
                false
            )
        );
        // Resume keeps the model/effort flags (a fallback fresh spawn needs
        // them, and the CLIs accept them alongside resume).
        assert_eq!(
            agent_spawn_command(AgentKind::Claude, Some("sid"), Some("sonnet"), None, None),
            (
                "claude".into(),
                guided(&["--resume", "sid", "--model", "sonnet"]),
                true
            )
        );
        // Cursor has no model/effort knobs — choices are ignored.
        assert_eq!(
            agent_spawn_command(AgentKind::Cursor, None, Some("m"), Some("e"), None),
            ("cursor-agent".into(), vec!["--force".to_string()], false)
        );
        // Override still wins over everything.
        assert_eq!(
            agent_spawn_command(AgentKind::Claude, None, Some("opus"), None, Some("/bin/sh")),
            ("/bin/sh".into(), vec![], false)
        );
    }

    #[test]
    fn spawn_command_initial_prompt_is_claudes_positional_argument() {
        // The relocation notice trails everything, guidance included.
        let (_, args, resumed) = agent_spawn_command_with(
            AgentKind::Claude,
            Some("sid"),
            Some("opus"),
            None,
            None,
            Some("carry on"),
            LaunchFlags::interactive(),
        );
        assert!(resumed);
        let mut expected = guided(&["--resume", "sid", "--model", "opus"]);
        expected.push("carry on".into());
        assert_eq!(args, expected);
        // Codex and cursor take no such argument — their resumes stay plain.
        assert_eq!(
            agent_spawn_command_with(
                AgentKind::Codex,
                Some("sid"),
                None,
                None,
                None,
                Some("carry on"),
                LaunchFlags::interactive()
            )
            .1,
            vec!["resume", "sid", "--yolo"]
        );
        assert_eq!(
            agent_spawn_command_with(
                AgentKind::Cursor,
                None,
                None,
                None,
                None,
                Some("carry on"),
                LaunchFlags::interactive()
            )
            .1,
            vec!["--force"]
        );
        // An override is verbatim: no guidance, no prompt.
        assert_eq!(
            agent_spawn_command_with(
                AgentKind::Claude,
                None,
                None,
                None,
                Some("/bin/sh -i"),
                Some("carry on"),
                LaunchFlags::interactive()
            ),
            ("/bin/sh".into(), vec!["-i".to_string()], false)
        );
    }

    #[test]
    fn spawn_command_claude_cloud_passes_the_task_as_one_argument() {
        assert_eq!(
            claude_cloud_spawn_command(
                CloudLaunch::Create("Fix auth\nRun tests; don't stop"),
                Some("opus"),
                Some("high"),
                None,
            ),
            (
                "claude".into(),
                vec![
                    "--cloud=Fix auth\nRun tests; don't stop".to_string(),
                    "--model".to_string(),
                    "opus".to_string(),
                    "--effort".to_string(),
                    "high".to_string(),
                ],
                false,
            )
        );
        assert_eq!(
            claude_cloud_spawn_command(
                CloudLaunch::Create("--dangerously-skip-permissions"),
                None,
                None,
                None
            )
            .1,
            vec!["--cloud=--dangerously-skip-permissions"]
        );
    }

    #[test]
    fn spawn_command_cloud_attach_and_teleport_bind_the_id() {
        let id = "session_016SiQW5Lem2LbnUf1A3undt";
        assert_eq!(
            claude_cloud_spawn_command(CloudLaunch::Attach(id), None, None, None),
            ("claude".into(), vec![format!("--cloud={id}")], false)
        );
        assert_eq!(
            claude_cloud_spawn_command(CloudLaunch::Teleport(id), Some("opus"), None, None),
            (
                "claude".into(),
                vec![format!("--teleport={id}"), "--model".into(), "opus".into()],
                false
            )
        );
        // Overrides (tests) stay verbatim — no cloud flag at all.
        assert_eq!(
            claude_cloud_spawn_command(CloudLaunch::Attach(id), None, None, Some("/bin/true")).1,
            Vec::<String>::new()
        );
    }

    #[test]
    fn cloud_worktree_branch_is_short_and_git_safe() {
        assert_eq!(
            cloud_worktree_branch("session_016SiQW5Lem2LbnUf1A3undt"),
            "cloud-f1A3undt"
        );
        assert_eq!(cloud_worktree_branch("session_ab"), "cloud-ab");
        assert_eq!(cloud_worktree_branch("session_"), "cloud-");
    }

    #[tokio::test]
    async fn attach_cloud_agent_requires_a_cloud_session() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "w", "/tmp", true);
        seed_agent(&daemon, "local", "w", None);
        let err = daemon
            .attach_cloud_agent(&AgentId("local".into()))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not launched in Claude Cloud"),
            "{err}"
        );
    }

    #[test]
    fn login_shell_wrap_quotes_and_execs() {
        let (program, args) = login_shell_wrap(
            "/bin/zsh",
            "claude",
            &["--resume".to_string(), "sid-1".to_string()],
        );
        assert_eq!(program, "/bin/zsh");
        assert_eq!(
            args,
            vec!["-l", "-i", "-c", "exec 'claude' '--resume' 'sid-1'"]
        );
        // Single quotes in an arg survive the wrapping.
        let (_, args) = login_shell_wrap("/bin/zsh", "echo", &["it's".to_string()]);
        assert_eq!(args[3], r"exec 'echo' 'it'\''s'");
    }

    fn test_daemon() -> Arc<Daemon> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        Daemon::new(
            store,
            HookEnv {
                port: 0,
                token: String::new(),
            },
        )
    }
    fn task_spec(project: &str, prompt: &str) -> TaskSpec {
        TaskSpec {
            project: ProjectId(project.into()),
            name: "nightly".into(),
            prompt: prompt.into(),
            kind: AgentKind::Claude,
            model: None,
            effort: None,
            cron: None,
            iterations: 1,
            unattended: false,
            final_prompt: None,
            stall_timeout_secs: 0,
            commit_on_finish: false,
            target: TaskTarget::Root,
            enabled: true,
        }
    }

    fn only_task(daemon: &Daemon) -> Task {
        let tasks = daemon.store.load_tasks().unwrap();
        assert_eq!(tasks.len(), 1, "expected exactly one task");
        tasks.into_iter().next().unwrap()
    }

    /// Validation lives in the daemon so every path shares it, and so a typo
    /// comes back as an error rather than becoming a task that quietly never
    /// fires. Each rejection below would otherwise be silent.
    #[test]
    fn create_task_refuses_what_it_cannot_run() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);

        let bad_cron = TaskSpec {
            cron: Some("every tuesday-ish".into()),
            ..task_spec("p", "go")
        };
        assert!(daemon.create_task(bad_cron).is_err(), "unparseable cron");

        let no_prompt = TaskSpec {
            prompt: "   ".into(),
            ..task_spec("p", "go")
        };
        assert!(daemon.create_task(no_prompt).is_err(), "nothing to send");

        let no_iterations = TaskSpec {
            iterations: 0,
            ..task_spec("p", "go")
        };
        assert!(
            daemon.create_task(no_iterations).is_err(),
            "runs zero times"
        );

        let runaway = TaskSpec {
            iterations: MAX_TASK_ITERATIONS + 1,
            ..task_spec("p", "go")
        };
        assert!(
            daemon.create_task(runaway).is_err(),
            "iterations are the only stop condition, so the cap is the rail"
        );

        let no_project = task_spec("nope", "go");
        assert!(daemon.create_task(no_project).is_err(), "unknown project");

        let gone = TaskSpec {
            target: TaskTarget::Worktree(WorktreeId("nope".into())),
            ..task_spec("p", "go")
        };
        assert!(daemon.create_task(gone).is_err(), "unknown worktree");

        assert!(
            daemon.store.load_tasks().unwrap().is_empty(),
            "a refused create stores nothing"
        );

        // And the happy path, including a 5-field crontab line and a name
        // that needs trimming.
        let ok = TaskSpec {
            name: "  nightly review  ".into(),
            cron: Some("0 2 * * *".into()),
            iterations: 3,
            ..task_spec("p", "  /code-review  ")
        };
        daemon.create_task(ok).unwrap();
        let task = only_task(&daemon);
        assert_eq!(task.name, "nightly review");
        assert_eq!(task.prompt, "/code-review");
        assert!(task.next_run_at > epoch_ms(), "a cron task is scheduled");
    }

    /// An empty cron string is how the TUI clears a schedule, so it has to
    /// mean "manual only" rather than "parse error".
    #[test]
    fn a_blank_cron_means_manual_not_broken() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            cron: Some("   ".into()),
            ..task_spec("p", "go")
        };
        daemon.create_task(spec).unwrap();
        let task = only_task(&daemon);
        assert_eq!(task.cron, None);
        assert_eq!(task.next_run_at, 0, "manual tasks are never due");
    }

    /// Disabling has to clear the due stamp, not just the flag: the tick
    /// checks `is_due` and a stale stamp on a re-enabled task would fire
    /// immediately instead of at the next window.
    #[test]
    fn enabling_and_disabling_moves_the_due_stamp() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            cron: Some("0 2 * * *".into()),
            ..task_spec("p", "go")
        };
        let EntityId::Task(id) = daemon.create_task(spec).unwrap() else {
            panic!("create_task returned a non-task id");
        };
        assert!(only_task(&daemon).next_run_at > 0);

        daemon.set_task_enabled(&id, false).unwrap();
        let task = only_task(&daemon);
        assert!(!task.enabled);
        assert_eq!(task.next_run_at, 0, "a disabled task is not scheduled");

        daemon.set_task_enabled(&id, true).unwrap();
        let task = only_task(&daemon);
        assert!(task.enabled);
        assert!(
            task.next_run_at > epoch_ms(),
            "re-enabling schedules the next window, not an instant run"
        );
    }

    /// An edit must not throw away what the last run did — that is the only
    /// record of it — and must re-derive the schedule from the new cron.
    #[test]
    fn updating_a_task_keeps_its_run_history() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let EntityId::Task(id) = daemon.create_task(task_spec("p", "go")).unwrap() else {
            panic!("create_task returned a non-task id");
        };
        let agent = AgentId("a1".into());
        daemon
            .store
            .set_task_run_state(&id, 4242, "ran 1 of 1", Some(&agent))
            .unwrap();

        let edited = TaskSpec {
            prompt: "go again".into(),
            cron: Some("*/5 * * * *".into()),
            iterations: 4,
            unattended: true,
            ..task_spec("p", "unused")
        };
        daemon.update_task(&id, edited).unwrap();
        let task = only_task(&daemon);
        assert_eq!(task.prompt, "go again");
        assert_eq!(task.iterations, 4);
        assert!(task.unattended);
        assert_eq!(task.last_run_at, 4242);
        assert_eq!(task.last_outcome.as_deref(), Some("ran 1 of 1"));
        assert_eq!(task.last_agent_id, Some(agent));
        assert!(task.next_run_at > 0, "the new cron is scheduled");

        // A bad edit changes nothing at all.
        let broken = TaskSpec {
            cron: Some("nope".into()),
            ..task_spec("p", "go")
        };
        assert!(daemon.update_task(&id, broken).is_err());
        assert_eq!(only_task(&daemon).prompt, "go again");
    }

    /// The tick must re-stamp before it runs. A run that fails would
    /// otherwise leave the task due forever and re-fire on every sweep —
    /// a missing agent CLI would spawn a session attempt every 30 seconds.
    #[tokio::test]
    async fn a_due_task_is_restamped_even_when_its_run_fails() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        // No worktrees seeded, so resolving Root fails and the run errors.
        let spec = TaskSpec {
            cron: Some("*/1 * * * *".into()),
            ..task_spec("p", "go")
        };
        let EntityId::Task(id) = daemon.create_task(spec).unwrap() else {
            panic!("create_task returned a non-task id");
        };
        // Force the window open.
        daemon.store.set_task_next_run(&id, 1).unwrap();
        assert!(crate::schedule::is_due(1, epoch_ms()));

        daemon.tick_scheduler().await;

        let task = only_task(&daemon);
        assert!(
            task.next_run_at > epoch_ms(),
            "the window moved on despite the failure"
        );
        assert!(
            task.last_outcome
                .as_deref()
                .unwrap_or("")
                .starts_with("failed:"),
            "the failure is the task's outcome, got {:?}",
            task.last_outcome
        );
        assert_eq!(task.last_agent_id, None, "no session was ever created");
    }

    /// Disabled and manual tasks are invisible to the sweep however their
    /// stamps read.
    #[tokio::test]
    async fn the_sweep_skips_disabled_and_manual_tasks() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        // Manual (no cron) but with a due stamp forced on, and a disabled
        // task with one too: neither may run.
        let EntityId::Task(manual) = daemon.create_task(task_spec("p", "go")).unwrap() else {
            panic!("non-task id");
        };
        let disabled_spec = TaskSpec {
            cron: Some("*/1 * * * *".into()),
            enabled: false,
            ..task_spec("p", "go")
        };
        let EntityId::Task(disabled) = daemon.create_task(disabled_spec).unwrap() else {
            panic!("non-task id");
        };
        daemon.store.set_task_next_run(&manual, 1).unwrap();
        daemon.store.set_task_next_run(&disabled, 1).unwrap();

        daemon.tick_scheduler().await;

        for id in [&manual, &disabled] {
            let task = daemon.store.get_task(id).unwrap().unwrap();
            assert_eq!(task.last_run_at, 0, "{id} should not have run");
            assert_eq!(task.last_outcome, None);
        }
    }

    /// The loop's bookkeeping, exercised without a PTY. `delivered` is only
    /// advanced by a delivery that actually wrote bytes, so this walks the
    /// counter by hand and checks the turn-end gate and the retire path.
    #[tokio::test]
    async fn a_task_loop_retires_when_its_iterations_run_out() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 2,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id: task_id.clone(),
                delivered: 2,
                total: 2,
                in_flight: false,
                last_progress_at: epoch_ms(),
            },
        );

        // Anything that isn't a turn end leaves the loop alone.
        daemon.continue_task_loop(&agent, &HookEvent::UserPromptSubmit);
        assert_eq!(daemon.task_loop_progress(&agent), Some((2, 2)));
        daemon.continue_task_loop(
            &agent,
            &HookEvent::Notification {
                notification_type: Some("permission_prompt".into()),
            },
        );
        assert_eq!(
            daemon.task_loop_progress(&agent),
            Some((2, 2)),
            "a permission prompt is not a turn end, so nothing is re-prompted"
        );

        // The turn end with nothing left retires the run and records it.
        daemon.continue_task_loop(&agent, &HookEvent::Stop);
        assert_eq!(daemon.task_loop_progress(&agent), None, "loop is done");
        let task = only_task(&daemon);
        assert_eq!(task.last_outcome.as_deref(), Some("ran 2 of 2"));
        assert_eq!(task.last_agent_id, Some(agent.clone()));

        // A turn end for an agent with no loop is a no-op, not a panic.
        daemon.continue_task_loop(&AgentId("nobody".into()), &HookEvent::Stop);
    }

    /// The failure this whole feature exists to avoid: a run whose session
    /// dies leaves the loop entry behind, and the task reads "running"
    /// forever while nothing at all is happening.
    #[tokio::test]
    async fn a_dead_session_ends_the_run() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 5,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id,
                delivered: 2,
                total: 5,
                in_flight: false,
                last_progress_at: epoch_ms(),
            },
        );

        daemon.abandon_task_run_on_exit(&agent, Some(1));
        assert_eq!(daemon.task_loop_progress(&agent), None, "loop is retired");
        let task = only_task(&daemon);
        assert_eq!(
            task.last_outcome.as_deref(),
            Some("stopped: session exited (1) at iteration 2 of 5")
        );

        // A PTY that went away without a code says so without inventing one,
        // and an agent with no run at all is a no-op rather than a panic.
        daemon.abandon_task_run_on_exit(&AgentId("nobody".into()), None);
    }

    /// `--dangerously-skip-permissions` does not cover `AskUserQuestion`: the
    /// CLI parks on the dialog, fires no turn end, and an unattended loop
    /// would wait for a person who is asleep.
    #[tokio::test]
    async fn an_unattended_run_that_asks_a_question_ends() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 5,
            unattended: true,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id,
                delivered: 3,
                total: 5,
                in_flight: false,
                last_progress_at: epoch_ms(),
            },
        );

        daemon.continue_task_loop(
            &agent,
            &HookEvent::PreToolUse {
                tool_name: Some("AskUserQuestion".into()),
            },
        );
        assert_eq!(daemon.task_loop_progress(&agent), None);
        assert_eq!(
            only_task(&daemon).last_outcome.as_deref(),
            Some("stopped: asked for input at iteration 3 of 5")
        );
    }

    /// The same question from an *attended* task is the system working: the
    /// user is watching the pane and can answer it, so the run stays alive.
    #[test]
    fn an_attended_run_may_ask_a_question_and_live() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 5,
            unattended: false,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id,
                delivered: 3,
                total: 5,
                in_flight: false,
                last_progress_at: epoch_ms(),
            },
        );
        daemon.continue_task_loop(
            &agent,
            &HookEvent::PreToolUse {
                tool_name: Some("AskUserQuestion".into()),
            },
        );
        assert_eq!(daemon.task_loop_progress(&agent), Some((3, 5)));
    }

    /// A turn that never ends is the other silent stall: nothing crashed, so
    /// there is no exit to react to. Only the clock can tell.
    #[tokio::test]
    async fn a_stalled_run_is_written_off() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 5,
            stall_timeout_secs: 300,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        let insert = |since: i64| {
            daemon.task_loops.lock().unwrap().insert(
                agent.clone(),
                LoopState {
                    run_id: TaskRunId("run-test".into()),
                    task_id: task_id.clone(),
                    delivered: 2,
                    total: 5,
                    in_flight: false,
                    last_progress_at: since,
                },
            );
        };

        // Four minutes into a five-minute window: still working.
        insert(epoch_ms() - 4 * 60 * 1_000);
        daemon.sweep_stalled_runs();
        assert_eq!(daemon.task_loop_progress(&agent), Some((2, 5)));

        // Past it, and the run is written off with the window in the reason.
        insert(epoch_ms() - 6 * 60 * 1_000);
        daemon.sweep_stalled_runs();
        assert_eq!(daemon.task_loop_progress(&agent), None);
        assert_eq!(
            only_task(&daemon).last_outcome.as_deref(),
            Some("stalled: no turn ended in 5m at iteration 2 of 5")
        );
    }

    /// A task row for the pure-function tests below, with no store behind it.
    fn sample_task() -> Task {
        Task {
            id: TaskId("t".into()),
            project_id: ProjectId("p".into()),
            name: "nightly".into(),
            prompt: "keep going".into(),
            kind: AgentKind::Claude,
            model: None,
            effort: None,
            cron: None,
            iterations: 1,
            unattended: false,
            final_prompt: None,
            stall_timeout_secs: 0,
            commit_on_finish: false,
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

    /// A finished run has to leave something behind that outlives it: the
    /// row keeps the numbers, the directory keeps the write-up. Without both,
    /// "what happened last night" is a single line that the next run
    /// overwrites.
    #[tokio::test]
    async fn a_finished_run_records_what_it_did() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 2,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let task = daemon.store.get_task(&task_id).unwrap().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let run = TaskRun {
            dir: tmp.path().join("run"),
            // No checkout on disk: the git half is skipped and the report
            // still has to be written.
            ..new_run_record(&task, epoch_ms())
        };
        daemon.store.insert_task_run(&run).unwrap();
        let agent = AgentId("a1".into());

        daemon
            .finish_run_record(
                run.id.clone(),
                2,
                &agent,
                &task,
                "ran 2 of 2".to_string(),
                TaskRunStatus::Completed,
            )
            .await;

        let read = daemon.store.get_task_run(&run.id).unwrap().unwrap();
        assert_eq!(read.status, TaskRunStatus::Completed);
        assert_eq!(read.outcome, "ran 2 of 2");
        assert_eq!(read.iterations_done, 2);
        assert!(read.ended_at > 0, "the run is over and says so");
        assert_eq!(read.agent_id.as_ref(), Some(&agent));

        let report = std::fs::read_to_string(read.report_path()).expect("a report on disk");
        assert!(report.contains("ran 2 of 2"), "{report}");
        assert!(report.contains("nightly"), "{report}");
        assert_eq!(
            only_task(&daemon).last_outcome.as_deref(),
            Some("ran 2 of 2"),
            "nothing to add to the line when nothing was diffed"
        );
    }

    /// Every stop path calls `end_task_run`, and two of them can fire for the
    /// same run (a stall sweep while the session is dying). The second must
    /// not overwrite the first's account of it.
    #[tokio::test]
    async fn only_the_first_stop_path_ends_a_run() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let EntityId::Task(task_id) = daemon.create_task(task_spec("p", "go")).unwrap() else {
            panic!("non-task id");
        };
        let task = daemon.store.get_task(&task_id).unwrap().unwrap();
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id: task_id.clone(),
                delivered: 1,
                total: 1,
                in_flight: false,
                last_progress_at: epoch_ms(),
            },
        );

        daemon.end_task_run(&agent, &task, "ran 1 of 1".into(), TaskRunStatus::Completed);
        daemon.end_task_run(
            &agent,
            &task,
            "stopped: session exited (1) at iteration 1 of 1".into(),
            TaskRunStatus::Stopped,
        );
        assert_eq!(
            only_task(&daemon).last_outcome.as_deref(),
            Some("ran 1 of 1"),
            "the late stop path found the loop already gone"
        );
    }

    /// A daemon that dies mid-run leaves a row saying "running" that nothing
    /// will ever close — and a task whose newest outcome is a lie.
    #[tokio::test]
    async fn a_run_the_daemon_outlived_is_closed_at_boot() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 3,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let task = daemon.store.get_task(&task_id).unwrap().unwrap();
        let started = epoch_ms();
        let mut run = new_run_record(&task, started);
        run.iterations_done = 2;
        daemon.store.insert_task_run(&run).unwrap();
        daemon
            .store
            .set_task_run_state(&task_id, started, "running", None)
            .unwrap();

        daemon.reconcile_unfinished_runs();

        let read = daemon.store.get_task_run(&run.id).unwrap().unwrap();
        assert_eq!(read.status, TaskRunStatus::Stopped);
        assert!(
            read.outcome.contains("daemon restarted"),
            "{}",
            read.outcome
        );
        assert!(read.ended_at > 0);
        assert_eq!(
            only_task(&daemon).last_outcome.as_deref(),
            Some(read.outcome.as_str()),
            "the task row stops claiming the run is still going"
        );
        assert!(
            daemon.store.unfinished_task_runs().unwrap().is_empty(),
            "and it is not swept twice"
        );
    }

    /// The run directory is the thing somebody finds in a file browser six
    /// weeks later, so it has to say which task and which night on sight.
    #[test]
    fn a_run_directory_names_its_task_and_its_night() {
        let task = Task {
            name: "Nightly Notes!".into(),
            ..sample_task()
        };
        let a = run_dir(
            &task,
            1_700_000_000_000,
            &TaskRunId("01ABCDEFGHIJKLMNOP".into()),
        );
        let b = run_dir(
            &task,
            1_700_000_000_000,
            &TaskRunId("01ABCDEFGHIJKLMNZZ".into()),
        );
        let a = a.to_string_lossy().to_string();
        assert!(a.contains("nightly-notes"), "{a}");
        assert!(a.contains("klmnop"), "the id's tail keeps runs apart: {a}");
        assert_ne!(a, b.to_string_lossy(), "same second, different runs");
    }

    /// Only the last turn is asked for a summary, and it is asked alongside
    /// the wrap-up rather than instead of it.
    #[test]
    fn the_last_turn_is_asked_to_write_the_run_up() {
        let path = std::path::PathBuf::from("/runs/x/summary.md");
        let task = Task {
            iterations: 3,
            final_prompt: Some("stop and land it".into()),
            ..sample_task()
        };
        let first = task_prompt_text(&task, 1, Some(&path));
        assert!(
            !first.contains("summary.md"),
            "a summary asked for on turn 1 would describe a third of a run: {first}"
        );
        let last = task_prompt_text(&task, 3, Some(&path));
        assert!(last.contains("stop and land it"), "{last}");
        assert!(last.contains("/runs/x/summary.md"), "{last}");

        // A one-turn task has no turn to spare, and still gets asked.
        let single = Task {
            iterations: 1,
            final_prompt: None,
            ..sample_task()
        };
        assert!(task_prompt_text(&single, 1, Some(&path)).contains("summary.md"));
        // And a run with nowhere to write it is not asked at all.
        assert!(!task_prompt_text(&single, 1, None).contains("summary.md"));
    }

    /// A transcript is megabytes; what crosses the socket is its end, and it
    /// says that it is only the end.
    #[test]
    fn a_long_artifact_is_sent_as_its_tail() {
        let short = tail_text(b"all of it", 64);
        assert_eq!(short, "all of it");

        let raw: Vec<u8> = std::iter::repeat_n(b'x', 100).chain(*b"THE END").collect();
        let tail = tail_text(&raw, 8);
        assert!(tail.contains("THE END"), "{tail}");
        assert!(tail.contains("earlier bytes not shown"), "{tail}");
    }

    /// The overnight failure that actually happened when this was first run
    /// for real: a checkout Claude Code had not seen before opened with its
    /// trust dialog, which swallowed the pasted prompt. No hook ever fired,
    /// so the session stayed `Fresh` and the run sat there. A first turn that
    /// has not started is a different failure from a turn taking a long time,
    /// and it is caught on a much shorter fuse — and named.
    #[tokio::test]
    async fn a_run_whose_first_turn_never_starts_is_caught_early_and_explained() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "w", "/tmp/p", true);
        seed_agent(&daemon, "a1", "w", None);
        let spec = TaskSpec {
            iterations: 3,
            stall_timeout_secs: 1_800,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        let park = |since: i64| {
            daemon.task_loops.lock().unwrap().insert(
                agent.clone(),
                LoopState {
                    run_id: TaskRunId("run-test".into()),
                    task_id: task_id.clone(),
                    delivered: 1,
                    total: 3,
                    in_flight: false,
                    last_progress_at: since,
                },
            );
        };

        // `seed_agent` makes a Running one: a turn genuinely in progress gets
        // the task's own generous window, not the short one.
        park(epoch_ms() - 5 * 60 * 1_000);
        daemon.sweep_stalled_runs();
        assert_eq!(
            daemon.task_loop_progress(&agent),
            Some((1, 3)),
            "five minutes into a running turn is not a stall"
        );

        // Same five minutes, but the session never left Fresh.
        daemon
            .store
            .set_agent_status(&agent, AgentStatus::Fresh)
            .unwrap();
        park(epoch_ms() - 5 * 60 * 1_000);
        daemon.sweep_stalled_runs();
        assert_eq!(daemon.task_loop_progress(&agent), None);
        let outcome = only_task(&daemon).last_outcome.unwrap();
        assert!(
            outcome.starts_with("stalled: no turn started in 2m"),
            "caught on the short fuse: {outcome}"
        );
        assert!(
            outcome.contains("trust prompt"),
            "and it names the likely cause: {outcome}"
        );
    }

    /// 0 means "wait forever" — the one setting that lets an overnight run
    /// hang, so it has to be exactly what it says.
    #[test]
    fn a_zero_watchdog_never_gives_up() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 5,
            stall_timeout_secs: 0,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id,
                delivered: 1,
                total: 5,
                in_flight: false,
                // A week without a turn.
                last_progress_at: epoch_ms() - 7 * 24 * 3_600 * 1_000,
            },
        );
        daemon.sweep_stalled_runs();
        assert_eq!(daemon.task_loop_progress(&agent), Some((1, 5)));
    }

    /// Two agents in one checkout is worse than a missed window, so the
    /// window is what gets dropped — and the stamp still moves, or the task
    /// would re-fire on every tick forever.
    #[tokio::test]
    async fn the_sweep_skips_a_task_whose_run_is_still_going() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            cron: Some("* * * * * *".into()),
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        // Make it due, then park a run on it.
        daemon.store.set_task_next_run(&task_id, 1).unwrap();
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id: task_id.clone(),
                delivered: 1,
                total: 5,
                in_flight: false,
                last_progress_at: epoch_ms(),
            },
        );

        daemon.tick_scheduler().await;

        let task = only_task(&daemon);
        assert_eq!(
            task.last_outcome.as_deref(),
            Some("skipped: previous run still going")
        );
        assert!(task.next_run_at > 1, "the due stamp still moved forward");
        assert_eq!(
            daemon.task_loop_progress(&agent),
            Some((1, 5)),
            "the run in flight is untouched"
        );
        assert!(
            daemon.store.load_tree().unwrap().2.is_empty(),
            "no second session was spawned"
        );
    }

    /// The wrap-up replaces the prompt on the final turn only, and only when
    /// there is a turn to spare.
    #[test]
    fn the_last_iteration_of_a_loop_gets_the_wrap_up_prompt() {
        let mut task = Task {
            id: TaskId("t".into()),
            project_id: ProjectId("p".into()),
            name: "nightly".into(),
            prompt: "keep going".into(),
            kind: AgentKind::Claude,
            model: None,
            effort: None,
            cron: None,
            iterations: 3,
            unattended: false,
            final_prompt: Some("stop and summarise".into()),
            stall_timeout_secs: 0,
            commit_on_finish: false,
            target: TaskTarget::Root,
            enabled: true,
            last_run_at: 0,
            next_run_at: 0,
            last_outcome: None,
            last_agent_id: None,
            created_at: 0,
            sort_order: 0,
        };
        assert_eq!(
            task_prompt_text(&task, 1, None),
            "[nebula] Task `nightly` — iteration 1 of 3.\nkeep going"
        );
        assert_eq!(
            task_prompt_text(&task, 3, None),
            "[nebula] Task `nightly` — iteration 3 of 3 (wrap-up).\nstop and summarise"
        );

        // No wrap-up set: the last turn is an ordinary turn.
        task.final_prompt = None;
        assert_eq!(
            task_prompt_text(&task, 3, None),
            "[nebula] Task `nightly` — iteration 3 of 3.\nkeep going"
        );

        // One turn total is all work and no wrap-up — spending the only turn
        // summarising would mean the task never does anything.
        task.final_prompt = Some("stop and summarise".into());
        task.iterations = 1;
        assert_eq!(
            task_prompt_text(&task, 1, None),
            "[nebula] Task `nightly`.\nkeep going"
        );
    }

    #[test]
    fn a_watchdog_window_reads_as_a_duration() {
        assert_eq!(mins_label(30), "30s");
        assert_eq!(mins_label(300), "5m");
        assert_eq!(mins_label(1_800), "30m");
        assert_eq!(mins_label(3_600), "1h");
        assert_eq!(mins_label(7_200), "2h");
    }

    /// A turn end that lands while a prompt is still being typed belongs to
    /// the previous turn — or is a duplicate Stop. Acting on it would consume
    /// an iteration for work that never happened, and in practice re-typed
    /// the same iteration twice: the e2e caught exactly that.
    // A tokio test because the second half actually schedules a delivery,
    // and `deliver_task_prompt` spawns onto the runtime.
    #[tokio::test]
    async fn a_turn_end_during_a_delivery_is_ignored() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 5,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id,
                delivered: 1,
                total: 5,
                in_flight: true,
                last_progress_at: epoch_ms(),
            },
        );

        daemon.continue_task_loop(&agent, &HookEvent::Stop);
        assert_eq!(
            daemon.task_loop_progress(&agent),
            Some((1, 5)),
            "the in-flight delivery keeps its iteration"
        );

        // Once the delivery lands, the next turn end advances exactly one.
        daemon
            .task_loops
            .lock()
            .unwrap()
            .get_mut(&agent)
            .unwrap()
            .in_flight = false;
        daemon.continue_task_loop(&agent, &HookEvent::Stop);
        assert_eq!(daemon.task_loop_progress(&agent), Some((2, 5)));
    }

    /// Deleting a task stops its loop without touching the session — killing
    /// it would throw away a turn's work.
    #[test]
    fn deleting_a_task_stops_its_loop_but_leaves_the_session() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        let spec = TaskSpec {
            iterations: 5,
            ..task_spec("p", "go")
        };
        let EntityId::Task(task_id) = daemon.create_task(spec).unwrap() else {
            panic!("non-task id");
        };
        let agent = AgentId("a1".into());
        daemon.task_loops.lock().unwrap().insert(
            agent.clone(),
            LoopState {
                run_id: TaskRunId("run-test".into()),
                task_id: task_id.clone(),
                delivered: 1,
                total: 5,
                in_flight: false,
                last_progress_at: epoch_ms(),
            },
        );
        daemon.delete_task(&task_id).unwrap();
        assert_eq!(daemon.task_loop_progress(&agent), None);
        assert!(daemon.store.get_task(&task_id).unwrap().is_none());
        // The turn end that follows finds nothing to do.
        daemon.continue_task_loop(&agent, &HookEvent::Stop);
    }

    /// Claude is the only kind that doesn't already skip permission prompts,
    /// so `unattended` is the one flag that changes its argv — and it must
    /// change nothing when off.
    #[test]
    fn unattended_adds_claudes_skip_permissions_flag() {
        let unattended = LaunchFlags {
            guidance: false,
            unattended: true,
        };
        let (_, args, _) =
            agent_spawn_command_with(AgentKind::Claude, None, None, None, None, None, unattended);
        assert_eq!(args, vec!["--dangerously-skip-permissions"]);

        // Off by default: an interactive session is untouched.
        let (_, args, _) = agent_spawn_command_with(
            AgentKind::Claude,
            None,
            None,
            None,
            None,
            None,
            LaunchFlags::default(),
        );
        assert!(
            args.is_empty(),
            "a normal claude launch gains nothing, got {args:?}"
        );

        // It lands after the resume args, where codex's --yolo already sits.
        let (_, args, resumed) = agent_spawn_command_with(
            AgentKind::Claude,
            Some("sid"),
            None,
            None,
            None,
            None,
            unattended,
        );
        assert!(resumed);
        assert_eq!(
            args,
            vec!["--resume", "sid", "--dangerously-skip-permissions"]
        );

        // Codex and cursor already skip; the flag must not double up.
        for kind in [AgentKind::Codex, AgentKind::Cursor] {
            let (_, args, _) =
                agent_spawn_command_with(kind, None, None, None, None, None, unattended);
            assert!(
                !args.iter().any(|a| a.contains("dangerously")),
                "{kind:?} should keep its own flag, got {args:?}"
            );
        }
    }

    /// A `NewWorktree` task returns to one branch per task rather than
    /// leaving a checkout behind on every run.
    #[test]
    fn a_new_worktree_task_names_its_branch_after_itself() {
        let mk = |name: &str| Task {
            id: TaskId("01ABC".into()),
            project_id: ProjectId("p".into()),
            name: name.into(),
            prompt: "go".into(),
            kind: AgentKind::Claude,
            model: None,
            effort: None,
            cron: None,
            iterations: 1,
            unattended: false,
            final_prompt: None,
            stall_timeout_secs: 0,
            commit_on_finish: false,
            target: TaskTarget::NewWorktree,
            enabled: true,
            last_run_at: 0,
            next_run_at: 0,
            last_outcome: None,
            last_agent_id: None,
            created_at: 0,
            sort_order: 0,
        };
        assert_eq!(
            task_run_branch(&mk("Nightly Review")),
            "task-nightly-review"
        );
        assert_eq!(
            task_run_branch(&mk("fix flaky tests")),
            "task-fix-flaky-tests"
        );
        // Punctuation only: the id keeps the branch legal and traceable.
        assert_eq!(task_run_branch(&mk("!!!")), "task-01abc");
        // Stable across calls, which is what makes the checkout reusable.
        assert_eq!(task_run_branch(&mk("x")), task_run_branch(&mk("x")));
    }

    #[tokio::test]
    async fn cloud_create_validates_tasks_and_rejects_non_claude_kinds() {
        let daemon = test_daemon();
        let worktree = WorktreeId("unused".into());

        let empty = daemon
            .create_agent(CreateAgentSpec {
                worktree: worktree.clone(),
                name: "cloud".into(),
                kind: AgentKind::Claude,
                model: None,
                effort: None,
                auto_title: false,
                cloud_prompt: Some(" \n ".into()),
            })
            .await
            .unwrap_err();
        assert!(empty.to_string().contains("needs a task"));

        let nul = daemon
            .create_agent(CreateAgentSpec {
                worktree: worktree.clone(),
                name: "cloud".into(),
                kind: AgentKind::Claude,
                model: None,
                effort: None,
                auto_title: false,
                cloud_prompt: Some("fix\0auth".into()),
            })
            .await
            .unwrap_err();
        assert!(nul.to_string().contains("NUL"));

        let too_long = daemon
            .create_agent(CreateAgentSpec {
                worktree: worktree.clone(),
                name: "cloud".into(),
                kind: AgentKind::Claude,
                model: None,
                effort: None,
                auto_title: false,
                cloud_prompt: Some("x".repeat(MAX_CLOUD_PROMPT_BYTES + 1)),
            })
            .await
            .unwrap_err();
        assert!(too_long.to_string().contains("too long"));

        let wrong_kind = daemon
            .create_agent(CreateAgentSpec {
                worktree,
                name: "cloud".into(),
                kind: AgentKind::Codex,
                model: None,
                effort: None,
                auto_title: false,
                cloud_prompt: Some("Fix auth".into()),
            })
            .await
            .unwrap_err();
        assert!(wrong_kind.to_string().contains("only supported for Claude"));
    }

    #[test]
    fn failed_agent_spawn_rolls_back_the_persisted_row() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "w", "/tmp", true);
        seed_agent(&daemon, "cloud", "w", None);
        let id = AgentId("cloud".into());

        let error = daemon
            .rollback_agent_on_spawn_error(&id, Err::<(), _>(anyhow::anyhow!("spawn failed")))
            .unwrap_err();

        assert!(error.to_string().contains("spawn failed"));
        assert!(daemon.store.get_agent(&id).unwrap().is_none());
    }

    fn seed_projects(daemon: &Daemon, names: &[&str]) {
        for (i, name) in names.iter().enumerate() {
            daemon
                .store
                .insert_project(&Project {
                    workspace_id: Default::default(),
                    id: ProjectId((*name).into()),
                    name: (*name).into(),
                    repo_path: format!("/tmp/{name}").into(),
                    sort_order: i as i64,
                })
                .unwrap();
        }
    }

    /// Project names in display order.
    fn names(daemon: &Daemon) -> Vec<String> {
        let (projects, _, _, _) = daemon.store.load_tree().unwrap();
        projects.into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn move_project_reorders_and_normalizes_sort_orders() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["a", "b", "c", "d"]);

        daemon.move_project(&ProjectId("d".into()), -2).unwrap();
        assert_eq!(names(&daemon), ["a", "d", "b", "c"]);
        let (projects, _, _, _) = daemon.store.load_tree().unwrap();
        assert_eq!(
            projects.iter().map(|p| p.sort_order).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );

        // Edge moves clamp to no-ops.
        daemon.move_project(&ProjectId("a".into()), -1).unwrap();
        daemon.move_project(&ProjectId("c".into()), 5).unwrap();
        assert_eq!(names(&daemon), ["a", "d", "b", "c"]);
    }

    fn seed_worktree(daemon: &Daemon, project: &str, id: &str, path: &str, is_main: bool) {
        daemon
            .store
            .insert_worktree(&Worktree {
                id: WorktreeId(id.into()),
                project_id: ProjectId(project.into()),
                path: path.into(),
                branch: id.into(),
                is_main,
                pinned: false,
                sort_order: 0,
            })
            .unwrap();
    }

    fn seed_agent(daemon: &Daemon, id: &str, worktree: &str, session_id: Option<&str>) {
        daemon
            .store
            .insert_agent(&Agent {
                id: AgentId(id.into()),
                worktree_id: WorktreeId(worktree.into()),
                name: id.into(),
                status: AgentStatus::Running,
                archived: false,
                archived_at: 0,
                pinned: false,
                unseen: false,
                kind: AgentKind::Claude,
                model: None,
                effort: None,
                session_id: session_id.map(str::to_string),
                cloud_session_id: None,
                sort_order: 0,
                status_changed_at: 0,
                alive: false,
            })
            .unwrap();
    }

    fn agent_worktree(daemon: &Daemon, id: &str) -> String {
        daemon
            .store
            .get_agent(&AgentId(id.into()))
            .unwrap()
            .unwrap()
            .worktree_id
            .to_string()
    }

    #[test]
    fn move_agent_rehomes_row_and_broadcasts() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_worktree(&daemon, "p", "feat", "/nebula-test/p-feat", false);
        seed_agent(&daemon, "a1", "root", None);
        let mut rx = daemon.events.subscribe();

        daemon
            .move_agent(&AgentId("a1".into()), &WorktreeId("feat".into()))
            .unwrap();
        assert_eq!(agent_worktree(&daemon, "a1"), "feat");
        match rx.try_recv().unwrap() {
            ServerEvent::EntityUpserted {
                entity: Entity::Agent(a),
            } => assert_eq!(a.worktree_id.to_string(), "feat"),
            other => panic!("expected agent upsert, got {other:?}"),
        }

        // Moving to the worktree it already lives in is a silent no-op.
        daemon
            .move_agent(&AgentId("a1".into()), &WorktreeId("feat".into()))
            .unwrap();
        assert!(rx.try_recv().is_err(), "no broadcast for a no-op move");
    }

    #[tokio::test]
    async fn enter_worktree_takes_an_existing_branch_and_moves_the_row_now() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_worktree(&daemon, "p", "feat", "/nebula-test/p-feat", false);
        seed_agent(&daemon, "a1", "root", Some("s1"));
        let a1 = AgentId("a1".into());
        let mut rx = daemon.events.subscribe();

        let (target, outcome) = daemon.enter_worktree(&a1, "feat", None).await.unwrap();
        assert_eq!(target.id.to_string(), "feat");
        // No PTY runs here, so nothing waits on a turn end.
        assert_eq!(outcome, EnterOutcome::NextLaunch);
        assert!(!daemon.relocation_pending(&a1));
        assert_eq!(agent_worktree(&daemon, "a1"), "feat");
        match rx.try_recv().unwrap() {
            ServerEvent::EntityUpserted {
                entity: Entity::Agent(a),
            } => assert_eq!(a.worktree_id.to_string(), "feat"),
            other => panic!("expected agent upsert, got {other:?}"),
        }

        // Already there: a settled answer, no broadcast.
        let (again, outcome) = daemon.enter_worktree(&a1, "feat", None).await.unwrap();
        assert_eq!(again.id, target.id);
        assert_eq!(outcome, EnterOutcome::AlreadyThere);
        assert!(rx.try_recv().is_err(), "no broadcast for a no-op enter");

        // Blank names are refused before anything is touched.
        assert!(daemon.enter_worktree(&a1, "  ", None).await.is_err());
    }

    /// Between `nebula worktree` and the turn's Stop the row already sits
    /// under the target while the process still reports the old checkout:
    /// that cwd must not drag it back, and only a turn-end hook drains the
    /// pending relocation.
    #[test]
    fn pending_relocation_ignores_the_old_cwd_until_the_turn_ends() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_worktree(&daemon, "p", "feat", "/nebula-test/p-feat", false);
        seed_agent(&daemon, "a1", "feat", Some("s1"));
        let a1 = AgentId("a1".into());
        let feat = daemon
            .store
            .get_worktree(&WorktreeId("feat".into()))
            .unwrap()
            .unwrap();
        daemon
            .pending_moves
            .lock()
            .unwrap()
            .insert(a1.clone(), feat);

        daemon.reparent_agent_by_cwd(&a1, "/nebula-test/p", Some("s1"), false);
        assert_eq!(
            agent_worktree(&daemon, "a1"),
            "feat",
            "the old checkout's cwd is ignored mid-relocation"
        );

        daemon.complete_pending_move(
            &a1,
            &HookEvent::PostToolUse {
                tool_name: Some("Bash".into()),
            },
        );
        assert!(
            daemon.relocation_pending(&a1),
            "a tool hook is not a turn end"
        );
        daemon.complete_pending_move(&a1, &HookEvent::Stop);
        assert!(!daemon.relocation_pending(&a1));

        // Drained, the reparent is live again.
        daemon.reparent_agent_by_cwd(&a1, "/nebula-test/p", Some("s1"), false);
        assert_eq!(agent_worktree(&daemon, "a1"), "root");
    }

    #[tokio::test]
    async fn enter_worktree_creates_the_checkout_in_nebulas_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-b", "main"]);
        git_in(&repo, &["commit", "--allow-empty", "-m", "init"]);
        let daemon = test_daemon();
        daemon
            .store
            .insert_project(&Project {
                workspace_id: Default::default(),
                id: ProjectId("p".into()),
                name: "p".into(),
                repo_path: repo.clone(),
                sort_order: 0,
            })
            .unwrap();
        seed_worktree(&daemon, "p", "root", &repo.to_string_lossy(), true);
        seed_agent(&daemon, "a1", "root", Some("s1"));
        let a1 = AgentId("a1".into());
        let mut rx = daemon.events.subscribe();

        let (target, _) = daemon.enter_worktree(&a1, "feat", None).await.unwrap();
        assert_eq!(target.branch, "feat");
        assert_eq!(target.path, root.join("repo-worktrees").join("feat"));
        assert!(target.path.join(".git").exists(), "a real checkout");
        assert_eq!(agent_worktree(&daemon, "a1"), target.id.to_string());
        // The worktree's upsert lands first, then the agent's.
        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerEvent::EntityUpserted { entity: Entity::Worktree(w) } if w.id == target.id
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerEvent::EntityUpserted { entity: Entity::Agent(a) } if a.worktree_id == target.id
        ));
    }

    fn seed_pending_agent(daemon: &Daemon, id: &str, worktree: &str) {
        daemon
            .store
            .insert_agent_with_auto_title(
                &Agent {
                    id: AgentId(id.into()),
                    worktree_id: WorktreeId(worktree.into()),
                    name: format!("{id}-default"),
                    status: AgentStatus::Fresh,
                    archived: false,
                    archived_at: 0,
                    pinned: false,
                    unseen: false,
                    kind: AgentKind::Claude,
                    model: None,
                    effort: None,
                    session_id: None,
                    cloud_session_id: None,
                    sort_order: 0,
                    status_changed_at: 0,
                    alive: false,
                },
                true,
            )
            .unwrap();
    }

    #[test]
    fn auto_rename_applies_once_and_defers_to_user_titles() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_pending_agent(&daemon, "a1", "root");
        let mut rx = daemon.events.subscribe();

        // First agent attempt lands, sanitized, and is broadcast.
        daemon
            .auto_rename_agent(&AgentId("a1".into()), "  Fix   Login\tRedirect  ")
            .unwrap();
        match rx.try_recv().unwrap() {
            ServerEvent::EntityUpserted {
                entity: Entity::Agent(a),
            } => assert_eq!(a.name, "Fix Login Redirect"),
            other => panic!("expected agent upsert, got {other:?}"),
        }

        // A second attempt is declined with a settled, informative error.
        let err = daemon
            .auto_rename_agent(&AgentId("a1".into()), "Another Title")
            .unwrap_err();
        assert!(err.to_string().contains("already has a title"), "{err}");
        assert_eq!(
            daemon
                .store
                .get_agent(&AgentId("a1".into()))
                .unwrap()
                .unwrap()
                .name,
            "Fix Login Redirect"
        );

        // A user rename beats a pending auto-title: the CLI's later attempt
        // must not clobber it.
        seed_pending_agent(&daemon, "a2", "root");
        daemon
            .rename_agent(&AgentId("a2".into()), "my session")
            .unwrap();
        let err = daemon
            .auto_rename_agent(&AgentId("a2".into()), "Model Title")
            .unwrap_err();
        assert!(err.to_string().contains("already has a title"), "{err}");

        // Garbage titles are rejected outright.
        assert!(daemon
            .auto_rename_agent(&AgentId("a1".into()), " \u{7}\n ")
            .is_err());
        // Unknown agents report cleanly.
        let err = daemon
            .auto_rename_agent(&AgentId("ghost".into()), "Some Title")
            .unwrap_err();
        assert!(err.to_string().contains("agent not found"), "{err}");
    }

    #[test]
    fn sanitize_title_collapses_and_caps() {
        assert_eq!(
            sanitize_title(" Fix   Login\u{7}Redirect \n"),
            "Fix Login Redirect"
        );
        assert_eq!(sanitize_title("\u{1b}[31m"), "[31m");
        assert_eq!(sanitize_title("   "), "");
        let long = "word ".repeat(30);
        assert!(sanitize_title(&long).chars().count() <= 60);
        assert!(!sanitize_title(&long).ends_with(' '));
    }

    #[test]
    fn move_agent_rejects_cross_project_targets() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p", "q"]);
        seed_worktree(&daemon, "p", "p-root", "/nebula-test/p", true);
        seed_worktree(&daemon, "q", "q-root", "/nebula-test/q", true);
        seed_agent(&daemon, "a1", "p-root", None);

        let err = daemon
            .move_agent(&AgentId("a1".into()), &WorktreeId("q-root".into()))
            .unwrap_err();
        assert!(err.to_string().contains("different project"));
        assert_eq!(agent_worktree(&daemon, "a1"), "p-root");
    }

    #[test]
    fn reparent_by_cwd_picks_deepest_matching_worktree() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        // Nested layout: the linked checkout lives under the repo root, so
        // both paths are prefixes of a cwd inside it — deepest must win.
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_worktree(&daemon, "p", "feat", "/nebula-test/p/.wt/feat", false);
        seed_agent(&daemon, "a1", "root", None);

        // cwd inside the root checkout (but outside the nested worktree)
        // keeps the agent where it is.
        daemon.reparent_agent_by_cwd(&AgentId("a1".into()), "/nebula-test/p/src", None, false);
        assert_eq!(agent_worktree(&daemon, "a1"), "root");

        // cwd inside the nested worktree re-homes it there.
        daemon.reparent_agent_by_cwd(
            &AgentId("a1".into()),
            "/nebula-test/p/.wt/feat/src",
            None,
            false,
        );
        assert_eq!(agent_worktree(&daemon, "a1"), "feat");

        // cwd outside every worktree is ignored.
        daemon.reparent_agent_by_cwd(&AgentId("a1".into()), "/elsewhere", None, false);
        assert_eq!(agent_worktree(&daemon, "a1"), "feat");
    }

    /// Regression: a session that creates a worktree and steps into it
    /// reports the new cwd *before* the sync has adopted a row for it (the
    /// `Stop` hook fires long before the next 2s sync tick). The cwd must be
    /// remembered and replayed on adoption, or the row sits under the old
    /// checkout until the user's next prompt.
    #[tokio::test]
    async fn worktree_sync_replays_a_cwd_reported_before_adoption() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-b", "main"]);
        git_in(&repo, &["commit", "--allow-empty", "-m", "init"]);

        let daemon = test_daemon();
        let project = Project {
            workspace_id: Default::default(),
            id: ProjectId("p".into()),
            name: "p".into(),
            repo_path: repo.clone(),
            sort_order: 0,
        };
        daemon.store.insert_project(&project).unwrap();
        seed_worktree(&daemon, "p", "root", &repo.to_string_lossy(), true);
        seed_agent(&daemon, "a1", "root", Some("s1"));

        // The agent creates a sibling worktree and walks into it. The hook
        // lands first: no row exists yet, so nothing moves.
        let feat = root.join("repo-worktrees").join("feat");
        git_in(
            &repo,
            &["worktree", "add", &feat.to_string_lossy(), "-b", "feat"],
        );
        daemon.reparent_agent_by_cwd(
            &AgentId("a1".into()),
            &feat.to_string_lossy(),
            Some("s1"),
            false,
        );
        assert_eq!(agent_worktree(&daemon, "a1"), "root");

        // The sync adopts the checkout and replays the remembered cwd.
        daemon.sync_project_worktrees(&project).await.unwrap();
        let (_, worktrees, _, _) = daemon.store.load_tree().unwrap();
        let adopted = worktrees
            .iter()
            .find(|w| w.branch == "feat")
            .expect("feat worktree adopted");
        assert_eq!(agent_worktree(&daemon, "a1"), adopted.id.to_string());

        // A deliberate move back must survive the next adoption: the move
        // drops the remembered cwd, so replaying it can't overrule the user.
        daemon
            .move_agent(&AgentId("a1".into()), &WorktreeId("root".into()))
            .unwrap();
        let other = root.join("repo-worktrees").join("other");
        git_in(
            &repo,
            &["worktree", "add", &other.to_string_lossy(), "-b", "other"],
        );
        daemon.sync_project_worktrees(&project).await.unwrap();
        assert_eq!(agent_worktree(&daemon, "a1"), "root");
    }

    /// The replay is scoped to the synced project and skips archived rows.
    #[test]
    fn cwd_replay_skips_other_projects_and_archived_agents() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p", "q"]);
        seed_worktree(&daemon, "p", "p-root", "/nebula-test/p", true);
        seed_worktree(&daemon, "q", "q-root", "/nebula-test/q", true);
        seed_worktree(&daemon, "q", "q-feat", "/nebula-test/q-feat", false);
        seed_agent(&daemon, "a1", "q-root", None);
        seed_agent(&daemon, "a2", "q-root", None);

        // Both agents report a cwd inside q-feat before it exists...
        daemon
            .store
            .delete_worktree(&WorktreeId("q-feat".into()))
            .unwrap();
        daemon.reparent_agent_by_cwd(&AgentId("a1".into()), "/nebula-test/q-feat", None, false);
        daemon.reparent_agent_by_cwd(&AgentId("a2".into()), "/nebula-test/q-feat", None, false);
        seed_worktree(&daemon, "q", "q-feat", "/nebula-test/q-feat", false);

        // ...but a replay for project p touches neither.
        let p = daemon
            .store
            .get_project(&ProjectId("p".into()))
            .unwrap()
            .unwrap();
        daemon.reparent_agents_by_last_cwd(&p);
        assert_eq!(agent_worktree(&daemon, "a1"), "q-root");

        // Archived agents stay put; live ones re-home.
        daemon
            .store
            .set_agent_archived(&AgentId("a2".into()), true)
            .unwrap();
        let q = daemon
            .store
            .get_project(&ProjectId("q".into()))
            .unwrap()
            .unwrap();
        daemon.reparent_agents_by_last_cwd(&q);
        assert_eq!(agent_worktree(&daemon, "a1"), "q-feat");
        assert_eq!(agent_worktree(&daemon, "a2"), "q-root");
    }

    /// `git rev-parse --show-toplevel` answers with the checkout it ran in, so
    /// `nebula add .` from inside a linked worktree used to make the worktree
    /// the project: named after the branch directory, `repo_path` pointing at
    /// it, and a ⌂ root row for a directory the project did not own. The repo
    /// is the project no matter which of its checkouts you add it from.
    #[tokio::test]
    async fn add_project_from_inside_a_worktree_roots_at_the_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-b", "main"]);
        git_in(&repo, &["commit", "--allow-empty", "-m", "init"]);
        let feat = root.join("repo-worktrees").join("gentle-narwhal-files");
        git_in(
            &repo,
            &[
                "worktree",
                "add",
                &feat.to_string_lossy(),
                "-b",
                "gentle-narwhal-files",
            ],
        );

        let daemon = test_daemon();
        daemon.add_project(&feat, None, false, None).await.unwrap();

        let (projects, worktrees, _, _) = daemon.store.load_tree().unwrap();
        let project = projects.first().expect("project added");
        assert_eq!(project.repo_path, repo, "project is rooted at the repo");
        assert_eq!(project.name, "repo", "named after the repo, not the branch");

        let main: Vec<&Worktree> = worktrees.iter().filter(|w| w.is_main).collect();
        assert_eq!(main.len(), 1, "exactly one root row: {worktrees:#?}");
        assert_eq!(
            main[0].path, repo,
            "the ⌂ root row is the project's own dir"
        );
        assert_eq!(main[0].branch, "main");
        let linked = worktrees
            .iter()
            .find(|w| !w.is_main)
            .expect("the worktree we added from is a plain row");
        assert_eq!(linked.path, feat);
        assert_eq!(linked.branch, "gentle-narwhal-files");
    }

    /// Adding the repo from a worktree of one already in the workspace is the
    /// same repo, so it collides instead of arriving as a second project.
    #[tokio::test]
    async fn adding_a_worktree_of_a_known_repo_is_a_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-b", "main"]);
        git_in(&repo, &["commit", "--allow-empty", "-m", "init"]);
        let feat = root.join("repo-worktrees").join("feat");
        git_in(
            &repo,
            &["worktree", "add", &feat.to_string_lossy(), "-b", "feat"],
        );

        let daemon = test_daemon();
        daemon.add_project(&repo, None, false, None).await.unwrap();
        let err = daemon
            .add_project(&feat, None, false, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("already added"),
            "expected a duplicate error, got: {err}"
        );
    }

    /// Root-ness is derived from git's checkout list on every pass, not frozen
    /// at insert time: a project whose rows were seeded before the root was
    /// known (or seeded wrong) has its ⌂ root row repaired in place, and the
    /// stale one loses the reprieve that kept it undeletable.
    #[tokio::test]
    async fn reconcile_moves_root_ness_onto_the_checkout_git_lists_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-b", "main"]);
        git_in(&repo, &["commit", "--allow-empty", "-m", "init"]);
        let feat = root.join("repo-worktrees").join("feat");
        git_in(
            &repo,
            &["worktree", "add", &feat.to_string_lossy(), "-b", "feat"],
        );

        let daemon = test_daemon();
        let project = Project {
            workspace_id: Default::default(),
            id: ProjectId("p".into()),
            name: "p".into(),
            repo_path: repo.clone(),
            sort_order: 0,
        };
        daemon.store.insert_project(&project).unwrap();
        // The wrong way round: the linked checkout wears the root badge and
        // the repo's own checkout is a plain row.
        seed_worktree(&daemon, "p", "wt", &feat.to_string_lossy(), true);
        seed_worktree(&daemon, "p", "rt", &repo.to_string_lossy(), false);

        daemon.sync_project_worktrees(&project).await.unwrap();

        let (_, worktrees, _, _) = daemon.store.load_tree().unwrap();
        let by = |id: &str| worktrees.iter().find(|w| w.id.as_str() == id).unwrap();
        assert!(by("rt").is_main, "the repo's checkout is the root row");
        assert!(!by("wt").is_main, "the linked checkout gave the badge back");
        assert_eq!(by("rt").branch, "main");
        assert_eq!(by("wt").branch, "feat");
    }

    /// A row still carrying a stale `is_main` no longer survives its checkout
    /// going away — the real root is always in git's list, so anything missing
    /// from it is a linked checkout, whatever flag it happens to hold.
    #[tokio::test]
    async fn reconcile_drops_a_vanished_row_that_still_claims_to_be_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-b", "main"]);
        git_in(&repo, &["commit", "--allow-empty", "-m", "init"]);

        let daemon = test_daemon();
        let project = Project {
            workspace_id: Default::default(),
            id: ProjectId("p".into()),
            name: "p".into(),
            repo_path: repo.clone(),
            sort_order: 0,
        };
        daemon.store.insert_project(&project).unwrap();
        seed_worktree(&daemon, "p", "rt", &repo.to_string_lossy(), true);
        seed_worktree(
            &daemon,
            "p",
            "ghost",
            &root.join("repo-worktrees").join("gone").to_string_lossy(),
            true,
        );

        daemon.sync_project_worktrees(&project).await.unwrap();

        let (_, worktrees, _, _) = daemon.store.load_tree().unwrap();
        assert!(
            worktrees.iter().all(|w| w.id.as_str() != "ghost"),
            "the ghost row is gone: {worktrees:#?}"
        );
        let rt = worktrees.iter().find(|w| w.id.as_str() == "rt").unwrap();
        assert!(rt.is_main, "the surviving root row keeps the badge");
    }

    fn git_in(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn reparent_by_cwd_ignores_foreign_sessions_unless_capturing() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_worktree(&daemon, "p", "feat", "/nebula-test/p-feat", false);
        seed_agent(&daemon, "a1", "root", Some("s1"));

        // A different session id on a non-capturing event (a nested claude
        // launched inside the agent's PTY) must not move the row.
        daemon.reparent_agent_by_cwd(
            &AgentId("a1".into()),
            "/nebula-test/p-feat",
            Some("s2"),
            false,
        );
        assert_eq!(agent_worktree(&daemon, "a1"), "root");

        // A capturing event (re)establishes ownership, so it may move it.
        daemon.reparent_agent_by_cwd(
            &AgentId("a1".into()),
            "/nebula-test/p-feat",
            Some("s2"),
            true,
        );
        assert_eq!(agent_worktree(&daemon, "a1"), "feat");
    }

    #[test]
    fn normalize_url_adds_https_and_refuses_non_links() {
        // Pasted URLs pass through untouched.
        assert_eq!(
            normalize_url("https://github.com/o/r/pull/7").unwrap(),
            "https://github.com/o/r/pull/7"
        );
        assert_eq!(normalize_url("  http://x.dev  ").unwrap(), "http://x.dev");
        // Typed hosts gain the scheme.
        assert_eq!(
            normalize_url("github.com/o/r/pull/7").unwrap(),
            "https://github.com/o/r/pull/7"
        );
        // Anything that isn't an http(s) URL is refused, so `open(1)` can
        // never be handed a scheme the user didn't intend.
        for bad in [
            "",
            "   ",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://",
            "just a note",
            "notaurl",
        ] {
            assert!(normalize_url(bad).is_err(), "expected refusal: {bad:?}");
        }
    }

    #[test]
    fn cli_missing_message_names_the_binary_not_the_kind() {
        // Cursor ships its agent as `cursor-agent`; naming the kind would
        // send the user off to install the wrong thing.
        assert!(cli_missing_message(AgentKind::Cursor).starts_with("cursor-agent was not found"));
        assert!(cli_missing_message(AgentKind::Claude).starts_with("claude was not found"));
        assert!(cli_missing_message(AgentKind::Codex).starts_with("codex was not found"));
        // No "restart nebula": agent CLIs are spawned through the user's
        // login shell, so a fresh install is picked up on the next try.
        for kind in AgentKind::ALL {
            let msg = cli_missing_message(kind);
            assert!(msg.contains("try again"), "{msg}");
            assert!(!msg.contains("restart"), "{msg}");
        }
    }

    #[test]
    fn prewarm_pool_buffers_hooks_and_drops_dead_entries() {
        let daemon = test_daemon();
        let key = (WorktreeId("w1".into()), AgentKind::Claude);
        daemon.prewarmed.lock().unwrap().insert(
            key.clone(),
            PrewarmEntry {
                agent_id: AgentId("warm-1".into()),
                spawned_at: Instant::now(),
                model: None,
                effort: None,
                buffered_hooks: Vec::new(),
            },
        );

        // Hooks for the warm (row-less) id are buffered on the entry, not
        // dropped; hooks for unrelated unknown ids still vanish quietly.
        daemon.apply_hook_event(
            &AgentId("warm-1".into()),
            HookEvent::SessionStart { source: None },
            Some("sid-9".into()),
        );
        daemon.apply_hook_event(&AgentId("stranger".into()), HookEvent::Stop, None);
        {
            let pool = daemon.prewarmed.lock().unwrap();
            let entry = pool.get(&key).unwrap();
            assert_eq!(entry.buffered_hooks.len(), 1);
            assert_eq!(
                entry.buffered_hooks[0],
                (
                    HookEvent::SessionStart { source: None },
                    Some("sid-9".to_string())
                )
            );
        }

        // The buffer is bounded: overflow drops the oldest.
        for i in 0..(PREWARM_HOOK_BUFFER_CAP + 5) {
            daemon.apply_hook_event(
                &AgentId("warm-1".into()),
                HookEvent::Notification {
                    notification_type: Some(format!("n{i}")),
                },
                None,
            );
        }
        assert_eq!(
            daemon
                .prewarmed
                .lock()
                .unwrap()
                .get(&key)
                .unwrap()
                .buffered_hooks
                .len(),
            PREWARM_HOOK_BUFFER_CAP
        );

        // No live PTY backs the entry, so take() refuses it (create falls
        // back to a cold spawn) and reap clears it out.
        assert!(daemon
            .take_prewarmed(&WorktreeId("w1".into()), AgentKind::Claude, &None, &None)
            .is_none());
        assert!(daemon.prewarmed.lock().unwrap().is_empty());

        daemon.prewarmed.lock().unwrap().insert(
            key.clone(),
            PrewarmEntry {
                agent_id: AgentId("warm-2".into()),
                spawned_at: Instant::now(),
                model: None,
                effort: None,
                buffered_hooks: Vec::new(),
            },
        );
        daemon.reap_prewarmed();
        assert!(daemon.prewarmed.lock().unwrap().is_empty());
    }

    #[test]
    fn kill_prewarmed_in_scopes_to_worktrees() {
        let daemon = test_daemon();
        for (wt, id) in [("w1", "a"), ("w2", "b")] {
            daemon.prewarmed.lock().unwrap().insert(
                (WorktreeId(wt.into()), AgentKind::Codex),
                PrewarmEntry {
                    agent_id: AgentId(id.into()),
                    spawned_at: Instant::now(),
                    model: None,
                    effort: None,
                    buffered_hooks: Vec::new(),
                },
            );
        }
        daemon.kill_prewarmed_in(&[WorktreeId("w1".into())]);
        let pool = daemon.prewarmed.lock().unwrap();
        assert_eq!(pool.len(), 1);
        assert!(pool.contains_key(&(WorktreeId("w2".into()), AgentKind::Codex)));
    }

    #[test]
    fn reparent_by_cwd_skips_archived_agents() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_worktree(&daemon, "p", "feat", "/nebula-test/p-feat", false);
        seed_agent(&daemon, "a1", "root", None);
        daemon
            .store
            .set_agent_archived(&AgentId("a1".into()), true)
            .unwrap();

        daemon.reparent_agent_by_cwd(&AgentId("a1".into()), "/nebula-test/p-feat", None, false);
        assert_eq!(agent_worktree(&daemon, "a1"), "root");
    }

    // ---- workspaces ----

    #[test]
    fn workspace_lifecycle_add_open_rename_delete() {
        let daemon = test_daemon();
        let EntityId::Workspace(id) = daemon.add_workspace(" client ").unwrap() else {
            panic!("add returns the workspace id");
        };
        // Name is trimmed; duplicates (trimmed) and blanks are refused.
        assert_eq!(
            daemon.store.get_workspace(&id).unwrap().unwrap().name,
            "client"
        );
        assert!(daemon.add_workspace("client").is_err());
        assert!(daemon.add_workspace("   ").is_err());

        // Adding never opens; opening one moves the remembered default
        // (and re-opening is a quiet no-op).
        assert_eq!(
            daemon.store.active_workspace_id().unwrap().as_str(),
            "default"
        );
        daemon.set_default_workspace(&id).unwrap();
        assert_eq!(daemon.store.active_workspace_id().unwrap(), id);
        daemon.set_default_workspace(&id).unwrap();
        assert!(daemon
            .set_default_workspace(&WorkspaceId("ghost".into()))
            .is_err());

        // Rename keeps names unique (a rename to itself is fine).
        daemon.rename_workspace(&id, "acme").unwrap();
        daemon.rename_workspace(&id, "acme").unwrap();
        assert!(daemon.rename_workspace(&id, "default").is_err());

        // Deleting the default workspace moves the default to a survivor.
        daemon.remove_workspace(&id).unwrap();
        assert_eq!(
            daemon.store.active_workspace_id().unwrap().as_str(),
            "default"
        );
        assert!(daemon.store.get_workspace(&id).unwrap().is_none());

        // The last workspace can't go.
        assert!(daemon
            .remove_workspace(&WorkspaceId("default".into()))
            .is_err());
    }

    #[test]
    fn workspace_with_projects_refuses_deletion() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]); // lands in 'default'
        let EntityId::Workspace(empty) = daemon.add_workspace("empty").unwrap() else {
            panic!("add returns the workspace id");
        };
        let err = daemon
            .remove_workspace(&WorkspaceId("default".into()))
            .unwrap_err();
        assert!(
            err.to_string().contains("1 project"),
            "helpful refusal: {err}"
        );
        // An empty, closed workspace deletes cleanly.
        daemon.remove_workspace(&empty).unwrap();
    }

    /// Reorders only see the project's own workspace: a move never swaps
    /// across workspaces, and other workspaces' sort orders stay untouched.
    #[test]
    fn move_project_is_scoped_to_the_workspace() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["a", "b"]); // default ws, sort 0 and 1
        let EntityId::Workspace(other) = daemon.add_workspace("other").unwrap() else {
            panic!("add returns the workspace id");
        };
        daemon
            .store
            .insert_project(&Project {
                workspace_id: other.clone(),
                id: ProjectId("x".into()),
                name: "x".into(),
                repo_path: "/tmp/x".into(),
                sort_order: 1, // interleaves between a and b globally
            })
            .unwrap();

        daemon.move_project(&ProjectId("a".into()), 1).unwrap();
        let (projects, _, _, _) = daemon.store.load_tree().unwrap();
        let default_order: Vec<&str> = projects
            .iter()
            .filter(|p| p.workspace_id.as_str() == "default")
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(default_order, ["b", "a"], "a swapped with b, not x");
        let x = projects.iter().find(|p| p.name == "x").unwrap();
        assert_eq!(x.sort_order, 1, "other workspace untouched");
    }

    /// The status broadcast carries the flag it persisted: a live turn
    /// landing on finished says `unseen`, the next prompt says not.
    #[test]
    fn status_broadcast_carries_the_unseen_flag() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_agent(&daemon, "a1", "root", None); // running
        let id = AgentId("a1".into());
        let mut rx = daemon.events.subscribe();

        daemon.apply_status_effects(&id, vec![Effect::SetStatus(AgentStatus::Finished)]);
        match rx.try_recv().unwrap() {
            ServerEvent::StatusChanged { status, unseen, .. } => {
                assert_eq!(status, AgentStatus::Finished);
                assert!(unseen, "yellow → green with nobody told otherwise");
            }
            other => panic!("expected a status change, got {other:?}"),
        }
        daemon.apply_status_effects(&id, vec![Effect::SetStatus(AgentStatus::Running)]);
        match rx.try_recv().unwrap() {
            ServerEvent::StatusChanged { unseen, .. } => {
                assert!(!unseen, "a new turn: nothing finished to read")
            }
            other => panic!("expected a status change, got {other:?}"),
        }
    }

    /// `mark_agent_seen` clears the flag and hands every subscriber the row
    /// — once. Marking a row already read sends nothing.
    #[test]
    fn mark_agent_seen_broadcasts_only_a_flip() {
        let daemon = test_daemon();
        seed_projects(&daemon, &["p"]);
        seed_worktree(&daemon, "p", "root", "/nebula-test/p", true);
        seed_agent(&daemon, "a1", "root", None);
        let id = AgentId("a1".into());
        daemon
            .store
            .set_agent_status(&id, AgentStatus::Finished)
            .unwrap();
        let mut rx = daemon.events.subscribe();

        daemon.mark_agent_seen(&id).unwrap();
        match rx.try_recv().unwrap() {
            ServerEvent::EntityUpserted {
                entity: Entity::Agent(a),
            } => assert!(!a.unseen),
            other => panic!("expected agent upsert, got {other:?}"),
        }
        daemon.mark_agent_seen(&id).unwrap();
        assert!(rx.try_recv().is_err(), "nothing to say twice");
    }
}
