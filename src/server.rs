//! The language server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{self, Analysis};
use crate::code_action::{self, OPEN_IN_HUB_COMMAND, REFRESH_COMMAND};
use crate::config::{Config, expand_home};
use crate::hover;
use crate::hub::Hub;
use crate::memory::{self, Memory};
use crate::source::{self, Selection};
use crate::store::Store;
use crate::warning::{self, Warning};

const WATCH_PATTERN: &str = "**/*.sarif";

pub struct Backend {
    client: Client,
    state: RwLock<State>,
    reload_lock: Mutex<()>,
}

struct Document {
    version: i32,
    text: String,
}

#[derive(Default)]
struct State {
    config: Config,
    root: Option<PathBuf>,
    hub: Option<Arc<Hub>>,
    /// The loaded analysis and how it was chosen.
    analysis: Option<Analysis>,
    selection: Option<Selection>,
    /// The shown warnings in the current code.
    store: Store,
    documents: HashMap<Url, Document>,
    can_watch_files: bool,
}

impl State {
    /// Sets the settings and the hub client they describe.
    fn configure(&mut self, config: Config) -> anyhow::Result<()> {
        self.hub = match &config.hub {
            Some(hub) => Some(Arc::new(Hub::new(hub)?)),
            None => None,
        };
        self.config = config;
        Ok(())
    }

    /// The current text of a file: from its document if open, else from disk.
    fn text(&self, uri: &Url) -> String {
        match self.documents.get(uri) {
            Some(document) => document.text.clone(),
            None => read(uri).unwrap_or_default(),
        }
    }

    /// Matches the warnings of a file to its current text again.
    fn relocate(&mut self, uri: &Url) {
        let text = self.text(uri);
        let changed = self.config.changed;

        if let Some(analysis) = &mut self.analysis {
            analysis.relocate(uri, &text);
            self.store.set(uri, analysis.warnings(uri, changed));
        }
    }

    fn diagnostics(&self, uri: &Url) -> (Vec<Diagnostic>, Option<i32>) {
        let diagnostics = self
            .store
            .get(uri)
            .iter()
            .map(Warning::to_diagnostic)
            .collect();
        (diagnostics, self.documents.get(uri).map(|d| d.version))
    }
}

fn read(uri: &Url) -> Option<String> {
    std::fs::read_to_string(uri.to_file_path().ok()?).ok()
}

