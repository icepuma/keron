//! End-to-end LSP tests: drive `run_with_connection` over an
//! in-memory transport exactly the way an editor would over stdio —
//! initialize handshake, didOpen/didChange, feature requests,
//! shutdown — and assert on the wire-level payloads.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::notification::{DidChangeTextDocument, DidOpenTextDocument, Notification as _};
use lsp_types::request::{
    Formatting, GotoDefinition, HoverRequest, Initialize, Request as _, SemanticTokensFullRequest,
    Shutdown,
};
use lsp_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    FormattingOptions, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, InitializeParams, Position, PublishDiagnosticsParams, SemanticTokens,
    SemanticTokensParams, TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri, VersionedTextDocumentIdentifier, WorkDoneProgressParams,
};

const TIMEOUT: Duration = Duration::from_secs(10);

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempProject {
    root: PathBuf,
}

impl TempProject {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("keron-lsp-e2e-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp dir");
        Self {
            root: fs::canonicalize(&root).expect("canonicalize temp dir"),
        }
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::write(&path, content).expect("write file");
        path
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn file_uri(path: &Path) -> Uri {
    // Editor-style file URI: forward slashes, no Windows `\\?\`
    // verbatim prefix, and a leading `/` before the drive letter.
    let s = path.display().to_string();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
    let root = if s.starts_with('/') { "" } else { "/" };
    Uri::from_str(&format!("file://{root}{s}")).expect("temp paths are uri-safe ascii")
}

/// A minimal LSP client over `Connection::memory()`. Notifications
/// that arrive while waiting for a response are buffered so tests can
/// assert on them later, in order.
struct Client {
    conn: Connection,
    server: Option<std::thread::JoinHandle<()>>,
    pending: VecDeque<Notification>,
    next_id: i32,
}

impl Client {
    fn start() -> Self {
        Self::start_with(InitializeParams::default()).0
    }

    /// Start a server and run the initialize handshake with custom
    /// client params; returns the raw initialize result too.
    fn start_with(init: InitializeParams) -> (Self, serde_json::Value) {
        let (server_side, client_side) = Connection::memory();
        let server = std::thread::spawn(move || {
            keron_lsp::run_with_connection(&server_side).expect("server run");
        });
        let mut client = Self {
            conn: client_side,
            server: Some(server),
            pending: VecDeque::new(),
            next_id: 0,
        };
        let result = client.request(Initialize::METHOD, init);
        client.notify("initialized", serde_json::json!({}));
        (client, result)
    }

    fn request<P: serde::Serialize>(&mut self, method: &str, params: P) -> serde_json::Value {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.conn
            .sender
            .send(Message::Request(Request::new(
                id.clone(),
                method.to_string(),
                params,
            )))
            .expect("send request");
        loop {
            match self.recv() {
                Message::Response(resp) => {
                    assert_eq!(resp.id, id, "responses arrive in order");
                    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
                    return resp.result.unwrap_or(serde_json::Value::Null);
                }
                Message::Notification(n) => self.pending.push_back(n),
                Message::Request(r) => panic!("server sent unexpected request {}", r.method),
            }
        }
    }

    fn notify<P: serde::Serialize>(&self, method: &str, params: P) {
        self.conn
            .sender
            .send(Message::Notification(Notification::new(
                method.to_string(),
                params,
            )))
            .expect("send notification");
    }

    fn recv(&self) -> Message {
        self.conn
            .receiver
            .recv_timeout(TIMEOUT)
            .expect("server response within timeout")
    }

    /// Next publishDiagnostics notification (buffered or fresh).
    fn diagnostics(&mut self) -> PublishDiagnosticsParams {
        loop {
            let n = self
                .pending
                .pop_front()
                .unwrap_or_else(|| match self.recv() {
                    Message::Notification(n) => n,
                    other => panic!("expected notification, got {other:?}"),
                });
            if n.method == "textDocument/publishDiagnostics" {
                return serde_json::from_value(n.params).expect("valid publish params");
            }
        }
    }

