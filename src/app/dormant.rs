//! Dormant agents: agents whose pane was closed while they were still running.
//! They stay in the Agents panel until resumed or deleted. An agent that exits
//! on its own (for example Claude's `/exit`) clears its session before the pane
//! closes, so it never becomes dormant.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent_resume::AgentSessionRef;

const MAX_DORMANT_AGENTS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DormantAgent {
    pub source: String,
    pub agent: String,
    pub session_ref: AgentSessionRef,
    pub cwd: PathBuf,
    #[serde(default)]
    pub title: Option<String>,
}

impl DormantAgent {
    /// The agent's own session id (or path) identifies the entry.
    pub fn id(&self) -> &str {
        &self.session_ref.value
    }

    pub(crate) fn from_terminal(terminal: &crate::terminal::TerminalState) -> Option<Self> {
        if !terminal.is_agent_terminal() {
            return None;
        }
        let (source, agent, kind, value) = terminal.current_session_identity_for_persistence()?;
        let session_ref = AgentSessionRef { kind, value };
        // Only agents Herdr knows how to resume can come back from the list.
        crate::agent_resume::plan(&source, &agent, &session_ref)?;
        if !has_resumable_history(&agent, &session_ref) {
            return None;
        }
        Some(Self {
            source,
            agent,
            session_ref,
            cwd: terminal.cwd.clone(),
            title: terminal
                .terminal_title_stripped()
                .filter(|title| !title.trim().is_empty()),
        })
    }

    /// Arguments after the agent executable that resume this session.
    pub(crate) fn resume_args(&self) -> Option<Vec<String>> {
        let plan = crate::agent_resume::plan(&self.source, &self.agent, &self.session_ref)?;
        Some(plan.argv.into_iter().skip(1).collect())
    }
}

/// Claude saves no transcript until the first message, and `--resume` then fails;
/// such a conversation has nothing to come back to. Runs only when a pane closes.
fn has_resumable_history(agent: &str, session_ref: &AgentSessionRef) -> bool {
    if cfg!(test) || agent != "claude" {
        return true;
    }
    let Ok(config_dir) = crate::integration::claude_dir() else {
        return true;
    };
    let transcript = format!("{}.jsonl", session_ref.value);
    std::fs::read_dir(config_dir.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|project| project.path().join(&transcript).is_file())
}

/// Space label for a directory: its folder name, the same convention the
/// Explorer launcher uses when it opens an agent in Herdr.
pub(crate) fn label_for_cwd(cwd: &Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            cwd.display()
                .to_string()
                .trim_end_matches(['\\', '/'])
                .to_owned()
        })
}

impl super::state::AppState {
    pub(crate) fn remember_dormant_agent(&mut self, agent: DormantAgent) {
        self.dormant_agents.retain(|entry| entry.id() != agent.id());
        self.dormant_agents.insert(0, agent);
        self.dormant_agents.truncate(MAX_DORMANT_AGENTS);
        self.dormant_agents_dirty = true;
    }

    pub(crate) fn take_dormant_agent(&mut self, id: &str) -> Option<DormantAgent> {
        let index = self
            .dormant_agents
            .iter()
            .position(|entry| entry.id() == id)?;
        self.dormant_agents_dirty = true;
        Some(self.dormant_agents.remove(index))
    }
}

fn path() -> PathBuf {
    crate::session::data_dir().join("dormant-agents.json")
}

pub(crate) fn load() -> Vec<DormantAgent> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub(crate) fn save(agents: &[DormantAgent]) {
    let path = path();
    let result = serde_json::to_vec_pretty(agents)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)
        });
    if let Err(err) = result {
        tracing::warn!(err = %err, "failed to save dormant agents");
    }
}
