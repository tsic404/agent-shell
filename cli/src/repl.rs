//! 交互式 REPL 模式（§17.2）。
//!
//! 当 CLI 未提供子命令时进入 REPL：逐行读取命令，解析为 shell words，
//! 当作 CLI 参数分派执行。`exit`/`quit` 或 EOF 退出。

use crate::cli::{Cli, OutputFormat};
use crate::{dispatch_command, CmdResult};
use clap::Parser;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

/// 进入 REPL 循环。返回退出码。
pub async fn run_repl(out: OutputFormat, retry: u32) -> CmdResult {
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
                    if let Err(e) = dispatch_command(command, out, retry).await {
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
