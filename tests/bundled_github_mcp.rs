//! Exercise the installed executable's MCP subcommand without GitHub credentials.
use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn bundled_server_initializes_lists_tools_and_enforces_read_only_without_credentials() {
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"push_files","arguments":{}}}),
    ];
    let mut child = Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["github-mcp", "--read-only"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0]["result"]["serverInfo"]["name"],
        "fridica-github-mcp"
    );
    let names: Vec<_> = rows[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"pull_request_read"));
    assert!(names.contains(&"git_context"));
    assert!(!names.contains(&"push_files"));
    assert_eq!(rows[2]["error"]["code"], -32602);
}
