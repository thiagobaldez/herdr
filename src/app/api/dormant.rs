//! `agent.new`, `agent.dormant.resume` and `agent.delete`: the Agents panel's
//! right-click actions and the click that brings a dormant agent back.

use std::path::PathBuf;

use crate::api::schema::{
    AgentDeleteParams, AgentDormantTarget, AgentNewParams, AgentStartParams, EventData,
    EventEnvelope, EventKind, PaneTarget, ResponseResult, TabCreateParams, WorkspaceCreateParams,
    WorkspaceTarget,
};
use crate::app::dormant::{label_for_cwd, DormantAgent};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

/// Where a new agent tab goes: an existing space, or a new one for a folder.
enum AgentTabTarget {
    Workspace {
        workspace_id: String,
        cwd: Option<PathBuf>,
    },
    NewWorkspace {
        cwd: PathBuf,
        label: String,
    },
}

impl App {
    pub(super) fn handle_agent_new(&mut self, id: String, params: AgentNewParams) -> String {
        let ws_idx = match params.workspace_id.as_deref() {
            Some(workspace_id) => match self.parse_workspace_id(workspace_id) {
                Some(index) => index,
                None => {
                    return encode_error(
                        id,
                        "workspace_not_found",
                        format!("workspace {workspace_id} not found"),
                    )
                }
            },
            None => match self.state.active {
                Some(index) => index,
                None => return encode_error(id, "workspace_not_found", "no active workspace"),
            },
        };
        let Some(label) = self.workspace_label(ws_idx) else {
            return encode_error(id, "workspace_not_found", "workspace not found");
        };
        let target = AgentTabTarget::Workspace {
            workspace_id: self.public_workspace_id(ws_idx),
            cwd: None,
        };
        let kind = params.kind.unwrap_or_else(|| "claude".to_owned());
        self.start_agent_in_new_tab(id, target, &label, kind, Vec::new())
    }

    pub(super) fn handle_agent_dormant_resume(
        &mut self,
        id: String,
        target: AgentDormantTarget,
    ) -> String {
        let Some(dormant) = self
            .state
            .dormant_agents
            .iter()
            .find(|agent| agent.id() == target.id)
            .cloned()
        else {
            return encode_error(id, "agent_not_found", "dormant agent not found");
        };
        let Some(args) = dormant.resume_args() else {
            return encode_error(id, "agent_not_resumable", "this agent cannot be resumed");
        };
        if !dormant.cwd.is_dir() {
            return encode_error(
                id,
                "cwd_not_found",
                format!("folder not found: {}", dormant.cwd.display()),
            );
        }
        let label = label_for_cwd(&dormant.cwd);
        let tab_target = match (0..self.state.workspaces.len())
            .find(|&ws_idx| self.workspace_label(ws_idx).as_deref() == Some(label.as_str()))
        {
            Some(ws_idx) => AgentTabTarget::Workspace {
                workspace_id: self.public_workspace_id(ws_idx),
                cwd: Some(dormant.cwd.clone()),
            },
            None => AgentTabTarget::NewWorkspace {
                cwd: dormant.cwd.clone(),
                label: label.clone(),
            },
        };
        let response =
            self.start_agent_in_new_tab(id, tab_target, &label, dormant.agent.clone(), args);
        if !is_error_response(&response) {
            self.state.take_dormant_agent(&target.id);
        }
        response
    }

    pub(super) fn handle_agent_delete(&mut self, id: String, params: AgentDeleteParams) -> String {
        match (params.pane_id, params.dormant_id) {
            (Some(pane_id), None) => {
                let Some((ws_idx, pane)) = self.parse_pane_id(&pane_id) else {
                    return encode_error(id, "pane_not_found", format!("pane {pane_id} not found"));
                };
                let Some(terminal_id) = self.state.terminal_id_for_pane(ws_idx, pane) else {
                    return encode_error(id, "pane_not_found", format!("pane {pane_id} not found"));
                };
                let deleted = self
                    .state
                    .terminals
                    .get(&terminal_id)
                    .and_then(DormantAgent::from_terminal);
                self.state
                    .dormant_suppressed_terminals
                    .insert(terminal_id.clone());
                if let Err(response) = self.close_pane(id.clone(), &PaneTarget { pane_id }) {
                    self.state.dormant_suppressed_terminals.remove(&terminal_id);
                    return response;
                }
                if let Some(deleted) = deleted {
                    self.emit_agent_deleted(&deleted);
                }
                encode_success(id, ResponseResult::Ok {})
            }
            (None, Some(dormant_id)) => {
                let Some(deleted) = self.state.take_dormant_agent(&dormant_id) else {
                    return encode_error(id, "agent_not_found", "dormant agent not found");
                };
                self.emit_agent_deleted(&deleted);
                encode_success(id, ResponseResult::Ok {})
            }
            _ => encode_error(
                id,
                "invalid_request",
                "agent.delete needs exactly one of pane_id or dormant_id",
            ),
        }
    }

    fn workspace_label(&self, ws_idx: usize) -> Option<String> {
        self.state
            .workspaces
            .get(ws_idx)
            .map(|ws| ws.display_name_from(&self.state.terminals, &self.terminal_runtimes))
    }

    fn emit_agent_deleted(&mut self, agent: &DormantAgent) {
        self.emit_event(EventEnvelope {
            event: EventKind::AgentDeleted,
            data: EventData::AgentDeleted {
                agent: agent.agent.clone(),
                session_id: agent.session_ref.value.clone(),
                cwd: agent.cwd.display().to_string(),
            },
        });
    }

    /// Opens a tab (or a new space) and starts the agent in its shell, reusing
    /// `tab.create`, `workspace.create` and `agent.start`.
    fn start_agent_in_new_tab(
        &mut self,
        id: String,
        target: AgentTabTarget,
        label: &str,
        kind: String,
        args: Vec<String>,
    ) -> String {
        let created = match target {
            AgentTabTarget::Workspace { workspace_id, cwd } => {
                let created = self.handle_tab_create(
                    id.clone(),
                    TabCreateParams {
                        workspace_id: Some(workspace_id.clone()),
                        cwd: cwd.map(|cwd| cwd.display().to_string()),
                        focus: true,
                        label: None,
                        env: Default::default(),
                    },
                );
                let _ = self.handle_workspace_focus(id.clone(), WorkspaceTarget { workspace_id });
                created
            }
            AgentTabTarget::NewWorkspace { cwd, label } => self.handle_workspace_create(
                id.clone(),
                WorkspaceCreateParams {
                    source_workspace_id: None,
                    cwd: Some(cwd.display().to_string()),
                    focus: true,
                    label: Some(label),
                    env: Default::default(),
                },
            ),
        };
        let Some(pane_id) = created_root_pane_id(&created) else {
            return created;
        };
        let params = AgentStartParams {
            name: self.unique_agent_name(label),
            kind,
            pane_id,
            args,
            timeout_ms: None,
        };
        match self.start_agent(params) {
            Ok((agent, argv)) => encode_success(id, ResponseResult::AgentStarted { agent, argv }),
            Err(err) => encode_error_body(id, self.agent_start_error_body(err)),
        }
    }
}

fn is_error_response(response: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(response)
        .map_or(true, |value| value.get("error").is_some())
}

/// The root pane of a `tab.create` or `workspace.create` success response.
fn created_root_pane_id(response: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(response).ok()?;
    value
        .pointer("/result/root_pane/pane_id")?
        .as_str()
        .map(str::to_owned)
}