    fn open(&self, uri: &Uri, text: &str) {
        self.notify(
            DidOpenTextDocument::METHOD,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "keron".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            },
        );
    }

    fn change(&self, uri: &Uri, version: i32, text: &str) {
        self.notify(
            DidChangeTextDocument::METHOD,
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.to_string(),
                }],
            },
        );
    }

    fn position_params(uri: &Uri, position: Position) -> TextDocumentPositionParams {
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        }
    }

    fn shutdown(mut self) {
        let _ = self.request(Shutdown::METHOD, serde_json::Value::Null);
        self.notify("exit", serde_json::Value::Null);
        self.server
            .take()
            .expect("server handle")
            .join()
            .expect("server thread exits cleanly");
    }
}

/// Position of `needle`'s first byte (+ `add` chars) in single-line
/// terms; the fixtures keep multi-line offsets simple by construction.
fn pos_of(src: &str, needle: &str, add: u32) -> Position {
    let offset = src.find(needle).expect("needle present");
    let line = src[..offset].matches('\n').count();
    let line_start = src[..offset].rfind('\n').map_or(0, |i| i + 1);
    let character = u32::try_from(offset - line_start).expect("short lines") + add;
    Position::new(u32::try_from(line).expect("short files"), character)
}

#[test]
fn diagnostics_lifecycle_open_broken_then_fix() {
    let proj = TempProject::new("diag");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let mut client = Client::start();

    client.open(&uri, "val n: Int = \"not an int\"\n");
    let published = client.diagnostics();
    assert_eq!(published.uri, uri);
    assert_eq!(published.version, Some(1));
    assert!(
        !published.diagnostics.is_empty(),
        "type error must produce a diagnostic"
    );
    let diag = &published.diagnostics[0];
    assert_eq!(diag.source.as_deref(), Some("keron"));
    assert!(
        diag.message.contains("Int"),
        "message should mention the type: {}",
        diag.message
    );

    client.change(&uri, 2, "val n: Int = 1\n");
    let cleared = client.diagnostics();
    assert_eq!(cleared.uri, uri);
    assert_eq!(cleared.version, Some(2));
    assert!(
        cleared.diagnostics.is_empty(),
        "fixed buffer must clear diagnostics"
    );
    client.shutdown();
}

#[test]
fn stale_document_change_cannot_roll_back_newer_text() {
    let proj = TempProject::new("stale-change");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let mut client = Client::start();
    client.open(&uri, "val initial: Int = 1\n");
    client.change(&uri, 3, "val current: Int = 2  ");
    client.change(&uri, 2, "val stale: Int = \"broken\"\n");

    let result = client.request(
        Formatting::METHOD,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    );
    let edits: Vec<lsp_types::TextEdit> = serde_json::from_value(result).expect("edits");
    assert_eq!(edits[0].new_text, "val current: Int = 2\n");
    client.shutdown();
}

#[test]
fn aliased_uri_cannot_mutate_an_open_document() {
    let proj = TempProject::new("aliased-change");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let alias = Uri::from_str(&uri.as_str().replacen("file://", "file://localhost", 1))
        .expect("localhost file URI");
    let mut client = Client::start();
    client.open(&uri, "val original: Int = 1  ");
    client.change(&alias, 2, "val forged: Int = 2  ");

    let result = client.request(
        Formatting::METHOD,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    );
    let edits: Vec<lsp_types::TextEdit> = serde_json::from_value(result).expect("edits");
    assert_eq!(edits[0].new_text, "val original: Int = 1\n");
    client.shutdown();
}

