// ABOUTME: Claude Code CLI client wrapper
// ABOUTME: Uses claude-code-agent-sdk to spawn and communicate with claude CLI

use crate::acp::AcpOutbound;
use crate::error::{Error, Result};
use agent_client_protocol as acp;
use claude_agent_sdk_rs::{
    CanUseToolCallback, ClaudeAgentOptions, McpServers, Message, PermissionMode, PermissionResult,
    PermissionResultAllow, PermissionResultDeny, UserContentBlock,
};
use futures::StreamExt;
use log::{debug, info};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};

/// Claude Code CLI client
///
/// Wraps the SDK client with a Mutex since query() requires &mut self.
/// The streaming architecture forwards messages via a broadcast channel.
pub struct ClaudeClient {
    /// SDK client (protected by Mutex for interior mutability)
    client: Arc<Mutex<Option<claude_agent_sdk_rs::ClaudeClient>>>,
    /// Working directory
    cwd: PathBuf,
    /// Claude Code session ID (persisted by the CLI on disk).
    claude_session_id: String,
    /// If set, connect() will resume this Claude Code session id (`claude --resume <id>`).
    resume_session_id: Option<String>,
    /// If true, connect() will fork the resumed session (`claude --fork-session`).
    ///
    /// When used together with `resume_session_id`, we also force a new stable `--session-id`
    /// so the fork persists as a separate session on disk.
    fork_session: bool,
    /// ACP session ID for permission requests / notifications
    session_id: acp::SessionId,
    /// Channel to the ACP server for outbound messages (permission prompts, tool status updates)
    acp_tx: mpsc::UnboundedSender<AcpOutbound>,
    /// Current session permission mode
    permission_mode: Arc<RwLock<PermissionMode>>,
    /// Tool names allowed for the remainder of the session
    allowed_tools: Arc<RwLock<HashSet<String>>>,
    /// Broadcast channel for streaming messages to subscribers
    message_tx: broadcast::Sender<Message>,
    /// Cancel flag
    cancelled: Arc<Mutex<bool>>,
    /// MCP servers configuration
    mcp_servers: McpServers,
}

impl ClaudeClient {
    /// Create a new Claude client for the given working directory
    pub(crate) fn new(
        cwd: PathBuf,
        claude_session_id: String,
        resume_session_id: Option<String>,
        fork_session: bool,
        session_id: acp::SessionId,
        acp_tx: mpsc::UnboundedSender<AcpOutbound>,
        mcp_servers: McpServers,
    ) -> Self {
        let (message_tx, _) = broadcast::channel::<Message>(100);
        Self {
            client: Arc::new(Mutex::new(None)),
            cwd,
            claude_session_id,
            resume_session_id,
            fork_session,
            session_id,
            acp_tx,
            permission_mode: Arc::new(RwLock::new(PermissionMode::Default)),
            allowed_tools: Arc::new(RwLock::new(HashSet::new())),
            message_tx,
            cancelled: Arc::new(Mutex::new(false)),
            mcp_servers,
        }
    }
    pub async fn set_session_mode(&self, mode_id: &str) -> Result<()> {
        let mode = match mode_id {
            "default" => PermissionMode::Default,
            "acceptEdits" => PermissionMode::AcceptEdits,
            "plan" => PermissionMode::Plan,
            "bypassPermissions" => PermissionMode::BypassPermissions,
            other => {
                return Err(Error::ClaudeConnection(format!(
                    "Unsupported session mode: {other}"
                )));
            }
        };

        let mut guard = self.permission_mode.write().await;
        *guard = mode;
        Ok(())
    }

