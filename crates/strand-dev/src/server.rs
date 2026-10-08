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
use std::sync::Arc;
use std::time::{Duration, Instant};

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CompletionOptions, CompletionParams, CompletionResponse,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentChanges, DocumentFormattingParams, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, HoverProviderCapability, Location, OneOf,
    OptionalVersionedTextDocumentIdentifier, PrepareRenameResponse, PublishDiagnosticsParams,
    RenameOptions, RenameParams, ServerCapabilities, TextDocumentEdit, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TextEdit, Uri, WorkspaceEdit,
};
use serde_json::{Value, json};
use strand_compiler::FileId;
use strand_compiler::fmt::format;
use strand_compiler::schema::Schema;
use strand_compiler::syntax::Span;

use crate::text::uri_to_path;
use crate::workspace::{Analysis, ConfigKey, Workspace, default_dir};
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

/// The schema `strand-dev lsp` serves: the builtin schema extended by
/// every builtin service (`strand_services_schema::schemas()`, the texts
/// without the service runtime), as
/// `strand check` and `strand run` check against it. A service schema
/// that does not apply (a bug the service crates' tests catch) is
/// logged and left out.
pub fn schema() -> Arc<Schema> {
    static SCHEMA: std::sync::OnceLock<Arc<Schema>> = std::sync::OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            let texts = strand_services_schema::schemas();
            Arc::new(match Schema::builtin_with(&texts) {
                Ok(s) => s,
                Err((i, errors)) => {
                    eprintln!("strand-dev: service schema #{i} does not apply: {errors:?}");
                    Schema::builtin().clone()
                }
            })
        })
        .clone()
}

/// Runs the server on `conn` against [`schema`]: the initialize
/// handshake, then requests and notifications until `shutdown` and `exit`.
pub fn serve(conn: &Connection) -> Result<()> {
    serve_with(conn, schema())
}

