//! Native Windows fixture for `acp_projected_local_e2e.mjs`.
//!
//! `std::process::Command` and the Workdir provider intentionally do not use a
//! command shell or PATHEXT expansion. The E2E therefore compiles this tiny
//! dependency-free JSON-RPC peer as both `gemini.exe` and `npx.exe`.

use std::io::{self, BufRead, Write};

fn escaped(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn prefix(value: &str) -> &str {
    value.get(..6).unwrap_or(value)
}

fn main() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let codex = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("npx"));
    let session = if codex {
        "codex-session"
    } else {
        "projected-session"
    };
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.contains("\"method\":\"initialize\"") {
            writeln!(
                stdout,
                r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"agentCapabilities":{{}}}}}}"#
            )?;
        } else if line.contains("\"method\":\"session/new\"") {
            writeln!(
                stdout,
                r#"{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{session}"}}}}"#
            )?;
        } else if line.contains("\"method\":\"session/prompt\"") {
            let text = if codex {
                let home = env("CODEX_HOME");
                let config = std::path::Path::new(&home).join("config.toml").is_file();
                format!(
                    "CODEX_PROJECTED base={} key={} home={} config={}",
                    env("OPENAI_BASE_URL"),
                    prefix(&env("OPENAI_API_KEY")),
                    home,
                    if config { "yes" } else { "no" }
                )
            } else {
                format!(
                    "PROJECTED base={} model={} key={} cwd={} home={}",
                    env("GOOGLE_GEMINI_BASE_URL"),
                    env("GEMINI_MODEL"),
                    prefix(&env("GEMINI_API_KEY")),
                    std::env::current_dir()?.display(),
                    env("GEMINI_DIR")
                )
            };
            writeln!(
                stdout,
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{session}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{}"}}}}}}}}"#,
                escaped(text)
            )?;
            writeln!(
                stdout,
                r#"{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}"#
            )?;
            stdout.flush()?;
            return Ok(());
        }
        stdout.flush()?;
    }
    Ok(())
}
