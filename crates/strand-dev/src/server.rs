//! The language server: `strand-dev lsp` over stdio, or any
//! [`Connection`] (tests drive it in process with `Connection::memory`).
//!
//! Diagnostics are published for every file of a document's config when
//! it opens or is saved, and after changes stop for the debounce time
//! (200 ms; `initializationOptions.debounceMs`), so typing does not flash
//! errors. Requests always see the latest text.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CompletionOptions, CompletionParams, CompletionResponse,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentFormattingParams, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, HoverProviderCapability, Location, OneOf,
    PrepareRenameResponse, PublishDiagnosticsParams, RenameOptions, RenameParams,
    ServerCapabilities, TextDocumentPositionParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, TextEdit, Uri,
    WorkspaceEdit,
};
use serde_json::{Value, json};
use strand_compiler::fmt::format;
use strand_compiler::syntax::Span;

use crate::text::uri_to_path;
use crate::workspace::{ConfigKey, Workspace};
use crate::{completion, diag, nav};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// How long diagnostics wait after the last change.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Serves the protocol on stdin and stdout until the client exits.
pub fn run_stdio() -> Result<()> {
    let (conn, io) = Connection::stdio();
    serve(&conn)?;
    drop(conn);
    io.join()?;
    Ok(())
}

pub fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..TextDocumentSyncOptions::default()
            },
        )),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec!["$".into(), ".".into(), ">".into()]),
            ..CompletionOptions::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
            ..CodeActionOptions::default()
        })),
        document_formatting_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    }
}