#[test]
fn hover_shows_builtin_signature() {
    let proj = TempProject::new("hover");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let src = "val s: Symlink = symlink(source = \"a\", target = \"b\")\nreconcile s\n";
    let mut client = Client::start();
    client.open(&uri, src);

    let result = client.request(
        HoverRequest::METHOD,
        HoverParams {
            text_document_position_params: Client::position_params(
                &uri,
                pos_of(src, "symlink(", 2),
            ),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    );
    let hover: Hover = serde_json::from_value(result).expect("hover result");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(
        markup.value.contains("fn symlink(") && markup.value.contains("Symlink"),
        "builtin signature expected, got: {}",
        markup.value
    );
    client.shutdown();
}

#[test]
fn definition_jumps_across_files() {
    let proj = TempProject::new("defs");
    let lib = proj.write(
        "lib.keron",
        "fn greet(who: String): String { \"hi \" + who }\n",
    );
    let main = proj.write("main.keron", "");
    let main_uri = file_uri(&main);
    let lib_uri = file_uri(&fs::canonicalize(&lib).expect("lib exists"));
    let src = "from \"./lib.keron\" use greet\nval g: String = greet(\"you\")\n";
    let mut client = Client::start();
    client.open(&main_uri, src);

    let result = client.request(
        GotoDefinition::METHOD,
        GotoDefinitionParams {
            text_document_position_params: Client::position_params(
                &main_uri,
                pos_of(src, "greet(\"you\")", 2),
            ),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        },
    );
    let response: GotoDefinitionResponse =
        serde_json::from_value(result).expect("definition result");
    let GotoDefinitionResponse::Scalar(location) = response else {
        panic!("expected scalar location");
    };
    assert_eq!(location.uri, lib_uri, "definition must land in lib.keron");
    assert_eq!(location.range.start, Position::new(0, 3));
    client.shutdown();
}

#[test]
fn definition_rejects_non_module_use_target() {
    let proj = TempProject::new("defs-invalid-target");
    proj.write("notes.txt", "not a keron module\n");
    let main = proj.write("main.keron", "");
    let main_uri = file_uri(&main);
    let src = "from \"./notes.txt\" use anything\n";
    let mut client = Client::start();
    client.open(&main_uri, src);

    let result = client.request(
        GotoDefinition::METHOD,
        GotoDefinitionParams {
            text_document_position_params: Client::position_params(
                &main_uri,
                pos_of(src, "./notes.txt", 2),
            ),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        },
    );

    assert!(
        result.is_null(),
        "invalid imports must not expose arbitrary files"
    );
    client.shutdown();
}

#[test]
fn formatting_returns_whole_document_edit() {
    let proj = TempProject::new("fmt");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let mut client = Client::start();
    client.open(&uri, "val x : Int=1");

    let result = client.request(
        Formatting::METHOD,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    );
    let edits: Vec<lsp_types::TextEdit> = serde_json::from_value(result).expect("edits");
    assert_eq!(edits.len(), 1, "one whole-document edit");
    assert_eq!(edits[0].new_text, "val x: Int = 1\n");
    assert_eq!(edits[0].range.start, Position::new(0, 0));
    client.shutdown();
}

#[test]
fn semantic_tokens_cover_keywords_and_functions() {
    let proj = TempProject::new("tokens");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let mut client = Client::start();
    client.open(
        &uri,
        "# comment\nval s: Symlink = symlink(source = \"a\", target = \"b\")\n",
    );

    let result = client.request(
        SemanticTokensFullRequest::METHOD,
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        },
    );
    let tokens: SemanticTokens = serde_json::from_value(result).expect("tokens");
    assert!(
        tokens.data.len() >= 8,
        "expected a full token stream, got {} tokens",
        tokens.data.len()
    );
    // First token: the comment at line 0 col 0 (legend index 1).
    let first = &tokens.data[0];
    assert_eq!((first.delta_line, first.delta_start), (0, 0));
    assert_eq!(first.token_type, 1, "leading comment");
    // Second token: `val` keyword on the next line (legend index 0).
    let second = &tokens.data[1];
    assert_eq!(second.delta_line, 1);
    assert_eq!(second.token_type, 0, "`val` keyword");
    assert_eq!(second.length, 3);
    client.shutdown();
}

