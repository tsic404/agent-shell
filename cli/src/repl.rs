//! 交互式 REPL 模式（§17.2）。
//!
//! 当 CLI 未提供子命令时进入 REPL：逐行读取命令，解析为 shell words，
//! 当作 CLI 参数分派执行。`exit`/`quit` 或 EOF 退出。

use crate::cli::{Cli, OutputFormat};
use crate::{dispatch_command, CmdResult};
use clap::Parser;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::path::PathBuf;

/// 进入 REPL 循环。返回退出码。`socket` 为显式 daemon 端点（同单次执行，§17.2）。
pub async fn run_repl(out: OutputFormat, retry: u32, socket: Option<PathBuf>) -> CmdResult {
    let mut rl = DefaultEditor::new().map_err(|e| e.to_string())?;
    println!("agent-shell REPL — type 'exit' or 'quit' to leave.");

    loop {
        let line = match rl.readline("agent-shell> ") {
            Ok(line) => line,
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break Ok(0),
            Err(e) => break Err(e.to_string()),
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if matches!(trimmed, "exit" | "quit") {
            break Ok(0);
        }

        // 将输入行追加到历史，便于上下键回溯。
        let _ = rl.add_history_entry(&line);

        // 解析为 shell words，前置程序名以满足 clap 解析。
        let words = match shell_words::split(&line) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("parse error: {e}");
                continue;
            }
        };

        let mut argv: Vec<std::ffi::OsString> = vec!["agent-shell".into()];
        argv.extend(words.into_iter().map(Into::into));

        match Cli::try_parse_from(argv.clone()) {
            Ok(args) => match args.command {
                Some(command) => {
                    let socket = command_socket(args.socket, socket.as_ref());
                    if let Err(e) = dispatch_command(command, out, retry, socket).await {
                        eprintln!("error: {e}");
                    }
                }
                None => eprintln!("nested REPL not supported — provide a subcommand"),
            },
            Err(e) => {
                eprintln!("{e}");
                if let Some(hint) = crate::cli::at_syntax_hint(&argv, &e) {
                    eprintln!("\nhint: {hint}");
                }
            }
        }
    }
}

/// 嵌套命令的 daemon 端点：该行命令自己的 `--socket`/`AGENT_SHELL_SOCKET`
/// （clap 已解析进 `parsed`）优先，其后才是 REPL 启动时的端点。
///
/// 早前实现无条件传启动端点，导致 REPL 内 `--socket /other.sock info` 静默连到
/// 另一个 daemon。
fn command_socket(parsed: Option<PathBuf>, outer: Option<&PathBuf>) -> Option<PathBuf> {
    crate::socket_endpoint(parsed).or_else(|| outer.cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_socket_prefers_the_lines_own_endpoint() {
        let line = Some(PathBuf::from("/tmp/line.sock"));
        let outer = Some(PathBuf::from("/tmp/outer.sock"));
        assert_eq!(
            command_socket(line, outer.as_ref()),
            Some(PathBuf::from("/tmp/line.sock"))
        );
    }

    #[test]
    fn command_socket_falls_back_to_the_repl_endpoint() {
        let outer = Some(PathBuf::from("/tmp/outer.sock"));
        assert_eq!(
            command_socket(None, outer.as_ref()),
            Some(PathBuf::from("/tmp/outer.sock"))
        );
        assert_eq!(command_socket(Some(PathBuf::new()), outer.as_ref()), outer);
        assert_eq!(command_socket(None, None), None);
    }
}