fn modified(uri: &Url) -> Option<SystemTime> {
    std::fs::metadata(uri.to_file_path().ok()?)
        .ok()?
        .modified()
        .ok()
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: RwLock::default(),
            reload_lock: Mutex::default(),
        }
    }

    async fn error(&self, error: impl std::fmt::Display) {
        self.client
            .show_message(MessageType::ERROR, format!("CodeSonar: {error}"))
            .await;
    }

    /// Loads the analysis shown before, else the configured one.
    async fn reload(&self) {
        let selection = self.state.read().await.selection.clone();
        self.load(selection.unwrap_or(Selection::Configured)).await;
    }

    /// Loads an analysis and publishes its warnings as diagnostics.
    async fn load(&self, selection: Selection) {
        let _guard = self.reload_lock.lock().await;
        let (config, root, hub) = {
            let state = self.state.read().await;
            (state.config.clone(), state.root.clone(), state.hub.clone())
        };

        let loaded =
            match source::load(&config, root.as_deref(), hub.as_deref(), selection.clone()).await {
                Ok(Some(loaded)) => loaded,
                Ok(None) => {
                    self.client
                        .log_message(MessageType::INFO, "CodeSonar: no warning source configured")
                        .await;
                    return;
                }
                Err(error) => {
                    self.error(format!("{error:#}")).await;
                    return;
                }
            };

        let warnings = warning::from_log(&loaded.log, &config, root.as_deref());
        let time = loaded
            .log
            .runs
            .first()
            .and_then(|run| run.end_time())
            .and_then(analysis::parse_time);
        let mut analysis = Analysis::new(loaded.key, loaded.name, loaded.hub_id, time, warnings);
        self.learn_from_disk(&config, &mut analysis).await;

        let uris = {
            let mut state = self.state.write().await;
            let mut uris: Vec<Url> = state.store.files().map(|(uri, _)| uri.clone()).collect();
            uris.extend(analysis.uris().cloned());
            uris.sort();
            uris.dedup();

            state.analysis = Some(analysis);
            state.selection = Some(selection);
            state.store = Store::default();

            for uri in &uris {
                state.relocate(uri);
            }

            uris
        };

        self.publish(&uris).await;
        self.loaded().await;
    }

    /// Gives warnings the analyzed code from the memory, else from files
    /// that are unchanged since the analysis, and remembers what it learned.
    async fn learn_from_disk(&self, config: &Config, analysis: &mut Analysis) {
        let Some(dir) = config.state_dir.clone().or_else(memory::default_dir) else {
            return;
        };
        let mut memory = Memory::open(&expand_home(&dir));
        analysis.set_anchors(&memory.anchors(&analysis.key));

        let mut learned = HashMap::new();

        for uri in analysis.missing_anchors() {
            let unchanged = match (analysis.time, modified(&uri)) {
                (Some(time), Some(modified)) => modified <= time,
                (None, Some(_)) => true,
                (_, None) => false,
            };

            if let Some(text) = unchanged.then(|| read(&uri)).flatten() {
                learned.extend(analysis.learn(&uri, &text));
            }
        }

        if !learned.is_empty()
            && let Err(error) = memory.remember(&analysis.key, learned)
        {
            self.client
                .log_message(MessageType::WARNING, format!("CodeSonar: {error:#}"))
                .await;
        }
    }

    async fn loaded(&self) {
        let message = {
            let state = self.state.read().await;
            let Some(analysis) = &state.analysis else {
                return;
            };
            let counts = analysis.counts();
            let mut message = format!("CodeSonar: {}: {} warnings", analysis.name, analysis.len());

            if counts.changed > 0 {
                message.push_str(&format!(", {} in changed code", counts.changed));
            }

            message
        };
        self.client.log_message(MessageType::INFO, message).await;
    }

    async fn publish(&self, uris: &[Url]) {
        let diagnostics: Vec<_> = {
            let state = self.state.read().await;
            uris.iter()
                .map(|uri| (uri.clone(), state.diagnostics(uri)))
                .collect()
        };

        for (uri, (diagnostics, version)) in diagnostics {
            self.client
                .publish_diagnostics(uri, diagnostics, version)
                .await;
        }
    }

    /// Updates a document and its warnings.
    async fn update(&self, uri: Url, document: Option<Document>) {
        {
            let mut state = self.state.write().await;

            match document {
                // Notifications may be handled out of order.
                Some(document)
                    if state
                        .documents
                        .get(&uri)
                        .is_some_and(|d| d.version > document.version) =>
                {
                    return;
                }
                Some(document) => {
                    state.documents.insert(uri.clone(), document);
                }
                None => {
                    state.documents.remove(&uri);
                }
            }

            if state.store.get(&uri).is_empty()
                && !state.analysis.as_ref().is_some_and(|a| a.contains(&uri))
            {
                return;
            }

            state.relocate(&uri);
        }

        self.publish(&[uri]).await;
    }

    async fn watch_files(&self) {
        let options = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![FileSystemWatcher {
                glob_pattern: GlobPattern::String(WATCH_PATTERN.to_string()),
                kind: None,
            }],
        };
        let registration = Registration {
            id: "codesonar-watch-sarif".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(options).ok(),
        };

        if let Err(error) = self.client.register_capability(vec![registration]).await {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("CodeSonar: cannot watch SARIF files: {error}"),
                )
                .await;
        }
    }

    async fn open_in_hub(&self, arguments: &[Value]) -> Result<()> {
        let uri = arguments
            .first()
            .and_then(Value::as_str)
            .and_then(|uri| Url::parse(uri).ok())
            .ok_or_else(|| Error::invalid_params("expected a warning URL"))?;

        self.client
            .show_document(ShowDocumentParams {
                uri,
                external: Some(true),
                take_focus: Some(true),
                selection: None,
            })
            .await?;
        Ok(())
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let config = match &params.initialization_options {
            Some(options) => {
                Config::from_value(options).map_err(|e| Error::invalid_params(e.to_string()))?
            }

            None => Config::default(),
        };

        let can_watch_files = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files)
            .and_then(|watch| watch.dynamic_registration)
            .unwrap_or(false);

        let mut state = self.state.write().await;
        state.root = workspace_root(&params);
        state
            .configure(config)
            .map_err(|e| Error::invalid_params(format!("{e:#}")))?;
        state.can_watch_files = can_watch_files;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        ..CodeActionOptions::default()
                    },
                )),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![REFRESH_COMMAND.to_string(), OPEN_IN_HUB_COMMAND.to_string()],
                    ..ExecuteCommandOptions::default()
                }),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if self.state.read().await.can_watch_files {
            self.watch_files().await;
        }

        self.reload().await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // Only the `codesonar` section is used, so that unrelated or empty
        // notifications keep the settings from `initializationOptions`.
        let Some(settings) = params.settings.get("codesonar") else {
            return;
        };

        let config = match Config::from_value(settings) {
            Ok(config) => config,
            Err(error) => {
                self.client
                    .show_message(
                        MessageType::ERROR,
                        format!("CodeSonar: invalid settings: {error}"),
                    )
                    .await;
                return;
            }
        };

        {
            let mut state = self.state.write().await;

            if state.config == config {
                return;
            }

            if let Err(error) = state.configure(config) {
                drop(state);
                self.error(format!("invalid settings: {error:#}")).await;
                return;
            }
        }

        self.reload().await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let sarif_file = {
            let state = self.state.read().await;
            state.config.sarif_file(state.root.as_deref())
        };

        let Some(sarif_file) = sarif_file else {
            return;
        };

        let changed = params
            .changes
            .iter()
            .any(|change| change.uri.to_file_path().ok().as_ref() == Some(&sarif_file));

        if changed {
            self.reload().await;
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        self.update(
            document.uri,
            Some(Document {
                version: document.version,
                text: document.text,
            }),
        )
        .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // With full synchronization, the last change holds the whole text.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        self.update(
            params.text_document.uri,
            Some(Document {
                version: params.text_document.version,
                text: change.text,
            }),
        )
        .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.update(params.text_document.uri, None).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params;
        let state = self.state.read().await;
        Ok(hover::hover(
            &state.store,
            &position.text_document.uri,
            position.position,
        ))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let state = self.state.read().await;
        let actions =
            code_action::code_actions(&state.store, &params.text_document.uri, params.range);
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
        match params.command.as_str() {
            REFRESH_COMMAND => self.reload().await,
            OPEN_IN_HUB_COMMAND => self.open_in_hub(&params.arguments).await?,
            command => {
                return Err(Error::invalid_params(format!("unknown command {command}")));
            }
        }

        Ok(None)
    }
}

#[allow(deprecated)]
fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .map(|folder| &folder.uri)
        .or(params.root_uri.as_ref())
        .and_then(|uri| uri.to_file_path().ok())
}
