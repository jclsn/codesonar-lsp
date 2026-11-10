//! The language server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{self, Analysis};
use crate::analyze;
use crate::code_action;
use crate::commands::{
    self, AnalyzeArgs, ConnectArgs, ExplainArgs, LoadArgs, OPEN_IN_HUB_COMMAND, ProjectArgs,
    REFRESH_COMMAND,
};
use crate::config::{Config, expand_home};
use crate::hover;
use crate::hub::{self, Hub};
use crate::memory::{self, Memory};
use crate::source::{self, Selection};
use crate::store::Store;
use crate::tracking::Anchor;
use crate::warning::{self, Rule, Warning};

const WATCH_PATTERN: &str = "**/*.sarif";

pub struct Backend {
    client: Client,
    state: RwLock<State>,
    reload_lock: Mutex<()>,
    /// The rules of warnings, by their hub URL.
    rules: Mutex<HashMap<Url, Vec<Rule>>>,
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

    fn status(&self) -> Value {
        let hub = self.config.hub.as_ref();
        // How analyses are run, also before one is loaded.
        let mut status = json!({
            "loaded": false,
            "hub": hub.map(|hub| &hub.url),
            "project": hub.and_then(|hub| hub.project.as_ref()),
            "presets": self.config.analyze.presets,
        });
        let Some(analysis) = &self.analysis else {
            return status;
        };
        let url = analysis
            .hub_id
            .as_ref()
            .zip(self.hub.as_ref())
            .and_then(|(id, hub)| hub.analysis_url(id).ok());
        let fields = [
            ("loaded", json!(true)),
            ("name", json!(analysis.name)),
            ("key", json!(analysis.key)),
            ("hubId", json!(analysis.hub_id)),
            ("url", json!(url)),
            ("total", json!(analysis.len())),
            ("shown", json!(self.store.len())),
            ("counts", json!(analysis.counts())),
        ];

        for (key, value) in fields {
            status[key] = value;
        }

        status
    }

    /// The shown warnings, by file and position.
    fn warnings(&self) -> Value {
        let mut files: Vec<_> = self.store.files().collect();
        files.sort_by_key(|(uri, _)| uri.as_str());
        let warnings: Vec<Value> = files
            .into_iter()
            .flat_map(|(uri, warnings)| {
                let mut warnings: Vec<&Warning> = warnings.iter().collect();
                warnings.sort_by_key(|w| w.range.start);
                warnings.into_iter().map(move |warning| {
                    let mut value = json!(warning.to_diagnostic());
                    value["uri"] = json!(uri);
                    value["status"] = json!(warning.status);
                    value["hubUri"] = json!(warning.hub_uri);
                    value
                })
            })
            .collect();
        Value::Array(warnings)
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

fn open_memory(config: &Config) -> Option<Memory> {
    let dir = config.state_dir.clone().or_else(memory::default_dir)?;
    Some(Memory::open(&expand_home(&dir)))
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
            rules: Mutex::default(),
        }
    }

    async fn error(&self, error: impl std::fmt::Display) {
        self.client
            .show_message(MessageType::ERROR, format!("CodeSonar: {error}"))
            .await;
    }

    /// Loads the analysis shown before, else the configured one, and
    /// reports errors.
    async fn reload(&self) {
        let selection = self.state.read().await.selection.clone();

        if let Err(error) = self.load(selection.unwrap_or(Selection::Configured)).await {
            self.error(format!("{error:#}")).await;
        }
    }

