//! Integration smoke test for `keron lsp`.
//!
//! Spawns the binary, drives a minimal `initialize → didOpen → wait
//! for diagnostics → shutdown` round-trip over stdio. The point is
//! to catch regressions in the message-loop wiring; the diagnostic
//! content itself is exercised by `keron-lang`'s unit and corpus
//! tests.

#![allow(unreachable_pub)]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_keron");

fn write_msg(stdin: &mut ChildStdin, msg: &serde_json::Value) {
    let body = serde_json::to_vec(msg).expect("serialize message");
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
    stdin.write_all(&body).expect("write body");
    stdin.flush().expect("flush stdin");
}

fn read_msg(stdout: &mut BufReader<ChildStdout>) -> serde_json::Value {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = stdout.read_line(&mut line).expect("read header line");
        assert!(n > 0, "EOF while reading LSP headers");
        if line == "\r\n" {
            break;
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse().expect("parse Content-Length"));
        }
    }
    let len = content_length.expect("missing Content-Length header");
    let mut buf = vec![0u8; len];
    stdout.read_exact(&mut buf).expect("read message body");
    serde_json::from_slice(&buf).expect("parse JSON body")
}

#[test]
fn lsp_publishes_diagnostics_for_bad_source() {
    let mut child = Command::new(BIN)
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `keron lsp`");

    let mut stdin = child.stdin.take().expect("stdin pipe");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));

    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "capabilities": {},
                "processId": null,
                "rootUri": null,
                "clientInfo": {"name": "lsp_stdio test"}
            }
        }),
    );
    let init_resp = read_msg(&mut stdout);
    assert_eq!(init_resp["id"], 1);
    assert!(
        init_resp["result"].is_object(),
        "expected init result, got {init_resp}"
    );

    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    );

    let uri = "file:///tmp/keron_lsp_smoke.keron";
    let bad_src = r"val s: Symlink = 42";
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "keron",
                    "version": 1,
                    "text": bad_src
                }
            }
        }),
    );

    let diagnostics = loop {
        let msg = read_msg(&mut stdout);
        if msg["method"] == "textDocument/publishDiagnostics" {
            break msg["params"]["diagnostics"].clone();
        }
    };
    let arr = diagnostics
        .as_array()
        .expect("publishDiagnostics carries an array");
    assert!(!arr.is_empty(), "expected ≥1 diagnostic, got {arr:?}");
    let msg_text = arr[0]["message"]
        .as_str()
        .expect("diagnostic message is a string");
    assert!(
        msg_text.contains("Symlink") || msg_text.contains("Int"),
        "unexpected diagnostic: {msg_text}"
    );

    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": null
        }),
    );
    let _ = read_msg(&mut stdout);
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit"
        }),
    );
    // Close the pipe so the LSP's stdin reader sees EOF and the IO
    // threads can join. Without this, `child.wait()` blocks forever.
    drop(stdin);

    let status = child.wait().expect("wait on child");
    assert!(status.success(), "`keron lsp` exited non-zero: {status}");
}
