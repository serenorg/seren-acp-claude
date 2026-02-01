// ABOUTME: ACP server module for Claude agent
// ABOUTME: Implements agent-client-protocol trait API

mod server;

use agent_client_protocol as acp_sdk;
use tokio::sync::oneshot;

pub use server::run_acp_server;

#[derive(Debug)]
pub(crate) enum AcpOutbound {
    SessionNotification(acp_sdk::SessionNotification),
    RequestPermission {
        request: acp_sdk::RequestPermissionRequest,
        response_tx: oneshot::Sender<Result<acp_sdk::RequestPermissionResponse, acp_sdk::Error>>,
    },
}