    /// Connect to Claude CLI
    pub async fn connect(&self) -> Result<()> {
        info!("Connecting to Claude CLI in {:?}", self.cwd);

        let permission_mode = Arc::clone(&self.permission_mode);
        let allowed_tools = Arc::clone(&self.allowed_tools);
        let acp_tx = self.acp_tx.clone();
        let session_id = self.session_id.clone();

        let can_use_tool: CanUseToolCallback = Arc::new(move |tool_name, input, context| {
            let permission_mode = Arc::clone(&permission_mode);
            let allowed_tools = Arc::clone(&allowed_tools);
            let acp_tx = acp_tx.clone();
            let session_id = session_id.clone();

            Box::pin(async move {
                // If we're bypassing permissions, always allow.
                let mode = {
                    let guard = permission_mode.read().await;
                    *guard
                };
                if mode == PermissionMode::BypassPermissions {
                    return PermissionResult::Allow(PermissionResultAllow {
                        updated_input: Some(input.clone()),
                        ..Default::default()
                    });
                }

                // If the tool is already allowed for this session, allow without prompting.
                let already_allowed = {
                    let guard = allowed_tools.read().await;
                    guard.contains(&tool_name)
                };
                if already_allowed {
                    return PermissionResult::Allow(PermissionResultAllow {
                        updated_input: Some(input.clone()),
                        ..Default::default()
                    });
                }

                // Prompt the ACP client for approval.
                let tool_use_id = context
                    .tool_use_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let tool_call_id = acp::ToolCallId::new(format!("claude-tool:{tool_use_id}"));

                let tool_call = acp::ToolCallUpdate::new(
                    tool_call_id.clone(),
                    acp::ToolCallUpdateFields::new()
                        .title(tool_name.clone())
                        .kind(tool_kind_for_claude_tool(&tool_name))
                        .status(acp::ToolCallStatus::Pending)
                        .raw_input(input.clone()),
                );

                let options = vec![
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("allow-once"),
                        "Allow once",
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("allow-session"),
                        "Allow for session",
                        acp::PermissionOptionKind::AllowAlways,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("reject"),
                        "Reject",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("cancel"),
                        "Reject and cancel turn",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                ];

                let request =
                    acp::RequestPermissionRequest::new(session_id.clone(), tool_call, options);
                let response = match request_permission(&acp_tx, request).await {
                    Ok(res) => res,
                    Err(e) => {
                        return PermissionResult::Deny(PermissionResultDeny {
                            message: format!("Permission request failed: {e}"),
                            interrupt: true,
                        });
                    }
                };

                let decision = match response.outcome {
                    acp::RequestPermissionOutcome::Cancelled => PermissionDecision::CancelTurn,
                    acp::RequestPermissionOutcome::Selected(selected) => {
                        match selected.option_id.0.as_ref() {
                            "allow-once" => PermissionDecision::AllowOnce,
                            "allow-session" => PermissionDecision::AllowForSession,
                            "reject" => PermissionDecision::RejectOnce,
                            "cancel" => PermissionDecision::CancelTurn,
                            _ => PermissionDecision::CancelTurn,
                        }
                    }
                    _ => PermissionDecision::CancelTurn,
                };

                // Update tool call status so clients don't leave it in "Pending" forever.
                let status = match decision {
                    PermissionDecision::AllowOnce | PermissionDecision::AllowForSession => {
                        acp::ToolCallStatus::InProgress
                    }
                    PermissionDecision::RejectOnce | PermissionDecision::CancelTurn => {
                        acp::ToolCallStatus::Failed
                    }
                };
                let status_update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    tool_call_id.clone(),
                    acp::ToolCallUpdateFields::new().status(status),
                ));
                let _ = acp_tx.send(AcpOutbound::SessionNotification(
                    acp::SessionNotification::new(session_id.clone(), status_update),
                ));

                match decision {
                    PermissionDecision::AllowOnce => {
                        PermissionResult::Allow(PermissionResultAllow {
                            updated_input: Some(input.clone()),
                            ..Default::default()
                        })
                    }
                    PermissionDecision::AllowForSession => {
                        {
                            let mut guard = allowed_tools.write().await;
                            guard.insert(tool_name.clone());
                        }
                        PermissionResult::Allow(PermissionResultAllow {
                            updated_input: Some(input.clone()),
                            ..Default::default()
                        })
                    }
                    PermissionDecision::RejectOnce => {
                        PermissionResult::Deny(PermissionResultDeny {
                            message: "Tool use denied".to_string(),
                            interrupt: false,
                        })
                    }
                    PermissionDecision::CancelTurn => {
                        PermissionResult::Deny(PermissionResultDeny {
                            message: "Turn cancelled".to_string(),
                            interrupt: true,
                        })
                    }
                }
            })
        });

        let mut options = ClaudeAgentOptions::builder()
            .cwd(self.cwd.clone())
            .permission_mode(PermissionMode::Default)
            .can_use_tool(can_use_tool)
            .include_partial_messages(true)
            .max_thinking_tokens(31999)
            .mcp_servers(self.mcp_servers.clone())
            .fork_session(self.fork_session)
            .build();

        if let Some(ref resume_id) = self.resume_session_id {
            options.resume = Some(resume_id.clone());
        }

        if self.resume_session_id.is_none() || self.fork_session {
            // Ensure we can later `load_session` by using a stable, caller-defined session id.
            // Claude Code persists sessions by id on disk (see `~/.claude/projects/...`).
            options.extra_args.insert(
                "session-id".to_string(),
                Some(self.claude_session_id.clone()),
            );
        }

        let mut sdk_client = claude_agent_sdk_rs::ClaudeClient::new(options);
        sdk_client.connect().await.map_err(Error::from)?;

        let mut guard = self.client.lock().await;
        *guard = Some(sdk_client);

        info!("Connected to Claude CLI");
        Ok(())
    }

    /// Check if connected
    pub async fn is_connected(&self) -> bool {
        let guard = self.client.lock().await;
        guard.is_some()
    }

    pub async fn set_model(&self, model: Option<&str>) -> Result<()> {
        let guard = self.client.lock().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| Error::ClaudeConnection("Not connected".to_string()))?;
        client.set_model(model).await.map_err(Error::from)?;
        Ok(())
    }

    pub async fn get_server_info(&self) -> Option<serde_json::Value> {
        let guard = self.client.lock().await;
        guard.as_ref().and_then(|c| c.get_server_info())
    }

    /// Send a query to Claude and stream responses
    ///
    /// This method holds the client lock for the duration of the query
    /// because the SDK's receive_response() returns a stream with a
    /// lifetime tied to the client reference.
    pub async fn query(&self, text: &str) -> Result<()> {
        debug!(
            "Sending query to Claude: {}",
            text.chars().take(100).collect::<String>()
        );

        // Reset cancelled flag
        {
            let mut cancelled = self.cancelled.lock().await;
            *cancelled = false;
        }

        // Acquire lock for the entire query + streaming operation
        let mut guard = self.client.lock().await;
        let client = guard
            .as_mut()
            .ok_or_else(|| Error::ClaudeConnection("Not connected".to_string()))?;

        // Send the query
        client.query(text).await.map_err(Error::from)?;

        // Apply the current session permission mode to this query (if the client supports it).
        let mode = {
            let guard = self.permission_mode.read().await;
            *guard
        };
        if let Err(e) = client.set_permission_mode(mode).await {
            log::warn!("Failed to set Claude permission mode: {}", e);
        }

        // Get response stream and process
        let message_tx = self.message_tx.clone();
        let cancelled = self.cancelled.clone();
        let mut stream = client.receive_response();

        while let Some(result) = stream.next().await {
            // Check for cancellation
            {
                let is_cancelled = *cancelled.lock().await;
                if is_cancelled {
                    info!("Query cancelled");
                    break;
                }
            }

            match result {
                Ok(message) => {
                    debug!(
                        "Received message from Claude: {:?}",
                        std::mem::discriminant(&message)
                    );
                    let is_result = matches!(message, Message::Result(_));
                    let _ = message_tx.send(message);
                    if is_result {
                        break;
                    }
                }
                Err(e) => {
                    return Err(Error::from(e));
                }
            }
        }

        Ok(())
    }

    /// Send a query with structured content blocks (supports text + images)
    pub async fn query_with_content(&self, content: Vec<UserContentBlock>) -> Result<()> {
        debug!("Sending query with {} content blocks to Claude", content.len());

        // Reset cancelled flag
        {
            let mut cancelled = self.cancelled.lock().await;
            *cancelled = false;
        }

        // Acquire lock for the entire query + streaming operation
        let mut guard = self.client.lock().await;
        let client = guard
            .as_mut()
            .ok_or_else(|| Error::ClaudeConnection("Not connected".to_string()))?;

        // Send the query with content blocks
        client.query_with_content(content).await.map_err(Error::from)?;

        // Apply the current session permission mode
        let mode = {
            let guard = self.permission_mode.read().await;
            *guard
        };
        if let Err(e) = client.set_permission_mode(mode).await {
            log::warn!("Failed to set Claude permission mode: {}", e);
        }

        // Get response stream and process
        let message_tx = self.message_tx.clone();
        let cancelled = self.cancelled.clone();
        let mut stream = client.receive_response();

        while let Some(result) = stream.next().await {
            // Check for cancellation
            {
                let is_cancelled = *cancelled.lock().await;
                if is_cancelled {
                    info!("Query cancelled");
                    break;
                }
            }

            match result {
                Ok(message) => {
                    debug!(
                        "Received message from Claude: {:?}",
                        std::mem::discriminant(&message)
                    );
                    let is_result = matches!(message, Message::Result(_));
                    let _ = message_tx.send(message);
                    if is_result {
                        break;
                    }
                }
                Err(e) => {
                    return Err(Error::from(e));
                }
            }
        }

        Ok(())
    }

    /// Subscribe to message stream
    pub fn subscribe(&self) -> broadcast::Receiver<Message> {
        self.message_tx.subscribe()
    }

    /// Cancel the current query
    pub async fn cancel(&self) {
        let mut cancelled = self.cancelled.lock().await;
        *cancelled = true;

        // Try to interrupt the SDK client
        if let Some(ref client) = *self.client.lock().await {
            if let Err(e) = client.interrupt().await {
                log::warn!("Failed to interrupt Claude CLI: {}", e);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PermissionDecision {
    AllowOnce,
    AllowForSession,
    RejectOnce,
    CancelTurn,
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

async fn request_permission(
    acp_tx: &mpsc::UnboundedSender<AcpOutbound>,
    request: acp::RequestPermissionRequest,
) -> std::result::Result<acp::RequestPermissionResponse, String> {
    let (response_tx, response_rx) = oneshot::channel();

    acp_tx
        .send(AcpOutbound::RequestPermission {
            request,
            response_tx,
        })
        .map_err(|_| "ACP outbound channel closed".to_string())?;

    response_rx
        .await
        .map_err(|_| "ACP permission request cancelled".to_string())?
        .map_err(|e| e.to_string())
}