/// [`serve`] against `schema`: the builtin schema as the service crates
/// linked into the caller extend it (`Schema::extend`), so hover,
/// completion and checking see their services.
pub fn serve_with(conn: &Connection, schema: Arc<Schema>) -> Result<()> {
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
    let options = &params["initializationOptions"];
    let debounce = options["debounceMs"]
        .as_u64()
        .map_or(DEBOUNCE, Duration::from_millis);
    // The default config directory, as `strand check` finds it; a client
    // (or a test) may name another.
    let config_dir = match options["configDir"].as_str() {
        Some(d) => Some(PathBuf::from(d)),
        None => default_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        ),
    };
    let client = &params["capabilities"];
    let document_changes = client["workspace"]["workspaceEdit"]["documentChanges"]
        .as_bool()
        .unwrap_or(false);
    // Hear about `.strand` files changed outside the editor.
    if client["workspace"]["didChangeWatchedFiles"]["dynamicRegistration"].as_bool() == Some(true) {
        conn.sender.send(Message::Request(Request::new(
            RequestId::from("strand-dev/watch".to_string()),
            "client/registerCapability".into(),
            json!({ "registrations": [{
                "id": "strand-dev/watch",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": { "watchers": [{ "globPattern": "**/*.strand" }] },
            }]}),
        )))?;
    }
    let mut server = Server {
        conn,
        ws: Workspace::new(roots, config_dir, schema),
        debounce,
        document_changes,
        dirty: HashSet::new(),
        deadline: None,
        published: HashMap::new(),
        owner: HashMap::new(),
    };
    server.run()
}

struct Server<'c> {
    conn: &'c Connection,
    ws: Workspace,
    debounce: Duration,
    /// The client applies versioned `documentChanges`, so it can refuse
    /// edits made for text it no longer has.
    document_changes: bool,
    /// Configs whose diagnostics wait for the debounce.
    dirty: HashSet<ConfigKey>,
    deadline: Option<Instant>,
    /// The configs published, with the URIs whose diagnostics they show
    /// (to clear them later).
    published: HashMap<ConfigKey, HashSet<String>>,
    /// The config that last published each URI's diagnostics. A URI moves
    /// between configs (a new file saved into a config directory), and
    /// only its owner may clear or republish it.
    owner: HashMap<String, ConfigKey>,
}

impl Server<'_> {
    fn run(&mut self) -> Result<()> {
        loop {
            // Past the debounce, publish before reading on: a client that
            // keeps the channel busy (hovers, cancels) must not hold the
            // diagnostics back.
            if self.deadline.is_some_and(|d| d <= Instant::now()) {
                self.flush()?;
            }
            let timer = match self.deadline {
                Some(d) => crossbeam_channel::after(d.saturating_duration_since(Instant::now())),
                None => crossbeam_channel::never(),
            };
            let msg = crossbeam_channel::select! {
                recv(self.conn.receiver) -> m => match m {
                    Ok(m) => m,
                    Err(_) => return Ok(()),
                },
                // An introspection answer came in: what checks `from
                // dbus` services is due again.
                recv(crate::workspace::introspected()) -> _ => {
                    while crate::workspace::introspected().try_recv().is_ok() {}
                    self.introspected();
                    continue;
                },
                recv(timer) -> _ => {
                    self.flush()?;
                    continue;
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
                    self.recheck_rebuilt();
                    self.evict_unshown();
                }
                Message::Notification(n) => {
                    if n.method == "exit" {
                        return Ok(());
                    }
                    let method = n.method.clone();
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.notification(n)
                    })) {
                        Ok(Ok(())) => {}
                        // Bad params: say so and carry on. Only a closed
                        // channel ends the server.
                        Ok(Err(e)) if e.is::<serde_json::Error>() => {
                            self.log(&format!("strand-dev: ignored {method}: {e}"))?;
                        }
                        Ok(Err(e)) => return Err(e),
                        Err(_) => {
                            self.log(&format!("strand-dev: internal error handling {method}"))?
                        }
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
                self.prune();
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
                self.ws.disk_changed();
                self.publish_now(p.text_document.uri.as_str())?;
                self.prune();
            }
            "textDocument/didClose" => {
                let p: DidCloseTextDocumentParams = serde_json::from_value(n.params)?;
                let uri = p.text_document.uri.as_str().to_string();
                let key = self.ws.config_of(&uri);
                self.ws.close(&uri);
                self.dirty.remove(&key);
                if self.ws.has_open(&key) || self.ws.is_workspace(&key) {
                    // Still checked: as the disk has it now.
                    self.publish(&key)?;
                } else {
                    // Nothing shows it any more.
                    self.unpublish(&key)?;
                }
                self.prune();
            }
            "workspace/didChangeWatchedFiles" => {
                self.ws.disk_changed();
                // Re-check what is shown, after the debounce, each file
                // with the config it belongs to now.
                let keys: Vec<ConfigKey> = self.published.keys().cloned().collect();
                for k in keys {
                    let now = match &k {
                        ConfigKey::Single(u) => self.ws.config_of(u),
                        ConfigKey::Dir(_) => k,
                    };
                    self.dirty.insert(now);
                }
                // An open file that left a config (now checked alone, or
                // in another directory) is shown from its new config.
                let open: Vec<String> = self.ws.docs.keys().cloned().collect();
                for u in open {
                    let k = self.ws.config_of(&u);
                    self.dirty.insert(k);
                }
                self.deadline = Some(Instant::now() + self.debounce);
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
        self.prune();
        Ok(())
    }

    /// Schedules what is shown for a check again: an analysis that checked
    /// a `from dbus` service against introspection is stale once a bus
    /// answered ([`Workspace::analysis`] rebuilds it).
    fn introspected(&mut self) {
        let keys: Vec<ConfigKey> = self.published.keys().cloned().collect();
        if !keys.is_empty() {
            self.dirty.extend(keys);
            self.deadline = Some(Instant::now() + self.debounce);
        }
    }

    /// Schedules the configs a request found changed on disk (the client
    /// did not say so), so what is shown catches up with what requests
    /// see.
    fn recheck_rebuilt(&mut self) {
        let keys: Vec<ConfigKey> = self
            .ws
            .take_rebuilt()
            .into_iter()
            .filter(|k| self.published.contains_key(k))
            .collect();
        if !keys.is_empty() {
            self.dirty.extend(keys);
            self.deadline = Some(Instant::now() + self.debounce);
        }
    }

    /// Publishes diagnostics for every file of a config, taking those
    /// files over from any config that published them before, and
    /// clearing files it showed that left it.
    fn publish(&mut self, key: &ConfigKey) -> Result<()> {
        let an = self.ws.analysis(key);
        // Published now, whatever changed on disk to cause it.
        self.ws.settle(key);
        let old = self.published.remove(key).unwrap_or_default();
        let mut shown = HashSet::new();
        let mut in_config = HashSet::new();
        for f in an.files() {
            let uri = an.uri(f).to_string();
            let diags = diag::for_file(&an, f);
            let version = self.ws.docs.get(&uri).map(|d| d.version);
            let previous = self.owner.insert(uri.clone(), key.clone());
            // Another config showed this file: what it showed is replaced
            // by what is sent now, so it no longer holds it.
            let moved = previous.as_ref().is_some_and(|p| p != key);
            if let Some(p) = previous.filter(|p| p != key)
                && let Some(set) = self.published.get_mut(&p)
            {
                set.remove(&uri);
            }
            if !diags.is_empty() || old.contains(&uri) || moved || self.ws.docs.contains_key(&uri) {
                if !diags.is_empty() {
                    shown.insert(uri.clone());
                }
                self.send_diagnostics(&uri, diags, version)?;
            }
            in_config.insert(uri);
        }
        for gone in old.iter().filter(|u| !in_config.contains(*u)) {
            // Only the owner clears a file; another config may show it now.
            if self.owner.get(gone) == Some(key) {
                self.owner.remove(gone);
                self.send_diagnostics(gone, Vec::new(), None)?;
            }
        }
        // A clean file that left the config is no longer held by it.
        self.owner.retain(|u, k| k != key || in_config.contains(u));
        self.published.insert(key.clone(), shown);
        Ok(())
    }

    /// Forgets the analyses requests compiled for configs that show
    /// nothing and wait for nothing (a hover in a file never opened), so
    /// a long session does not keep every config it was asked about.
    fn evict_unshown(&mut self) {
        let unshown: Vec<ConfigKey> = self
            .ws
            .cached_keys()
            .into_iter()
            .filter(|k| !self.published.contains_key(k) && !self.dirty.contains(k))
            .collect();
        for k in unshown {
            if !self.ws.has_open(&k) {
                self.ws.forget(&k);
            }
        }
    }

    /// Clears what `key` shows and forgets it.
    fn unpublish(&mut self, key: &ConfigKey) -> Result<()> {
        if let Some(set) = self.published.remove(key) {
            for u in set {
                if self.owner.get(&u) == Some(key) {
                    self.send_diagnostics(&u, Vec::new(), None)?;
                }
            }
        }
        self.owner.retain(|_, k| k != key);
        self.dirty.remove(key);
        self.ws.forget(key);
        Ok(())
    }

    /// Forgets the published configs that no longer own any file: their
    /// files moved to another config, which shows them now. They are not
    /// re-checked again.
    fn prune(&mut self) {
        let owning: HashSet<&ConfigKey> = self.owner.values().collect();
        let stale: Vec<ConfigKey> = self
            .published
            .keys()
            .filter(|k| !owning.contains(k))
            .cloned()
            .collect();
        for k in stale {
            self.published.remove(&k);
            self.dirty.remove(&k);
            self.ws.forget(&k);
        }
    }

    /// A message for the client's log.
    fn log(&self, message: &str) -> Result<()> {
        eprintln!("{message}");
        self.conn
            .sender
            .send(Message::Notification(Notification::new(
                "window/logMessage".into(),
                json!({ "type": 1, "message": message }),
            )))?;
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

    /// A workspace edit of the config's files. With a client that takes
    /// `documentChanges`, edits to open documents carry their version, so
    /// the client refuses them if the text changed since.
    // `WorkspaceEdit::changes` is keyed by `Uri`, whose parsed form caches
    // through a `Cell`; the key is never mutated here.
    #[allow(clippy::mutable_key_type)]
    fn workspace_edit(
        &self,
        an: &Analysis,
        edits: Vec<(FileId, Vec<(Span, String)>)>,
    ) -> WorkspaceEdit {
        let mut changes = HashMap::new();
        let mut documents = Vec::new();
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
            if self.document_changes {
                let version = self.ws.docs.get(an.uri(f)).map(|d| d.version);
                documents.push(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                });
            } else {
                changes.insert(uri, edits);
            }
        }
        if self.document_changes {
            WorkspaceEdit {
                document_changes: Some(DocumentChanges::Edits(documents)),
                ..WorkspaceEdit::default()
            }
        } else {
            WorkspaceEdit {
                changes: Some(changes),
                ..WorkspaceEdit::default()
            }
        }
    }

    fn rename(&mut self, params: Value) -> Reply {
        let p: RenameParams = serde_json::from_value(params)?;
        let Some((an, file, offset)) = self.locate(&p.text_document_position) else {
            return Ok(Value::Null);
        };
        let edits = nav::rename(&an, file, offset, &p.new_name).map_err(Failed)?;
        Ok(serde_json::to_value(
            self.workspace_edit(&an, edits.into_iter().collect()),
        )?)
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
            .map(|fix| {
                CodeActionOrCommand::CodeAction(CodeAction {
                    title: fix.title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![fix.diagnostic]),
                    edit: Some(
                        self.workspace_edit(&an, vec![(file, vec![(fix.span, fix.new_text)])]),
                    ),
                    is_preferred: Some(fix.preferred),
                    ..CodeAction::default()
                })
            })
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
