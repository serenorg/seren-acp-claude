// ABOUTME: ACP server implementation using agent-client-protocol trait API
// ABOUTME: Implements acp::Agent trait to bridge ACP to Claude Code CLI

use super::AcpOutbound;
use crate::claude::ClaudeClient;
use agent_client_protocol::{self as acp, Client as _};
use claude_agent_sdk_rs::types::mcp::McpStdioServerConfig;
use claude_agent_sdk_rs::{
    ContentBlock, McpServerConfig, McpServers, Message, ToolResultContent, UserContentBlock,
};

use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{RwLock, mpsc};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Session data mapping ACP session to Claude client
struct SessionData {
    client: Arc<ClaudeClient>,
    #[allow(dead_code)]
    cwd: String,
    mode_id: String,
    current_model_id: Option<String>,
}

/// Claude ACP Agent - bridges ACP protocol to Claude Code CLI
pub struct ClaudeAgent {
    /// Channel for sending outbound messages (notifications / permission requests) to the ACP client
    acp_tx: mpsc::UnboundedSender<AcpOutbound>,
    /// Session mappings: session_id -> SessionData
    sessions: RwLock<HashMap<String, SessionData>>,
}

impl ClaudeAgent {
    /// Create a new Claude agent
    pub fn new(acp_tx: mpsc::UnboundedSender<AcpOutbound>) -> Self {
        Self {
            acp_tx,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    fn emit_update(
        &self,
        session_id: &acp::SessionId,
        update: acp::SessionUpdate,
    ) -> Result<(), acp::Error> {
        self.acp_tx
            .send(AcpOutbound::SessionNotification(
                acp::SessionNotification::new(session_id.clone(), update),
            ))
            .map_err(|_| acp::Error::new(-32603, "ACP outbound channel closed"))
    }

    fn claude_projects_root() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home).join(".claude").join("projects"))
    }

    fn encode_project_dir_name(cwd: &Path) -> String {
        // Claude Code stores per-project session indexes under:
        //   ~/.claude/projects/<encoded-absolute-path>/
        // where encoding is a simple path separator replacement (e.g. "/Users/a/b" -> "-Users-a-b").
        let s = cwd.to_string_lossy();
        format!("-{}", s.trim_start_matches('/').replace('/', "-"))
    }

    async fn find_sessions_index_path(cwd: &Path) -> Option<PathBuf> {
        let root = Self::claude_projects_root()?;

        // Fast-path: deterministic encoding.
        let direct = root
            .join(Self::encode_project_dir_name(cwd))
            .join("sessions-index.json");
        if tokio::fs::try_exists(&direct).await.unwrap_or(false) {
            return Some(direct);
        }

        // Fallback: scan for a sessions-index.json whose originalPath matches the cwd.
        let mut rd = tokio::fs::read_dir(root).await.ok()?;
        while let Ok(Some(ent)) = rd.next_entry().await {
            let path = ent.path().join("sessions-index.json");
            if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                continue;
            }
            let Ok(bytes) = tokio::fs::read(&path).await else {
                continue;
            };
            let Ok(index) = serde_json::from_slice::<ClaudeSessionsIndex>(&bytes) else {
                continue;
            };
            if index.original_path.as_deref() == Some(&cwd.to_string_lossy()) {
                return Some(path);
            }
        }
        None
    }

    async fn find_session_jsonl_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
        // Prefer the sessions-index fullPath if available.
        if let Some(index_path) = Self::find_sessions_index_path(cwd).await {
            if let Ok(bytes) = tokio::fs::read(&index_path).await {
                if let Ok(index) = serde_json::from_slice::<ClaudeSessionsIndex>(&bytes) {
                    if let Some(ent) = index.entries.iter().find(|e| e.session_id == session_id) {
                        return Some(PathBuf::from(ent.full_path.clone()));
                    }
                }
            }
        }

        // Best-effort fallback: infer the path.
        let root = Self::claude_projects_root()?;
        let dir = root.join(Self::encode_project_dir_name(cwd));
        Some(dir.join(format!("{session_id}.jsonl")))
    }

    fn replay_chunk_meta(v: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();
        meta.insert("replay".to_string(), serde_json::Value::Bool(true));

        if let Some(message_id) = v.get("uuid").and_then(|x| x.as_str()) {
            meta.insert(
                "messageId".to_string(),
                serde_json::Value::String(message_id.to_string()),
            );
        }

        if let Some(timestamp) = v.get("timestamp").cloned() {
            meta.insert("timestamp".to_string(), timestamp);
        }

        meta
    }

    async fn replay_history_best_effort(
        &self,
        session_id: &acp::SessionId,
        cwd: &Path,
        claude_session_id: &str,
    ) -> Result<(), acp::Error> {
        let Some(path) = Self::find_session_jsonl_path(cwd, claude_session_id).await else {
            return Ok(());
        };
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(());
        }

        let file = tokio::fs::File::open(&path).await.map_err(|e| {
            acp::Error::new(
                -32603,
                format!("Failed to read Claude session history: {e}"),
            )
        })?;
        let mut lines = BufReader::new(file).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(t) = v.get("type").and_then(|x| x.as_str()) else {
                continue;
            };

            let replay_meta = Self::replay_chunk_meta(&v);

            match t {
                "user" => {
                    let Some(content) = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    else {
                        continue;
                    };
                    for block in content {
                        match block.get("type").and_then(|x| x.as_str()) {
                            Some("text") => {
                                let text = block.get("text").and_then(|x| x.as_str()).unwrap_or("");
                                if !text.is_empty() {
                                    self.emit_update(
                                        session_id,
                                        acp::SessionUpdate::UserMessageChunk(
                                            acp::ContentChunk::new(text.to_string().into())
                                                .meta(replay_meta.clone()),
                                        ),
                                    )?;
                                }
                            }
                            Some("tool_result") => {
                                let tool_use_id = block
                                    .get("tool_use_id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("");
                                if tool_use_id.is_empty() {
                                    continue;
                                }
                                let is_error = block
                                    .get("is_error")
                                    .and_then(|x| x.as_bool())
                                    .unwrap_or(false);
                                let status = if is_error {
                                    acp::ToolCallStatus::Failed
                                } else {
                                    acp::ToolCallStatus::Completed
                                };
                                let raw_output = block
                                    .get("content")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                self.emit_update(
                                    session_id,
                                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                        acp::ToolCallId::new(format!("claude-tool:{tool_use_id}")),
                                        acp::ToolCallUpdateFields::new()
                                            .status(status)
                                            .raw_output(raw_output),
                                    )),
                                )?;
                            }
                            _ => {}
                        }
                    }
                }
                "assistant" => {
                    let Some(content) = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    else {
                        continue;
                    };
                    for block in content {
                        match block.get("type").and_then(|x| x.as_str()) {
                            Some("text") => {
                                let text = block.get("text").and_then(|x| x.as_str()).unwrap_or("");
                                if !text.is_empty() {
                                    self.emit_update(
                                        session_id,
                                        acp::SessionUpdate::AgentMessageChunk(
                                            acp::ContentChunk::new(text.to_string().into())
                                                .meta(replay_meta.clone()),
                                        ),
                                    )?;
                                }
                            }
                            Some("thinking") => {
                                let text =
                                    block.get("thinking").and_then(|x| x.as_str()).unwrap_or("");
                                if !text.is_empty() {
                                    self.emit_update(
                                        session_id,
                                        acp::SessionUpdate::AgentThoughtChunk(
                                            acp::ContentChunk::new(text.to_string().into())
                                                .meta(replay_meta.clone()),
                                        ),
                                    )?;
                                }
                            }
                            Some("tool_use") => {
                                let tool_use_id =
                                    block.get("id").and_then(|x| x.as_str()).unwrap_or("");
                                let name =
                                    block.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                                if tool_use_id.is_empty() {
                                    continue;
                                }
                                let input = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                self.emit_update(
                                    session_id,
                                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                        acp::ToolCallId::new(format!("claude-tool:{tool_use_id}")),
                                        acp::ToolCallUpdateFields::new()
                                            .title(name.to_string())
                                            .kind(tool_kind_for_claude_tool(name))
                                            .status(acp::ToolCallStatus::InProgress)
                                            .raw_input(input),
                                    )),
                                )?;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn build_mode_state(current: &'static str) -> acp::SessionModeState {
        // Claude Code supports these permission modes via its CLI (and the Rust SDK).
        let default_mode = acp::SessionMode::new("default", "Default")
            .description("Standard behavior".to_string());
        let accept_edits_mode = acp::SessionMode::new("acceptEdits", "Accept Edits")
            .description("Auto-accept file edit operations".to_string());
        let plan_mode = acp::SessionMode::new("plan", "Plan Mode")
            .description("Planning mode; no actual tool execution".to_string());
        let bypass_mode = acp::SessionMode::new("bypassPermissions", "Bypass Permissions")
            .description("Auto-approve all operations".to_string());

        acp::SessionModeState::new(
            current,
            vec![default_mode, accept_edits_mode, plan_mode, bypass_mode],
        )
    }

    async fn get_models_state_best_effort(
        client: &ClaudeClient,
        preferred_model: Option<&str>,
    ) -> Option<acp::SessionModelState> {
        let info = client.get_server_info().await?;
        parse_models_from_server_info(&info, preferred_model)
    }

    async fn emit_available_commands_best_effort(
        &self,
        session_id: &acp::SessionId,
        client: &ClaudeClient,
    ) {
        let Some(info) = client.get_server_info().await else {
            return;
        };
        let cmds = parse_commands_from_server_info(&info);
        if cmds.is_empty() {
            return;
        }
        let _ = self.emit_update(
            session_id,
            acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(cmds)),
        );
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSessionsIndex {
    #[serde(default)]
    entries: Vec<ClaudeSessionsIndexEntry>,
    #[serde(default, rename = "originalPath")]
    original_path: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSessionsIndexEntry {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "fullPath")]
    full_path: String,
    #[serde(default, rename = "firstPrompt")]
    first_prompt: Option<String>,
    #[serde(default)]
    modified: Option<String>,
    #[serde(default, rename = "projectPath")]
    project_path: Option<String>,
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ClaudeAgent {
    async fn initialize(
        &self,
        request: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        info!("Received initialize request");

        let protocol_version = if request.protocol_version < acp::ProtocolVersion::V1 {
            acp::ProtocolVersion::V1
        } else {
            std::cmp::min(request.protocol_version, acp::ProtocolVersion::LATEST)
        };

        // Build capabilities
        let prompt_caps = acp::PromptCapabilities::new()
            .embedded_context(true)
            .image(true);
        let mut agent_caps = acp::AgentCapabilities::new()
            .prompt_capabilities(prompt_caps)
            .mcp_capabilities(acp::McpCapabilities::new())
            .load_session(true);
        agent_caps.session_capabilities = acp::SessionCapabilities::new()
            .list(acp::SessionListCapabilities::new())
            .fork(acp::SessionForkCapabilities::new())
            .resume(acp::SessionResumeCapabilities::new());

        // Build agent info
        let agent_info = acp::Implementation::new("seren-acp-claude", env!("CARGO_PKG_VERSION"))
            .title("Claude Code".to_string());

        Ok(acp::InitializeResponse::new(protocol_version)
            .agent_capabilities(agent_caps)
            .agent_info(agent_info))
    }

    async fn authenticate(
        &self,
        _request: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        info!("Received authenticate request");
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        request: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let cwd = request.cwd.to_string_lossy().to_string();
        info!("Received new session request for cwd: {}", cwd);
        info!("MCP servers from client: {}", request.mcp_servers.len());

        // Convert ACP MCP servers to SDK format
        let mcp_servers = convert_acp_mcp_to_sdk(&request.mcp_servers);

        // Generate session ID
        let session_id = uuid::Uuid::new_v4().to_string();

        // Create Claude client for this session
        let client = Arc::new(ClaudeClient::new(
            request.cwd.clone(),
            session_id.clone(),
            None,
            false,
            acp::SessionId::new(session_id.clone()),
            self.acp_tx.clone(),
            mcp_servers,
        ));

        // Connect to Claude CLI
        client.connect().await.map_err(|e| {
            let err_str = e.to_string();

            // Extract clean error message for common cases
            let clean_msg = if err_str.contains("below minimum required version") {
                // Version mismatch - extract the actionable part
                if let Some(start) = err_str.find("Claude Code CLI version") {
                    err_str[start..].to_string()
                } else {
                    format!("Claude CLI version is outdated. Please update with: npm install -g @anthropic-ai/claude-code@latest")
                }
            } else if err_str.contains("not found") || err_str.contains("No such file") {
                "Claude CLI not found. Please install with: npm install -g @anthropic-ai/claude-code".to_string()
            } else {
                // For other errors, include the full message
                format!("Connection failed: {}", err_str)
            };

            error!("Failed to connect to Claude CLI: {}", clean_msg);
            acp::Error::new(-32603, clean_msg)
        })?;

        info!("Created session {} for cwd {}", session_id, cwd);

        let acp_session_id = acp::SessionId::new(session_id.clone());
        self.emit_available_commands_best_effort(&acp_session_id, client.as_ref())
            .await;

        let mode_state = Self::build_mode_state("default");
        let models = Self::get_models_state_best_effort(client.as_ref(), None).await;
        let current_model_id = models
            .as_ref()
            .map(|m| m.current_model_id.0.as_ref().to_string());

        // Store session mapping
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_id.clone(),
                SessionData {
                    client,
                    cwd,
                    mode_id: "default".to_string(),
                    current_model_id,
                },
            );
        }

        Ok(acp::NewSessionResponse::new(session_id)
            .modes(mode_state)
            .models(models))
    }

    async fn load_session(
        &self,
        request: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let cwd = request.cwd.to_string_lossy().to_string();
        let session_id = request.session_id.0.to_string();
        info!(
            "Received load session request: session={}, cwd={}",
            session_id, cwd
        );

        // Best-effort: verify the session exists on disk before spawning a CLI.
        let history_path = Self::find_session_jsonl_path(&request.cwd, &session_id).await;
        let history_exists = match history_path.as_ref() {
            Some(p) => tokio::fs::try_exists(p).await.unwrap_or(false),
            None => false,
        };
        if !history_exists {
            return Err(acp::Error::new(
                -32603,
                format!("Claude session not found: {session_id}"),
            ));
        }

        // Convert ACP MCP servers to SDK format
        let mcp_servers = convert_acp_mcp_to_sdk(&request.mcp_servers);

        let client = Arc::new(ClaudeClient::new(
            request.cwd.clone(),
            session_id.clone(),
            Some(session_id.clone()),
            false,
            request.session_id.clone(),
            self.acp_tx.clone(),
            mcp_servers,
        ));

        client.connect().await.map_err(|e| {
            let msg = format!("Failed to connect to Claude CLI for resume: {e}");
            error!("{msg}");
            acp::Error::new(-32603, msg)
        })?;

        // Replay transcript so clients without local persistence can reconstruct the conversation.
        // seren-desktop suppresses these during resume when it already has local history.
        if let Err(e) = self
            .replay_history_best_effort(&request.session_id, &request.cwd, &session_id)
            .await
        {
            warn!("Failed to replay Claude history (best-effort): {e}");
        }

        self.emit_available_commands_best_effort(&request.session_id, client.as_ref())
            .await;

        let mode_state = Self::build_mode_state("default");
        let models = Self::get_models_state_best_effort(client.as_ref(), None).await;
        let current_model_id = models
            .as_ref()
            .map(|m| m.current_model_id.0.as_ref().to_string());

        // Store session mapping
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_id.clone(),
                SessionData {
                    client,
                    cwd,
                    mode_id: "default".to_string(),
                    current_model_id,
                },
            );
        }

        Ok(acp::LoadSessionResponse::new()
            .modes(mode_state)
            .models(models))
    }

    async fn list_sessions(
        &self,
        request: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        info!("Received list sessions request");

        let Some(cwd) = request.cwd.as_deref() else {
            // Avoid scanning all projects by default (Claude stores thousands of per-project dirs).
            return Ok(acp::ListSessionsResponse::new(Vec::new()));
        };

        let Some(index_path) = Self::find_sessions_index_path(cwd).await else {
            return Ok(acp::ListSessionsResponse::new(Vec::new()));
        };

        let bytes = tokio::fs::read(&index_path)
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to read sessions index: {e}")))?;
        let index: ClaudeSessionsIndex = serde_json::from_slice(&bytes)
            .map_err(|e| acp::Error::new(-32603, format!("Invalid sessions index JSON: {e}")))?;

        let mut out: Vec<acp::SessionInfo> = Vec::new();
        for ent in index.entries {
            let session_cwd = ent
                .project_path
                .clone()
                .unwrap_or_else(|| cwd.to_string_lossy().to_string());
            let mut info = acp::SessionInfo::new(ent.session_id, PathBuf::from(session_cwd));
            if let Some(title) = ent.first_prompt {
                info = info.title(title);
            }
            if let Some(updated_at) = ent.modified {
                info = info.updated_at(updated_at);
            }
            out.push(info);
        }

        Ok(acp::ListSessionsResponse::new(out))
    }

    async fn resume_session(
        &self,
        request: acp::ResumeSessionRequest,
    ) -> Result<acp::ResumeSessionResponse, acp::Error> {
        let cwd = request.cwd.to_string_lossy().to_string();
        let session_id = request.session_id.0.to_string();
        info!(
            "Received resume session request: session={}, cwd={}",
            session_id, cwd
        );

        // Best-effort: verify the session exists on disk before spawning a CLI.
        let history_path = Self::find_session_jsonl_path(&request.cwd, &session_id).await;
        let history_exists = match history_path.as_ref() {
            Some(p) => tokio::fs::try_exists(p).await.unwrap_or(false),
            None => false,
        };
        if !history_exists {
            return Err(acp::Error::new(
                -32603,
                format!("Claude session not found: {session_id}"),
            ));
        }

        let mcp_servers = convert_acp_mcp_to_sdk(&request.mcp_servers);

        let client = Arc::new(ClaudeClient::new(
            request.cwd.clone(),
            session_id.clone(),
            Some(session_id.clone()),
            false,
            request.session_id.clone(),
            self.acp_tx.clone(),
            mcp_servers,
        ));

        client.connect().await.map_err(|e| {
            let msg = format!("Failed to connect to Claude CLI for resume: {e}");
            error!("{msg}");
            acp::Error::new(-32603, msg)
        })?;

        self.emit_available_commands_best_effort(&request.session_id, client.as_ref())
            .await;

        // No transcript replay for resume_session.
        let mode_state = Self::build_mode_state("default");
        let models = Self::get_models_state_best_effort(client.as_ref(), None).await;
        let current_model_id = models
            .as_ref()
            .map(|m| m.current_model_id.0.as_ref().to_string());

        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_id.clone(),
                SessionData {
                    client,
                    cwd,
                    mode_id: "default".to_string(),
                    current_model_id,
                },
            );
        }

        Ok(acp::ResumeSessionResponse::new()
            .modes(mode_state)
            .models(models))
    }

    async fn fork_session(
        &self,
        request: acp::ForkSessionRequest,
    ) -> Result<acp::ForkSessionResponse, acp::Error> {
        let cwd = request.cwd.to_string_lossy().to_string();
        let from_session_id = request.session_id.0.to_string();
        info!(
            "Received fork session request: from_session={}, cwd={}",
            from_session_id, cwd
        );

        // Verify the source session exists before forking.
        let history_path = Self::find_session_jsonl_path(&request.cwd, &from_session_id).await;
        let history_exists = match history_path.as_ref() {
            Some(p) => tokio::fs::try_exists(p).await.unwrap_or(false),
            None => false,
        };
        if !history_exists {
            return Err(acp::Error::new(
                -32603,
                format!("Claude session not found: {from_session_id}"),
            ));
        }

        let new_session_id = uuid::Uuid::new_v4().to_string();

        let mcp_servers = convert_acp_mcp_to_sdk(&request.mcp_servers);

        let client = Arc::new(ClaudeClient::new(
            request.cwd.clone(),
            new_session_id.clone(),
            Some(from_session_id.clone()),
            true,
            acp::SessionId::new(new_session_id.clone()),
            self.acp_tx.clone(),
            mcp_servers,
        ));

        client.connect().await.map_err(|e| {
            let msg = format!("Failed to connect to Claude CLI for fork: {e}");
            error!("{msg}");
            acp::Error::new(-32603, msg)
        })?;

        let acp_session_id = acp::SessionId::new(new_session_id.clone());
        self.emit_available_commands_best_effort(&acp_session_id, client.as_ref())
            .await;

        let mode_state = Self::build_mode_state("default");
        let models = Self::get_models_state_best_effort(client.as_ref(), None).await;
        let current_model_id = models
            .as_ref()
            .map(|m| m.current_model_id.0.as_ref().to_string());

        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                new_session_id.clone(),
                SessionData {
                    client,
                    cwd,
                    mode_id: "default".to_string(),
                    current_model_id,
                },
            );
        }

        Ok(acp::ForkSessionResponse::new(new_session_id)
            .modes(mode_state)
            .models(models))
    }

    async fn prompt(&self, request: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let session_id_str: &str = &request.session_id.0;
        info!("Received prompt request for session: {}", session_id_str);

        // Get client for this session
        let client = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_id_str)
                .map(|s| Arc::clone(&s.client))
                .ok_or_else(|| {
                    acp::Error::new(-32603, format!("Session not found: {}", session_id_str))
                })?
        };

        // Convert ACP content blocks to SDK UserContentBlocks
        let mut content_blocks: Vec<UserContentBlock> = Vec::new();
        let mut has_text = false;

        for block in &request.prompt {
            match block {
                acp::ContentBlock::Text(text) => {
                    if !text.text.is_empty() {
                        content_blocks.push(UserContentBlock::text(text.text.clone()));
                        has_text = true;
                    }
                }
                acp::ContentBlock::Image(image) => {
                    // Convert ACP ImageContent to SDK UserContentBlock::Image
                    match UserContentBlock::image_from_base64(
                        &image.mime_type,
                        &image.data,
                    ) {
                        Ok(image_block) => {
                            info!("Added image block: type={}", image.mime_type);
                            content_blocks.push(image_block);
                        }
                        Err(e) => {
                            error!("Failed to create image block: {}", e);
                            return Err(acp::Error::new(
                                -32602,
                                format!("Invalid image data: {}", e),
                            ));
                        }
                    }
                }
            }
        }

        if !has_text {
            return Err(acp::Error::new(-32602, "No text content in prompt"));
        }

        debug!(
            "Sending {} content blocks to Claude ({} images)",
            content_blocks.len(),
            content_blocks.len() - content_blocks.iter().filter(|b| matches!(b, UserContentBlock::Text { .. })).count()
        );

        // Subscribe to message stream before sending query
        let mut rx = client.subscribe();
        let session_id = request.session_id.clone();
        let acp_tx = self.acp_tx.clone();

        // Spawn task to forward Claude messages to ACP notifications
        tokio::task::spawn_local(async move {
            loop {
                match rx.recv().await {
                    Ok(message) => {
                        // Convert Claude message to ACP notification
                        for update in convert_claude_message(&message) {
                            let acp_notif =
                                acp::SessionNotification::new(session_id.clone(), update);
                            if acp_tx
                                .send(AcpOutbound::SessionNotification(acp_notif))
                                .is_err()
                            {
                                return;
                            }
                        }

                        // Check for result message
                        if matches!(message, Message::Result(_)) {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        // Send query with content blocks (supports text + images)
        client
            .query_with_content(content_blocks)
            .await
            .map_err(|e| {
                error!("Claude query failed: {}", e);
                acp::Error::new(-32603, format!("Query failed: {}", e))
            })?;

        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn cancel(&self, request: acp::CancelNotification) -> Result<(), acp::Error> {
        let session_id_str: &str = &request.session_id.0;
        info!("Received cancel request for session: {}", session_id_str);

        // Get client for this session
        let client = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_id_str)
                .map(|s| Arc::clone(&s.client))
                .ok_or_else(|| {
                    acp::Error::new(-32603, format!("Session not found: {}", session_id_str))
                })?
        };

        client.cancel().await;
        Ok(())
    }

    async fn set_session_mode(
        &self,
        request: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        info!("Received set session mode request: {:?}", request.mode_id);
        let session_id_str: &str = &request.session_id.0;
        let mode_id: &str = request.mode_id.0.as_ref();

        if mode_id != "default"
            && mode_id != "acceptEdits"
            && mode_id != "plan"
            && mode_id != "bypassPermissions"
        {
            return Err(acp::Error::new(
                -32602,
                format!("Unsupported mode id: {mode_id}"),
            ));
        }

        {
            let mut sessions = self.sessions.write().await;
            let session = sessions.get_mut(session_id_str).ok_or_else(|| {
                acp::Error::new(-32603, format!("Session not found: {session_id_str}"))
            })?;
            session.mode_id = mode_id.to_string();
            session
                .client
                .set_session_mode(mode_id)
                .await
                .map_err(|e| acp::Error::new(-32603, format!("Failed to set session mode: {e}")))?;
        }

        Ok(acp::SetSessionModeResponse::default())
    }

    async fn set_session_model(
        &self,
        request: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        let session_id_str: &str = &request.session_id.0;
        let model_id: &str = request.model_id.0.as_ref();
        info!(
            "Received set session model request: session={}, model={}",
            session_id_str, model_id
        );

        let client = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_id_str)
                .map(|s| Arc::clone(&s.client))
                .ok_or_else(|| {
                    acp::Error::new(-32603, format!("Session not found: {session_id_str}"))
                })?
        };

        client
            .set_model(Some(model_id))
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to set session model: {e}")))?;

        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(session_id_str) {
                session.current_model_id = Some(model_id.to_string());
            }
        }

        Ok(acp::SetSessionModelResponse::new())
    }

    async fn ext_method(&self, _request: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _request: acp::ExtNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}

/// Convert Claude SDK message to ACP SessionUpdate
fn convert_claude_message(message: &Message) -> Vec<acp::SessionUpdate> {
    match message {
        Message::Assistant(assistant) => assistant
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text_block) => {
                    if text_block.text.is_empty() {
                        None
                    } else {
                        Some(acp::SessionUpdate::AgentMessageChunk(
                            acp::ContentChunk::new(text_block.text.clone().into()),
                        ))
                    }
                }
                ContentBlock::Thinking(thinking_block) => {
                    if thinking_block.thinking.is_empty() {
                        None
                    } else {
                        Some(acp::SessionUpdate::AgentThoughtChunk(
                            acp::ContentChunk::new(thinking_block.thinking.clone().into()),
                        ))
                    }
                }
                ContentBlock::ToolUse(tool_use) => Some(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(
                        acp::ToolCallId::new(format!("claude-tool:{}", tool_use.id)),
                        acp::ToolCallUpdateFields::new()
                            .title(tool_use.name.clone())
                            .kind(tool_kind_for_claude_tool(&tool_use.name))
                            .status(acp::ToolCallStatus::InProgress)
                            .raw_input(tool_use.input.clone()),
                    ),
                )),
                ContentBlock::ToolResult(tool_result) => {
                    let status = if tool_result.is_error.unwrap_or(false) {
                        acp::ToolCallStatus::Failed
                    } else {
                        acp::ToolCallStatus::Completed
                    };

                    let raw_output = match &tool_result.content {
                        Some(ToolResultContent::Text(text)) => {
                            serde_json::Value::String(text.clone())
                        }
                        Some(ToolResultContent::Blocks(blocks)) => {
                            serde_json::Value::Array(blocks.clone())
                        }
                        None => serde_json::Value::Null,
                    };

                    Some(acp::SessionUpdate::ToolCallUpdate(
                        acp::ToolCallUpdate::new(
                            acp::ToolCallId::new(format!(
                                "claude-tool:{}",
                                tool_result.tool_use_id
                            )),
                            acp::ToolCallUpdateFields::new()
                                .status(status)
                                .raw_output(raw_output),
                        ),
                    ))
                }
                ContentBlock::Image(_) => None,
            })
            .collect(),
        Message::StreamEvent(event) => {
            // Stream events may contain delta text
            if let Some(delta) = event.event.get("delta").and_then(|d| d.as_str()) {
                let content = acp::ContentChunk::new(delta.to_string().into());
                vec![acp::SessionUpdate::AgentMessageChunk(content)]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn tool_kind_for_claude_tool(tool_name: &str) -> acp::ToolKind {
    let lower = tool_name.to_ascii_lowercase();
    if lower.contains("bash") || lower.contains("shell") || lower.contains("exec") {
        return acp::ToolKind::Execute;
    }
    if lower.contains("read") {
        return acp::ToolKind::Read;
    }
    if lower.contains("edit") || lower.contains("write") || lower.contains("replace") {
        return acp::ToolKind::Edit;
    }
    if lower.contains("search") || lower.contains("grep") || lower.contains("find") {
        return acp::ToolKind::Search;
    }
    if lower.contains("fetch") || lower.contains("web") || lower.contains("http") {
        return acp::ToolKind::Fetch;
    }
    acp::ToolKind::Other
}

fn parse_models_from_server_info(
    info: &serde_json::Value,
    preferred_model: Option<&str>,
) -> Option<acp::SessionModelState> {
    let mut model_entries: Vec<serde_json::Value> = Vec::new();
    if let Some(models) = info.get("models") {
        append_model_entries(models, &mut model_entries);
    }
    for key in [
        "availableModels",
        "available_models",
        "modelOptions",
        "model_options",
    ] {
        if let Some(v) = info.get(key) {
            append_model_entries(v, &mut model_entries);
        }
    }

    if model_entries.is_empty() {
        return None;
    }

    let mut available: Vec<acp::ModelInfo> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut current_from_flags: Option<String> = None;
    let mut default_from_flags: Option<String> = None;

    for model in &model_entries {
        let id = model
            .get("value")
            .and_then(|v| v.as_str())
            .or_else(|| model.get("modelId").and_then(|v| v.as_str()))
            .or_else(|| model.get("model_id").and_then(|v| v.as_str()))
            .or_else(|| model.get("id").and_then(|v| v.as_str()))
            .or_else(|| model.get("name").and_then(|v| v.as_str()))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let Some(id) = id.filter(|s| !s.is_empty()) else {
            continue;
        };

        if !seen_ids.insert(id.to_string()) {
            continue;
        }

        let name = model
            .get("displayName")
            .and_then(|v| v.as_str())
            .or_else(|| model.get("display_name").and_then(|v| v.as_str()))
            .or_else(|| model.get("name").and_then(|v| v.as_str()))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(id);

        let description = model.get("description").and_then(|v| v.as_str());

        if current_from_flags.is_none()
            && (model
                .get("isCurrent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || model
                    .get("current")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                || model
                    .get("isSelected")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                || model
                    .get("selected")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false))
        {
            current_from_flags = Some(id.to_string());
        }

        if default_from_flags.is_none()
            && (model
                .get("isDefault")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || model
                    .get("default")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false))
        {
            default_from_flags = Some(id.to_string());
        }

        let mut info = acp::ModelInfo::new(id.to_string(), name.to_string());
        if let Some(desc) = description {
            info = info.description(desc.to_string());
        }
        available.push(info);
    }

    if available.is_empty() {
        return None;
    }

    let current = preferred_model
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .filter(|id| seen_ids.contains(*id))
        .map(|s| s.to_string())
        .or_else(|| {
            extract_current_model_id_from_info(info).filter(|id| seen_ids.contains(id.as_str()))
        })
        .or(current_from_flags)
        .or(default_from_flags)
        .or_else(|| available.first().map(|m| m.model_id.0.as_ref().to_string()))?;

    Some(acp::SessionModelState::new(current, available))
}

fn append_model_entries(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Array(arr) => {
            out.extend(arr.iter().cloned());
        }
        serde_json::Value::Object(obj) => {
            let mut found_nested = false;
            for key in [
                "available",
                "availableModels",
                "available_models",
                "models",
                "items",
                "options",
            ] {
                if let Some(v) = obj.get(key) {
                    found_nested = true;
                    append_model_entries(v, out);
                }
            }

            if !found_nested {
                for (id, entry) in obj {
                    match entry {
                        serde_json::Value::Object(model_obj) => {
                            let mut with_id = model_obj.clone();
                            if !(with_id.contains_key("id")
                                || with_id.contains_key("modelId")
                                || with_id.contains_key("model_id")
                                || with_id.contains_key("value")
                                || with_id.contains_key("name"))
                            {
                                with_id.insert(
                                    "id".to_string(),
                                    serde_json::Value::String(id.to_string()),
                                );
                            }
                            out.push(serde_json::Value::Object(with_id));
                        }
                        serde_json::Value::String(name) => {
                            out.push(serde_json::json!({ "id": id, "name": name }));
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
}

fn extract_current_model_id_from_info(info: &serde_json::Value) -> Option<String> {
    for key in [
        "currentModelId",
        "current_model_id",
        "modelId",
        "model_id",
        "model",
    ] {
        if let Some(raw) = info.get(key)
            && let Some(id) = extract_model_id(raw)
        {
            return Some(id);
        }
    }

    if let Some(models_obj) = info.get("models").and_then(|v| v.as_object()) {
        for key in [
            "currentModelId",
            "current_model_id",
            "modelId",
            "model_id",
            "model",
        ] {
            if let Some(raw) = models_obj.get(key)
                && let Some(id) = extract_model_id(raw)
            {
                return Some(id);
            }
        }
    }

    None
}

fn extract_model_id(value: &serde_json::Value) -> Option<String> {
    if let Some(s) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }

    let obj = value.as_object()?;
    for key in ["value", "modelId", "model_id", "id", "name"] {
        if let Some(s) = obj
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(s.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_models_uses_explicit_current_model_id() {
        let info = serde_json::json!({
            "models": [
                { "value": "claude-opus-4-1", "displayName": "Opus 4.1" },
                { "value": "claude-sonnet-4-5", "displayName": "Sonnet 4.5" }
            ],
            "currentModelId": "claude-sonnet-4-5"
        });

        let state = parse_models_from_server_info(&info, None).expect("models parsed");
        assert_eq!(state.current_model_id.0.as_ref(), "claude-sonnet-4-5");
        assert_eq!(state.available_models.len(), 2);
    }

    #[test]
    fn parse_models_supports_map_style_payload() {
        let info = serde_json::json!({
            "models": {
                "claude-opus-4-1": { "displayName": "Opus 4.1", "isDefault": true },
                "claude-sonnet-4-5": { "displayName": "Sonnet 4.5" }
            }
        });

        let state = parse_models_from_server_info(&info, None).expect("models parsed");
        assert_eq!(state.current_model_id.0.as_ref(), "claude-opus-4-1");
        assert_eq!(state.available_models.len(), 2);
    }

    #[test]
    fn parse_models_prefers_session_selected_model() {
        let info = serde_json::json!({
            "models": [
                { "value": "claude-opus-4-1", "displayName": "Opus 4.1" },
                { "value": "claude-sonnet-4-5", "displayName": "Sonnet 4.5" }
            ],
            "currentModelId": "claude-opus-4-1"
        });

        let state =
            parse_models_from_server_info(&info, Some("claude-sonnet-4-5")).expect("models parsed");
        assert_eq!(state.current_model_id.0.as_ref(), "claude-sonnet-4-5");
    }
}

fn parse_commands_from_server_info(info: &serde_json::Value) -> Vec<acp::AvailableCommand> {
    let Some(cmds) = info.get("commands").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut out: Vec<acp::AvailableCommand> = Vec::new();
    for c in cmds {
        let Some(name) = c.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let description = c
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut cmd = acp::AvailableCommand::new(name.to_string(), description);

        let hint = match c.get("argumentHint").or_else(|| c.get("argument_hint")) {
            Some(v) if v.is_string() => v.as_str().map(|s| s.to_string()),
            Some(v) if v.is_array() => v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
            _ => None,
        };
        if let Some(hint) = hint.filter(|s| !s.is_empty()) {
            cmd = cmd.input(acp::AvailableCommandInput::Unstructured(
                acp::UnstructuredCommandInput::new(hint),
            ));
        }

        out.push(cmd);
    }
    out
}

/// Convert ACP MCP server configurations to SDK format
fn convert_acp_mcp_to_sdk(acp_servers: &[acp::McpServer]) -> McpServers {
    if acp_servers.is_empty() {
        return McpServers::Empty;
    }

    let mut servers = HashMap::new();

    for server in acp_servers {
        match server {
            acp::McpServer::Stdio(stdio) => {
                let name = stdio.name.clone();
                let command = stdio.command.to_string_lossy().to_string();
                let args = if stdio.args.is_empty() {
                    None
                } else {
                    Some(stdio.args.clone())
                };

                // Convert env variables
                let env = if stdio.env.is_empty() {
                    None
                } else {
                    let env_map: HashMap<String, String> = stdio
                        .env
                        .iter()
                        .map(|v| (v.name.clone(), v.value.clone()))
                        .collect();
                    Some(env_map)
                };

                info!(
                    "Converting MCP server '{}': command={}, args={:?}",
                    name, command, args
                );

                let config = McpServerConfig::Stdio(McpStdioServerConfig { command, args, env });
                servers.insert(name, config);
            }
            // HTTP and SSE are not currently needed for seren-mcp stdio mode
            acp::McpServer::Http(http) => {
                info!("Skipping HTTP MCP server '{}' (not supported)", http.name);
            }
            acp::McpServer::Sse(sse) => {
                info!("Skipping SSE MCP server '{}' (not supported)", sse.name);
            }
            // Handle any future server types
            _ => {
                info!("Skipping unknown MCP server type");
            }
        }
    }

    if servers.is_empty() {
        McpServers::Empty
    } else {
        McpServers::Dict(servers)
    }
}

/// Run the ACP server on stdio
pub async fn run_acp_server() -> Result<(), acp::Error> {
    let _ = env_logger::try_init();

    let outgoing = tokio::io::stdout().compat_write();
    let incoming = tokio::io::stdin().compat();

    // Use LocalSet for non-Send futures
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let (tx, mut rx) = mpsc::unbounded_channel::<AcpOutbound>();

            // Create agent and connection
            let (conn, handle_io) =
                acp::AgentSideConnection::new(ClaudeAgent::new(tx), outgoing, incoming, |fut| {
                    tokio::task::spawn_local(fut);
                });

            // Background task to send notifications / permission requests
            tokio::task::spawn_local(async move {
                while let Some(msg) = rx.recv().await {
                    match msg {
                        AcpOutbound::SessionNotification(notification) => {
                            if let Err(e) = conn.session_notification(notification).await {
                                error!("Failed to send notification: {}", e);
                                break;
                            }
                        }
                        AcpOutbound::RequestPermission {
                            request,
                            response_tx,
                        } => {
                            let res = conn.request_permission(request).await;
                            let _ = response_tx.send(res);
                        }
                    }
                }
            });

            // Run until stdio closes
            handle_io.await
        })
        .await
}