#[test]
fn partially_broken_buffer_serves_hover_for_intact_items() {
    let proj = TempProject::new("partial");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let src = "val s: Symlink = symlink(source = \"a\", target = \"b\")\nreconcile s\n";
    let mut client = Client::start();
    // A clean open publishes nothing (only *changes* are pushed), so
    // don't wait for diagnostics here.
    client.open(&uri, src);

    // Break the FIRST item; parser recovery re-syncs at the next
    // top-level keyword, so the later items survive in the partial
    // AST while diagnostics report the broken one.
    let broken_src = format!("fn broken(: Int {{ 1 }}\n{src}");
    client.change(&uri, 2, &broken_src);
    let broken = client.diagnostics();
    assert!(
        !broken.diagnostics.is_empty(),
        "broken buffer must produce parse diagnostics"
    );

    // Hover on the intact item answers against the *live* text.
    let result = client.request(
        HoverRequest::METHOD,
        HoverParams {
            text_document_position_params: Client::position_params(
                &uri,
                pos_of(&broken_src, "symlink(", 2),
            ),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    );
    let hover: Option<Hover> = serde_json::from_value(result).expect("hover result");
    let hover = hover.expect("hover from the partial AST");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(
        markup.value.contains("fn symlink("),
        "got: {}",
        markup.value
    );
    client.shutdown();
}

#[test]
fn utf8_position_encoding_is_negotiated() {
    let proj = TempProject::new("utf8");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let init = InitializeParams {
        capabilities: lsp_types::ClientCapabilities {
            general: Some(lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![
                    lsp_types::PositionEncodingKind::UTF8,
                    lsp_types::PositionEncodingKind::UTF16,
                ]),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let (mut client, result) = Client::start_with(init);
    assert_eq!(
        result["capabilities"]["positionEncoding"], "utf-8",
        "server must advertise the negotiated encoding"
    );

    // 'é' is 2 bytes / 1 UTF-16 unit; with utf-8 columns the type
    // error on the string literal starts at byte column 14 (it would
    // be 13 under utf-16).
    let src = "val é: Int = \"x\"\n";
    client.open(&uri, src);
    let published = client.diagnostics();
    let diag = &published.diagnostics[0];
    assert_eq!(diag.range.start.line, 0);
    assert_eq!(
        diag.range.start.character, 14,
        "utf-8 columns are byte offsets: got {:?}",
        diag.range
    );
    client.shutdown();
}

#[test]
fn rename_rewrites_definition_and_importers() {
    let proj = TempProject::new("rename");
    let lib = proj.write(
        "lib.keron",
        "fn greet(who: String): String { \"hi \" + who }\n",
    );
    let main = proj.write("main.keron", "");
    let main_uri = file_uri(&main);
    let lib_uri = file_uri(&fs::canonicalize(&lib).expect("lib exists"));
    let src = "from \"./lib.keron\" use greet\nval g: String = greet(\"you\")\n";
    let mut client = Client::start();
    client.open(&main_uri, src);

    let result = client.request(
        "textDocument/rename",
        serde_json::json!({
            "textDocument": {"uri": main_uri.as_str()},
            "position": pos_of(src, "greet(\"you\")", 2),
            "newName": "welcome",
        }),
    );
    let edit: lsp_types::WorkspaceEdit = serde_json::from_value(result).expect("workspace edit");
    // Uri's interior cell trips clippy's mutable-key-type; the map is
    // read-only here.
    #[allow(clippy::mutable_key_type)]
    let changes = edit.changes.expect("changes map");
    let main_edits = changes.get(&main_uri).expect("edits in main");
    // use-name + callee reference.
    assert_eq!(main_edits.len(), 2, "main edits: {main_edits:?}");
    assert!(main_edits.iter().all(|e| e.new_text == "welcome"));
    let lib_edits = changes.get(&lib_uri).expect("edits in lib");
    assert_eq!(lib_edits.len(), 1, "lib edits: {lib_edits:?}");
    assert_eq!(lib_edits[0].range.start, Position::new(0, 3));
    client.shutdown();
}

#[test]
fn references_cross_module_and_respect_shadowing() {
    let proj = TempProject::new("refs");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let src = "val x: Int = 1\nfn f(x: Int): Int { x }\nval y: Int = x\n";
    let mut client = Client::start();
    client.open(&uri, src);

    let result = client.request(
        "textDocument/references",
        serde_json::json!({
            "textDocument": {"uri": uri.as_str()},
            "position": pos_of(src, "x: Int = 1", 0),
            "context": {"includeDeclaration": true},
        }),
    );
    let locations: Vec<lsp_types::Location> = serde_json::from_value(result).expect("locations");
    // The decl and the `val y` reference — NOT the param-shadowed
    // body occurrence.
    assert_eq!(locations.len(), 2, "got: {locations:?}");
    assert!(locations.iter().all(|l| l.uri == uri));
    client.shutdown();
}

#[test]
fn inlay_hints_show_inferred_types_and_param_names() {
    let proj = TempProject::new("hints");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let src = "val greeting = \"hello\"\nval s: Symlink = symlink(\"a\", \"b\")\nreconcile s\n";
    let mut client = Client::start();
    client.open(&uri, src);

    let result = client.request(
        "textDocument/inlayHint",
        serde_json::json!({
            "textDocument": {"uri": uri.as_str()},
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 3, "character": 0}},
        }),
    );
    let hints: Vec<lsp_types::InlayHint> = serde_json::from_value(result).expect("hints");
    let labels: Vec<String> = hints
        .iter()
        .map(|h| match &h.label {
            lsp_types::InlayHintLabel::String(s) => s.clone(),
            other @ lsp_types::InlayHintLabel::LabelParts(_) => {
                panic!("unexpected label {other:?}")
            }
        })
        .collect();
    assert!(
        labels.contains(&": String".to_string()),
        "inferred val type hint missing: {labels:?}"
    );
    assert!(
        labels.contains(&"source = ".to_string()) && labels.contains(&"target = ".to_string()),
        "param name hints missing: {labels:?}"
    );
    client.shutdown();
}

#[test]
fn code_action_offers_did_you_mean_quickfix() {
    let proj = TempProject::new("quickfix");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let src = "val name: String = \"x\"\nval a: String = nane\n";
    let mut client = Client::start();
    client.open(&uri, src);
    let published = client.diagnostics();
    let diag = published
        .diagnostics
        .iter()
        .find(|d| d.message.contains("did you mean"))
        .expect("suggestion diagnostic")
        .clone();

    let result = client.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": {"uri": uri.as_str()},
            "range": diag.range,
            "context": {"diagnostics": [diag]},
        }),
    );
    let actions: Vec<lsp_types::CodeActionOrCommand> =
        serde_json::from_value(result).expect("actions");
    let lsp_types::CodeActionOrCommand::CodeAction(action) = &actions[0] else {
        panic!("expected code action");
    };
    assert_eq!(action.title, "Replace with `name`");
    #[allow(clippy::mutable_key_type)] // read-only Uri-keyed map
    let edit = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .expect("edit");
    assert_eq!(edit[&uri][0].new_text, "name");

    let forged = lsp_types::Diagnostic {
        range: lsp_types::Range::new(Position::new(0, 0), Position::new(0, 3)),
        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
        source: Some("keron".to_string()),
        message: "help: did you mean `forged`?".to_string(),
        ..Default::default()
    };
    let result = client.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": {"uri": uri.as_str()},
            "range": forged.range,
            "context": {"diagnostics": [forged]},
        }),
    );
    let actions: Vec<lsp_types::CodeActionOrCommand> =
        serde_json::from_value(result).expect("actions");
    assert!(
        actions.is_empty(),
        "client-forged diagnostics must not produce edits"
    );
    client.shutdown();
}

