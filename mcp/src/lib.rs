//! agent-shell MCP server (§17.3).
//!
//! 基于 rmcp 的 MCP server，注册 18 个桌面操作工具。
//! 工具调用经 JSON-RPC 转发到 agent-shell daemon 执行。

pub mod server;
