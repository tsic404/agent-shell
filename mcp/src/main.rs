//! MCP server 入口——stdio 传输，18 tools registered。

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "agent_shell_mcp=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = agent_shell_mcp::server::run_stdio().await {
        eprintln!("MCP server error: {e}");
        std::process::exit(1);
    }
}
