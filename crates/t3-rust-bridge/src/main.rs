mod bridge;

use std::io::Read;
use std::sync::Arc;

use bridge::{acp_mcp_call, dedup_structured_content, js_stringify, run_bridge};
use serde_json::Value;
use tokio::sync::Mutex;

const MAX_ARGUMENTS_BYTES: u64 = 64 * 1024 * 1024;

const USAGE: &str = "Usage: t3-rust-bridge <command> [args]\n\
\n\
Commands:\n\
  acp-mcp-bridge                              Bridge T3 Code's MCP endpoint to stdio\n\
  acp-mcp-call <tool> <arguments-json | ->    Call one T3 Code MCP tool\n\
\n\
Options:\n\
  -h, --help       Print this help\n\
  -V, --version    Print version\n";

fn require_env(command: &str) -> Option<(String, String)> {
    let endpoint = std::env::var("T3_ACP_MCP_ENDPOINT").ok();
    let authorization = std::env::var("T3_ACP_MCP_AUTHORIZATION").ok();
    match (endpoint, authorization) {
        (Some(endpoint), Some(authorization)) => Some((endpoint, authorization)),
        _ => {
            eprintln!("{command} requires T3_ACP_MCP_ENDPOINT and T3_ACP_MCP_AUTHORIZATION.");
            None
        }
    }
}

async fn cmd_bridge() -> i32 {
    let Some((endpoint, authorization)) = require_env("acp-mcp-bridge") else {
        return 2;
    };
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    let output = Arc::new(Mutex::new(tokio::io::stdout()));
    run_bridge(endpoint, authorization, input, output).await;
    0
}

async fn cmd_call(args: &[String]) -> i32 {
    let Some((endpoint, authorization)) = require_env("acp-mcp-call") else {
        return 2;
    };
    let (Some(tool), Some(arguments_arg)) = (args.first(), args.get(1)) else {
        eprintln!("acp-mcp-call requires <tool> and <arguments-json>, or - to read it from stdin.");
        return 2;
    };
    // PowerShell strips JSON quotes on the command line; stdin keeps them.
    let arguments_json = if arguments_arg == "-" {
        let mut buf = Vec::new();
        let read = std::io::stdin()
            .take(MAX_ARGUMENTS_BYTES + 1)
            .read_to_end(&mut buf);
        if read.is_err() {
            eprintln!("acp-mcp-call failed to read arguments from stdin.");
            return 2;
        }
        if buf.len() as u64 > MAX_ARGUMENTS_BYTES {
            eprintln!(
                "acp-mcp-call arguments exceed {} bytes.",
                MAX_ARGUMENTS_BYTES
            );
            return 2;
        }
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        arguments_arg.clone()
    };
    let parsed: Value = match serde_json::from_str(&arguments_json) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("acp-mcp-call arguments must be valid JSON.");
            return 2;
        }
    };
    if !parsed.is_object() {
        eprintln!("acp-mcp-call arguments must be a JSON object.");
        return 2;
    }
    match acp_mcp_call(&endpoint, &authorization, tool, parsed).await {
        Ok(Some(mut result)) => {
            dedup_structured_content(&mut result);
            println!("{}", js_stringify(&result));
            0
        }
        // `JSON.stringify(undefined)` produces no string; the TS fast path
        // interpolates it and prints "undefined".
        Ok(None) => {
            println!("undefined");
            0
        }
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

fn unicode_args() -> Result<Vec<String>, std::ffi::OsString> {
    std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect()
}

fn main() {
    let args = match unicode_args() {
        Ok(args) => args,
        Err(_) => {
            eprintln!("t3-rust-bridge: command arguments must be valid Unicode");
            std::process::exit(2);
        }
    };
    let command = args.first().map(String::as_str);
    let code = match command {
        Some("-h") | Some("--help") => {
            print!("{USAGE}");
            0
        }
        Some("-V") | Some("--version") => {
            println!("t3-rust-bridge {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("acp-mcp-bridge") | Some("acp-mcp-call") => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            if command == Some("acp-mcp-bridge") {
                runtime.block_on(cmd_bridge())
            } else {
                runtime.block_on(cmd_call(&args[1..]))
            }
        }
        Some(other) => {
            eprintln!("t3-rust-bridge: unknown command `{other}`");
            eprint!("{USAGE}");
            2
        }
        None => {
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
