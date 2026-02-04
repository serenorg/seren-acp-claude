// ABOUTME: ACP server implementation using agent-client-protocol trait API
// ABOUTME: Implements acp::Agent trait to bridge ACP to Claude Code CLI

use super::AcpOutbound;
use crate::claude::ClaudeClient;
use agent_client_protocol::{self as acp, Client as _};
use claude_agent_sdk_rs::types::mcp::McpStdioServerConfig;
use claude_agent_sdk_rs::{ContentBlock, McpServerConfig, McpServers, Message, ToolResultContent};

use log::{debug, error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Session data mapping ACP session to Claude client
struct SessionData {
    client: Arc<ClaudeClient>,
    #[allow(dead_code)]
    cwd: String,
    mode_id: String,
}

/// Claude ACP Agent - bridges ACP protocol to Claude Code CLI
pub struct ClaudeAgent {
    /// Channel for sending outbound messages (notifications / permission requests) to the ACP client
    acp_tx: mpsc::UnboundedSender<AcpOutbound>,
    /// Session mappings: session_id -> SessionData
    sessions: RwLock<HashMap<String, SessionData>>,
    /// Next session ID counter
    next_session_id: RwLock<u64>,
}

impl ClaudeAgent {
    /// Create a new Claude agent
    pub fn new(acp_tx: mpsc::UnboundedSender<AcpOutbound>) -> Self {
        Self {
            acp_tx,
            sessions: RwLock::new(HashMap::new()),
            next_session_id: RwLock::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ClaudeAgent {
    async fn initialize(
        &self,
        request: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        info!("Received initialize request");

        // Build capabilities
        let prompt_caps = acp::PromptCapabilities::new()
            .embedded_context(true)
            .image(true);
        let agent_caps = acp::AgentCapabilities::new().prompt_capabilities(prompt_caps);

        // Build agent info
        let agent_info = acp::Implementation::new("seren-acp-claude", env!("CARGO_PKG_VERSION"))
            .title("Claude Code".to_string());

        Ok(acp::InitializeResponse::new(request.protocol_version)
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
        info!(
            "MCP servers from client: {}",
            request.mcp_servers.len()
        );

        // Convert ACP MCP servers to SDK format
        let mcp_servers = convert_acp_mcp_to_sdk(&request.mcp_servers);

        // Generate session ID
        let session_id = {
            let mut counter = self.next_session_id.write().await;
            let id = *counter;
            *counter += 1;
            format!("session-{}", id)
        };

        // Create Claude client for this session
        let client = Arc::new(ClaudeClient::new(
            request.cwd.clone(),
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

        // Store session mapping
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_id.clone(),
                SessionData {
                    client,
                    cwd,
                    mode_id: "default".to_string(),
                },
            );
        }

        // Build session modes
        let default_mode = acp::SessionMode::new("default", "Default")
            .description("Standard behavior".to_string());
        let bypass_mode = acp::SessionMode::new("bypassPermissions", "Bypass Permissions")
            .description("Auto-approve all operations".to_string());

        let mode_state = acp::SessionModeState::new("default", vec![default_mode, bypass_mode]);

        Ok(acp::NewSessionResponse::new(session_id).modes(mode_state))
    }

    async fn load_session(
        &self,
        _request: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        info!("Received load session request");
        // Session loading not supported
        Err(acp::Error::new(-32603, "Session loading not supported"))
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

        // Extract prompt text from content blocks
        let prompt_text: String = request
            .prompt
            .iter()
            .filter_map(|block| {
                if let acp::ContentBlock::Text(text) = block {
                    Some(text.text.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        if prompt_text.is_empty() {
            return Err(acp::Error::new(-32602, "No text content in prompt"));
        }

        debug!(
            "Sending prompt to Claude: {}",
            prompt_text.chars().take(100).collect::<String>()
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

        // Send query to Claude
        client.query(&prompt_text).await.map_err(|e| {
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

        if mode_id != "default" && mode_id != "bypassPermissions" {
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
