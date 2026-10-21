//! The language server.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::code_action::{self, OPEN_IN_HUB_COMMAND, REFRESH_COMMAND};
use crate::config::Config;
use crate::hover;
use crate::source;
use crate::store::Store;

const WATCH_PATTERN: &str = "**/*.sarif";

pub struct Backend {
    client: Client,
    state: RwLock<State>,
    reload_lock: Mutex<()>,
}

#[derive(Default)]
struct State {
    config: Config,
    root: Option<PathBuf>,
    store: Store,
    can_watch_files: bool,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: RwLock::default(),
            reload_lock: Mutex::default(),
        }
    }

    /// Loads the warnings again and publishes them as diagnostics.
    async fn reload(&self) {
        let _guard = self.reload_lock.lock().await;
        let (config, root) = {
            let state = self.state.read().await;
            (state.config.clone(), state.root.clone())
        };

        let store = match source::load(&config, root.as_deref()).await {
            Ok(Some(warnings)) => Store::new(warnings),
            Ok(None) => {
                self.client
                    .log_message(MessageType::INFO, "CodeSonar: no warning source configured")
                    .await;
                Store::default()
            }
            Err(error) => {
                self.client
                    .show_message(MessageType::ERROR, format!("CodeSonar: {error:#}"))
                    .await;
                return;
            }
        };

        let mut diagnostics: Vec<(Url, Vec<Diagnostic>)> = store
            .files()
            .map(|(uri, warnings)| {
                let diagnostics = warnings.iter().map(|w| w.to_diagnostic()).collect();
                (uri.clone(), diagnostics)
            })
            .collect();
        let count = store.len();
        {
            let mut state = self.state.write().await;
            let current: HashSet<&Url> = store.files().map(|(uri, _)| uri).collect();
            for (uri, _) in state.store.files() {
                if !current.contains(uri) {
                    diagnostics.push((uri.clone(), Vec::new()));
                }
            }
            state.store = store;
        }

        for (uri, diagnostics) in diagnostics {
            self.client
                .publish_diagnostics(uri, diagnostics, None)
                .await;
        }
        self.client
            .log_message(
                MessageType::INFO,
                format!("CodeSonar: loaded {count} warnings"),
            )
            .await;
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
        state.config = config;
        state.can_watch_files = can_watch_files;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
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
            state.config = config;
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