    /// Loads an analysis and publishes its warnings as diagnostics.
    async fn load(&self, selection: Selection) -> anyhow::Result<()> {
        let _guard = self.reload_lock.lock().await;
        let (config, root, hub) = {
            let state = self.state.read().await;
            (state.config.clone(), state.root.clone(), state.hub.clone())
        };

        let Some(loaded) =
            source::load(&config, root.as_deref(), hub.as_deref(), selection.clone()).await?
        else {
            self.client
                .log_message(MessageType::INFO, "CodeSonar: no warning source configured")
                .await;
            return Ok(());
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

        if let Some(hub) = hub {
            self.learn_from_hub(&config, &hub).await;
        }

        Ok(())
    }

    /// Gives warnings whose analyzed code is still unknown the code of the
    /// analyzed sources that the hub keeps.
    async fn learn_from_hub(&self, config: &Config, hub: &Hub) {
        let (key, id, missing) = {
            let state = self.state.read().await;
            let Some(analysis) = &state.analysis else {
                return;
            };
            let Some(id) = analysis.hub_id.clone() else {
                return;
            };
            (analysis.key.clone(), id, analysis.missing_anchors())
        };

        if missing.is_empty() {
            return;
        }

        let files = match hub.files(&id).await {
            Ok(files) => files,
            Err(error) => {
                self.warn(format!("cannot list analyzed files: {error:#}"))
                    .await;
                return;
            }
        };
        let mut sources = Vec::new();

        for uri in missing {
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            let Some(file) = hub::find_file(&files, &path, |name| config.map_path(name)) else {
                continue;
            };

            match hub.source(&file.url).await {
                Ok(lines) => sources.push((uri, lines.join("\n"))),
                Err(error) => {
                    self.warn(format!("cannot read {}: {error:#}", file.file))
                        .await;
                }
            }
        }

        let mut learned = HashMap::new();
        {
            let mut state = self.state.write().await;

            match &mut state.analysis {
                Some(analysis) if analysis.key == key => {
                    for (uri, text) in &sources {
                        learned.extend(analysis.learn(uri, text));
                    }
                }
                _ => return,
            }

            for (uri, _) in &sources {
                state.relocate(uri);
            }
        }

        self.remember(config, &key, learned).await;
        let uris: Vec<Url> = sources.into_iter().map(|(uri, _)| uri).collect();
        self.publish(&uris).await;
        self.loaded().await;
    }

    async fn warn(&self, message: String) {
        self.client
            .log_message(MessageType::WARNING, format!("CodeSonar: {message}"))
            .await;
    }

    async fn remember(&self, config: &Config, key: &str, anchors: HashMap<String, Anchor>) {
        if anchors.is_empty() {
            return;
        }

        let Some(mut memory) = open_memory(config) else {
            return;
        };

        if let Err(error) = memory.remember(key, anchors) {
            self.warn(format!("{error:#}")).await;
        }
    }

    /// Gives warnings the analyzed code from the memory, else from files
    /// that are unchanged since the analysis, and remembers what it learned.
    async fn learn_from_disk(&self, config: &Config, analysis: &mut Analysis) {
        if let Some(memory) = open_memory(config) {
            analysis.set_anchors(&memory.anchors(&analysis.key));
        }

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

        self.remember(config, &analysis.key, learned).await;
    }

    async fn loaded(&self) {
        let (message, status) = {
            let state = self.state.read().await;
            let Some(analysis) = &state.analysis else {
                return;
            };
            let counts = analysis.counts();
            let mut message = format!("CodeSonar: {}: {} warnings", analysis.name, analysis.len());

            if counts.changed > 0 {
                message.push_str(&format!(", {} in changed code", counts.changed));
            }

            (message, state.status())
        };
        self.client.log_message(MessageType::INFO, message).await;
        self.client
            .send_notification::<commands::Loaded>(status)
            .await;
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

    fn hub(state: &State) -> anyhow::Result<Arc<Hub>> {
        state.hub.clone().context("no hub configured")
    }

    async fn connect(&self, args: ConnectArgs) -> anyhow::Result<Value> {
        let hub = {
            let mut state = self.state.write().await;
            let mut config = state.config.clone();
            let mut hub = config.hub.take().unwrap_or_default();

            if let Some(url) = args.url {
                hub.url = url;
            }

            if args.username.is_some() {
                hub.username = args.username;
            }

            if args.password.is_some() {
                hub.password = args.password;
            }

            if args.project.is_some() {
                hub.project = args.project.clone();
                hub.analysis_id.clear();
            }

            config.hub = Some(hub);
            state.configure(config)?;
            Self::hub(&state)?
        };

        let projects = hub.projects().await?;

        if args.project.is_some() {
            self.load(Selection::Latest).await?;
        }

        Ok(json!(projects))
    }

    async fn projects(&self) -> anyhow::Result<Value> {
        let hub = Self::hub(&*self.state.read().await)?;
        Ok(json!(hub.projects().await?))
    }

    async fn analyses(&self) -> anyhow::Result<Value> {
        let (hub, current) = {
            let state = self.state.read().await;
            let current = state.analysis.as_ref().and_then(|a| a.hub_id.clone());
            (Self::hub(&state)?, current)
        };
        let project = hub
            .config()
            .project
            .clone()
            .context("no hub project configured")?;
        let analyses: Vec<Value> = hub
            .analyses(&project)
            .await?
            .into_iter()
            .map(|analysis| {
                let mut value = json!(analysis);
                value["finished"] = json!(analysis.is_finished());
                value["current"] = json!(current.as_deref() == Some(&analysis.id.to_string()));
                value
            })
            .collect();
        Ok(json!({"project": project, "analyses": analyses}))
    }

    async fn load_command(&self, args: LoadArgs) -> anyhow::Result<Value> {
        let selection = match (args.analysis, args.file) {
            (Some(Value::String(id)), _) => Selection::Hub(id),
            (Some(Value::Number(id)), _) => Selection::Hub(id.to_string()),
            (Some(other), _) => bail!("invalid analysis id {other}"),
            (None, Some(file)) => Selection::File(expand_home(Path::new(&file))),
            (None, None) if args.latest => Selection::Latest,
            (None, None) => Selection::Configured,
        };
        self.load(selection).await?;
        Ok(self.state.read().await.status())
    }

    async fn analyze_command(&self, args: AnalyzeArgs) -> anyhow::Result<Value> {
        let state = self.state.read().await;
        let root = state.root.as_deref().context("no workspace folder")?;
        let command = analyze::command(&state.config, root, args.name.as_deref())?;
        Ok(json!(command))
    }

    async fn project(&self, args: ProjectArgs) -> anyhow::Result<Value> {
        let state = self.state.read().await;
        let root = state.root.as_deref().context("no workspace folder")?;
        let created = args.create_conf && analyze::create_conf(&state.config, root)?;
        let prefix = analyze::prefix(&state.config, root);
        let conf = analyze::conf_file(&prefix);
        Ok(json!({
            "prefix": prefix,
            "confExists": conf.exists(),
            "conf": conf,
            "created": created,
        }))
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

    /// Explains warnings, with the titles of their rules from the hub.
    async fn explain(&self, args: ExplainArgs) -> Value {
        let (mut explanations, hub) = {
            let state = self.state.read().await;
            let explanations = hover::explain(
                &state.store,
                &args.uri,
                args.fingerprint.as_deref(),
                args.range,
            );
            (explanations, state.hub.clone())
        };

        if let Some(hub) = hub {
            for explanation in &mut explanations {
                if let Some(rules) = self.warning_rules(&hub, explanation.hub_uri.as_ref()).await {
                    explanation.set_rules(rules);
                }
            }
        }

        json!(explanations)
    }

    async fn warning_rules(&self, hub: &Hub, hub_uri: Option<&Url>) -> Option<Vec<Rule>> {
        let hub_uri = hub_uri?;
        let id = hub::warning_instance_id(hub_uri)?;

        if let Some(rules) = self.rules.lock().await.get(hub_uri) {
            return Some(rules.clone());
        }

        match hub.warning_rules(id).await {
            Ok(rules) => {
                self.rules
                    .lock()
                    .await
                    .insert(hub_uri.clone(), rules.clone());
                Some(rules)
            }
            Err(error) => {
                self.warn(format!(
                    "could not get the rules of warning {id}: {error:#}"
                ))
                .await;
                None
            }
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
                    commands: commands::ALL.iter().map(|c| c.to_string()).collect(),
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

        let reload = {
            let mut state = self.state.write().await;

            if state.config == config {
                return;
            }

            // How analyses are run does not change the shown one.
            let shown = Config {
                analyze: config.analyze.clone(),
                ..state.config.clone()
            };
            let reload = shown != config;

            if let Err(error) = state.configure(config) {
                drop(state);
                self.error(format!("invalid settings: {error:#}")).await;
                return;
            }

            reload
        };

        if reload {
            self.reload().await;
        }
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
        let arguments = &params.arguments;
        let result = match params.command.as_str() {
            REFRESH_COMMAND => {
                self.reload().await;
                return Ok(None);
            }
            OPEN_IN_HUB_COMMAND => {
                self.open_in_hub(arguments).await?;
                return Ok(None);
            }
            commands::CONNECT => self.connect(parse(arguments)?).await,
            commands::PROJECTS => self.projects().await,
            commands::ANALYSES => self.analyses().await,
            commands::LOAD => self.load_command(parse(arguments)?).await,
            commands::EXPLAIN_COMMAND => Ok(self.explain(required(arguments)?).await),
            commands::STATUS => Ok(self.state.read().await.status()),
            commands::WARNINGS => Ok(self.state.read().await.warnings()),
            commands::ANALYZE_COMMAND => self.analyze_command(parse(arguments)?).await,
            commands::PROJECT => self.project(parse(arguments)?).await,
            commands::PRESETS => {
                let state = self.state.read().await;
                analyze::presets(&state.config).map(|presets| json!(presets))
            }
            command => {
                return Err(Error::invalid_params(format!("unknown command {command}")));
            }
        };

        result.map(Some).map_err(|error| Error {
            code: ErrorCode::InternalError,
            message: format!("{error:#}").into(),
            data: None,
        })
    }
}

fn required<T: for<'de> serde::Deserialize<'de>>(arguments: &[Value]) -> Result<T> {
    let argument = arguments
        .first()
        .ok_or_else(|| Error::invalid_params("missing argument"))?;
    T::deserialize(argument).map_err(|error| Error::invalid_params(error.to_string()))
}

fn parse<T: Default + for<'de> serde::Deserialize<'de>>(arguments: &[Value]) -> Result<T> {
    commands::args(arguments).map_err(Error::invalid_params)
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