/// Runs the server on `conn`: the initialize handshake, then requests and
/// notifications until `shutdown` and `exit`.
pub fn serve(conn: &Connection) -> Result<()> {
    let caps = serde_json::to_value(capabilities())?;
    let (id, params) = conn.initialize_start()?;
    conn.initialize_finish(
        id,
        json!({
            "capabilities": caps,
            "serverInfo": { "name": "strand-dev", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    let mut roots: Vec<PathBuf> = params["workspaceFolders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| uri_to_path(f["uri"].as_str()?))
        .collect();
    if roots.is_empty()
        && let Some(root) = params["rootUri"].as_str().and_then(uri_to_path)
    {
        roots.push(root);
    }
    let debounce = params["initializationOptions"]["debounceMs"]
        .as_u64()
        .map_or(DEBOUNCE, Duration::from_millis);
    let mut server = Server {
        conn,
        ws: Workspace::new(roots),
        debounce,
        dirty: HashSet::new(),
        deadline: None,
        published: HashMap::new(),
    };
    server.run()
}

struct Server<'c> {
    conn: &'c Connection,
    ws: Workspace,
    debounce: Duration,
    /// Configs whose diagnostics wait for the debounce.
    dirty: HashSet<ConfigKey>,
    deadline: Option<Instant>,
    /// URIs with diagnostics published, per config, to clear them later.
    published: HashMap<ConfigKey, HashSet<String>>,
}

impl Server<'_> {
    fn run(&mut self) -> Result<()> {
        loop {
            let msg = match self.deadline {
                Some(d) => {
                    match self
                        .conn
                        .receiver
                        .recv_timeout(d.saturating_duration_since(Instant::now()))
                    {
                        Ok(m) => m,
                        Err(RecvTimeoutError::Timeout) => {
                            self.flush()?;
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
                None => match self.conn.receiver.recv() {
                    Ok(m) => m,
                    Err(_) => return Ok(()),
                },
            };
            match msg {
                Message::Request(req) => {
                    if self.conn.handle_shutdown(&req)? {
                        return Ok(());
                    }
                    let id = req.id.clone();
                    // A bug in one feature must not take the editor's
                    // server down: answer with an error instead.
                    let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.request(req)
                    }))
                    .unwrap_or_else(|_| {
                        Response::new_err(
                            id,
                            ErrorCode::InternalError as i32,
                            "strand-dev: internal error (a panic); please report it".into(),
                        )
                    });
                    self.conn.sender.send(Message::Response(resp))?;
                }
                Message::Notification(n) => {
                    if n.method == "exit" {
                        return Ok(());
                    }
                    let method = n.method.clone();
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.notification(n)
                    })) {
                        Ok(r) => r?,
                        Err(_) => eprintln!("strand-dev: internal error handling {method}"),
                    }
                }
                Message::Response(_) => {}
            }
        }
    }

    // -----------------------------------------------------------------------
    // Notifications

    fn notification(&mut self, n: Notification) -> Result<()> {
        match n.method.as_str() {
            "textDocument/didOpen" => {
                let p: DidOpenTextDocumentParams = serde_json::from_value(n.params)?;
                let uri = p.text_document.uri.as_str().to_string();
                self.ws
                    .open(uri.clone(), p.text_document.text, p.text_document.version);
                self.publish_now(&uri)?;
            }
            "textDocument/didChange" => {
                let p: DidChangeTextDocumentParams = serde_json::from_value(n.params)?;
                let uri = p.text_document.uri.as_str().to_string();
                // Full sync: the last change holds the whole text.
                if let Some(c) = p.content_changes.into_iter().last() {
                    self.ws.change(&uri, c.text, p.text_document.version);
                }
                self.dirty.insert(self.ws.config_of(&uri));
                self.deadline = Some(Instant::now() + self.debounce);
            }
            "textDocument/didSave" => {
                let p: DidSaveTextDocumentParams = serde_json::from_value(n.params)?;
                self.ws.touch();
                self.publish_now(p.text_document.uri.as_str())?;
            }
            "textDocument/didClose" => {
                let p: DidCloseTextDocumentParams = serde_json::from_value(n.params)?;
                let uri = p.text_document.uri.as_str().to_string();
                let key = self.ws.config_of(&uri);
                self.ws.close(&uri);
                if let ConfigKey::Single(_) = key {
                    // Nothing checks it any more.
                    if let Some(set) = self.published.remove(&key) {
                        for u in set {
                            self.send_diagnostics(&u, Vec::new(), None)?;
                        }
                    }
                } else {
                    self.publish(&key)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn publish_now(&mut self, uri: &str) -> Result<()> {
        let key = self.ws.config_of(uri);
        self.dirty.remove(&key);
        self.publish(&key)
    }

    /// Publishes the configs waiting for the debounce.
    fn flush(&mut self) -> Result<()> {
        self.deadline = None;
        let keys: Vec<ConfigKey> = self.dirty.drain().collect();
        for k in keys {
            self.publish(&k)?;
        }
        Ok(())
    }

    /// Publishes diagnostics for every file of a config, clearing files
    /// that had some and no longer belong to it or have none.
    fn publish(&mut self, key: &ConfigKey) -> Result<()> {
        let an = self.ws.analysis(key);
        let mut now = HashSet::new();
        for f in an.files() {
            let uri = an.uri(f).to_string();
            let diags = diag::for_file(&an, f);
            let version = self.ws.docs.get(&uri).map(|d| d.version);
            let had = self.published.get(key).is_some_and(|s| s.contains(&uri));
            if !diags.is_empty() || had || self.ws.docs.contains_key(&uri) {
                self.send_diagnostics(&uri, diags, version)?;
            }
            now.insert(uri);
        }
        let old = self.published.insert(key.clone(), now.clone());
        for gone in old.into_iter().flatten().filter(|u| !now.contains(u)) {
            self.send_diagnostics(&gone, Vec::new(), None)?;
        }
        Ok(())
    }

    fn send_diagnostics(
        &self,
        uri: &str,
        diagnostics: Vec<lsp_types::Diagnostic>,
        version: Option<i32>,
    ) -> Result<()> {
        let Ok(uri) = uri.parse::<Uri>() else {
            return Ok(());
        };
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        };
        self.conn
            .sender
            .send(Message::Notification(Notification::new(
                "textDocument/publishDiagnostics".into(),
                params,
            )))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Requests

    fn request(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        let result = match req.method.as_str() {
            "textDocument/completion" => self.completion(req.params),
            "textDocument/hover" => self.hover(req.params),
            "textDocument/definition" => self.definition(req.params),
            "textDocument/prepareRename" => self.prepare_rename(req.params),
            "textDocument/rename" => self.rename(req.params),
            "textDocument/codeAction" => self.code_action(req.params),
            "textDocument/formatting" => self.formatting(req.params),
            m => {
                return Response::new_err(
                    id,
                    ErrorCode::MethodNotFound as i32,
                    format!("unknown method {m}"),
                );
            }
        };
        respond(id, result)
    }

    /// The analysis, file and offset of a position.
    fn locate(
        &mut self,
        p: &TextDocumentPositionParams,
    ) -> Option<(
        std::sync::Arc<crate::workspace::Analysis>,
        strand_compiler::FileId,
        u32,
    )> {
        let uri = p.text_document.uri.as_str();
        let an = self.ws.analysis_of(uri);
        let file = an.file(uri)?;
        let offset = an.offset(file, p.position);
        Some((an, file, offset))
    }

    fn completion(&mut self, params: Value) -> Reply {
        let p: CompletionParams = serde_json::from_value(params)?;
        let Some((an, file, offset)) = self.locate(&p.text_document_position) else {
            return Ok(Value::Null);
        };
        let items = completion::complete(&an, file, offset);
        Ok(serde_json::to_value(CompletionResponse::Array(items))?)
    }

    fn hover(&mut self, params: Value) -> Reply {
        let p: HoverParams = serde_json::from_value(params)?;
        let Some((an, file, offset)) = self.locate(&p.text_document_position_params) else {
            return Ok(Value::Null);
        };
        Ok(serde_json::to_value(nav::hover(&an, file, offset))?)
    }

    fn definition(&mut self, params: Value) -> Reply {
        let p: GotoDefinitionParams = serde_json::from_value(params)?;
        let Some((an, file, offset)) = self.locate(&p.text_document_position_params) else {
            return Ok(Value::Null);
        };
        let locations: Vec<Location> = nav::definition(&an, file, offset)
            .into_iter()
            .filter_map(|(f, span)| Some(Location::new(an.uri(f).parse().ok()?, an.range(f, span))))
            .collect();
        if locations.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::to_value(GotoDefinitionResponse::Array(
            locations,
        ))?)
    }

    fn prepare_rename(&mut self, params: Value) -> Reply {
        let p: TextDocumentPositionParams = serde_json::from_value(params)?;
        let Some((an, file, offset)) = self.locate(&p) else {
            return Ok(Value::Null);
        };
        let (span, placeholder) = nav::prepare_rename(&an, file, offset).map_err(Failed)?;
        Ok(serde_json::to_value(
            PrepareRenameResponse::RangeWithPlaceholder {
                range: an.range(file, span),
                placeholder,
            },
        )?)
    }

    // `WorkspaceEdit::changes` is keyed by `Uri`, whose parsed form caches
    // through a `Cell`; the key is never mutated here.
    #[allow(clippy::mutable_key_type)]
    fn rename(&mut self, params: Value) -> Reply {
        let p: RenameParams = serde_json::from_value(params)?;
        let Some((an, file, offset)) = self.locate(&p.text_document_position) else {
            return Ok(Value::Null);
        };
        let edits = nav::rename(&an, file, offset, &p.new_name).map_err(Failed)?;
        let mut changes = HashMap::new();
        for (f, list) in edits {
            let Ok(uri) = an.uri(f).parse::<Uri>() else {
                continue;
            };
            let edits: Vec<TextEdit> = list
                .into_iter()
                .map(|(span, new_text)| TextEdit {
                    range: an.range(f, span),
                    new_text,
                })
                .collect();
            changes.insert(uri, edits);
        }
        Ok(serde_json::to_value(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })?)
    }

    fn code_action(&mut self, params: Value) -> Reply {
        let p: CodeActionParams = serde_json::from_value(params)?;
        let uri = p.text_document.uri.as_str();
        let an = self.ws.analysis_of(uri);
        let Some(file) = an.file(uri) else {
            return Ok(Value::Null);
        };
        let range = Span::new(an.offset(file, p.range.start), an.offset(file, p.range.end));
        let actions: Vec<CodeActionOrCommand> = diag::quick_fixes(&an, file, range)
            .into_iter()
            .map(CodeActionOrCommand::CodeAction)
            .collect();
        Ok(serde_json::to_value(actions)?)
    }

    fn formatting(&mut self, params: Value) -> Reply {
        let p: DocumentFormattingParams = serde_json::from_value(params)?;
        let uri = p.text_document.uri.as_str();
        let an = self.ws.analysis_of(uri);
        let Some(file) = an.file(uri) else {
            return Ok(Value::Null);
        };
        let text = an.text(file);
        match format(text) {
            Ok(out) if out == text => Ok(json!([])),
            Ok(out) => {
                let whole = an.range(file, Span::new(0, text.len() as u32));
                Ok(serde_json::to_value(vec![TextEdit {
                    range: whole,
                    new_text: out,
                }])?)
            }
            // Syntax errors are already shown; leave the file alone.
            Err(_) => Ok(Value::Null),
        }
    }
}

/// A request that failed for a reason the user should see.
#[derive(Debug)]
struct Failed(String);

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for Failed {}

type Reply = std::result::Result<Value, Box<dyn Error + Send + Sync>>;

fn respond(id: RequestId, result: Reply) -> Response {
    match result {
        Ok(v) => Response::new_ok(id, v),
        Err(e) => {
            let code = if e.is::<Failed>() {
                ErrorCode::RequestFailed
            } else if e.is::<serde_json::Error>() {
                ErrorCode::InvalidParams
            } else {
                ErrorCode::InternalError
            };
            Response::new_err(id, code as i32, e.to_string())
        }
    }
}