#[test]
fn diagnostics_and_quickfix_preserve_client_file_uri() {
    let proj = TempProject::new("diagnostic-client-uri");
    let path = proj.write("main.keron", "");
    let canonical_uri = file_uri(&path);
    let client_uri = Uri::from_str(&canonical_uri.as_str().replacen(
        "file://",
        "file://localhost",
        1,
    ))
    .expect("localhost file URI");
    let src = "val name: String = \"x\"\nval a: String = nane\n";
    let mut client = Client::start();
    client.open(&client_uri, src);
    let published = client.diagnostics();
    assert_eq!(published.uri, client_uri);
    let diag = published
        .diagnostics
        .into_iter()
        .find(|diagnostic| diagnostic.message.contains("did you mean"))
        .expect("suggestion diagnostic");

    let actions: Vec<lsp_types::CodeActionOrCommand> = serde_json::from_value(client.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": {"uri": client_uri.as_str()},
            "range": diag.range,
            "context": {"diagnostics": [diag]},
        }),
    ))
    .expect("actions");
    assert_eq!(actions.len(), 1);
    client.shutdown();
}

#[test]
fn workspace_symbols_folding_and_selection_ranges_answer() {
    let proj = TempProject::new("navigation");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let src = "fn helper(a: Int): Int {\n  a + 1\n}\nval answer: Int = helper(41)\n";
    let mut client = Client::start();
    client.open(&uri, src);

    let symbols: Vec<lsp_types::SymbolInformation> = serde_json::from_value(
        client.request("workspace/symbol", serde_json::json!({"query": "help"})),
    )
    .expect("symbols");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "helper");

    let folds: Vec<lsp_types::FoldingRange> = serde_json::from_value(client.request(
        "textDocument/foldingRange",
        serde_json::json!({"textDocument": {"uri": uri.as_str()}}),
    ))
    .expect("folding ranges");
    assert!(
        folds.iter().any(|f| f.start_line == 0 && f.end_line == 2),
        "fn body fold missing: {folds:?}"
    );

    let chains: Vec<lsp_types::SelectionRange> = serde_json::from_value(client.request(
        "textDocument/selectionRange",
        serde_json::json!({
            "textDocument": {"uri": uri.as_str()},
            "positions": [pos_of(src, "a + 1", 0)],
        }),
    ))
    .expect("selection ranges");
    // Innermost `a` → `a + 1` → block → fn item: at least 3 levels.
    let mut depth = 0;
    let mut cur = Some(&chains[0]);
    while let Some(c) = cur {
        depth += 1;
        cur = c.parent.as_deref();
    }
    assert!(depth >= 3, "selection chain too shallow: {depth}");
    client.shutdown();
}

#[test]
fn document_highlight_marks_decl_and_reads() {
    let proj = TempProject::new("highlight");
    let path = proj.write("main.keron", "");
    let uri = file_uri(&path);
    let src = "val count: Int = 1\nval more: Int = count + count\n";
    let mut client = Client::start();
    client.open(&uri, src);

    let highlights: Vec<lsp_types::DocumentHighlight> = serde_json::from_value(client.request(
        "textDocument/documentHighlight",
        serde_json::json!({
            "textDocument": {"uri": uri.as_str()},
            "position": pos_of(src, "count + count", 1),
        }),
    ))
    .expect("highlights");
    assert_eq!(highlights.len(), 3, "got: {highlights:?}");
    let writes = highlights
        .iter()
        .filter(|h| h.kind == Some(lsp_types::DocumentHighlightKind::WRITE))
        .count();
    assert_eq!(writes, 1, "exactly the decl is a write");
    client.shutdown();
}
