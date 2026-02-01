// ABOUTME: Entry point for seren-acp-claude binary
// ABOUTME: Bridges ACP protocol to Claude Code CLI

use seren_acp_claude::acp::run_acp_server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_acp_server().await?;
    Ok(())
}
