//! The language server.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, watch};
use tower_lsp::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{self, Analysis};
use crate::analyze;
use crate::cache::Cache;
use crate::checks;
use crate::code_action;
use crate::commands::{
    self, AnalyzeArgs, CheckChoice, ConfigureAnalysisArgs, ConnectArgs, DeleteAnalysisArgs,
    DisableCheckArgs, ExplainArgs, InitConfArgs, LoadArgs, OPEN_IN_HUB_COMMAND, ProjectArgs,
    REFRESH_COMMAND, ResolveArgs,
};
use crate::config::{Config, HubConfig, expand_home};
use crate::detect::{self, LocalProject};
use crate::hover;
use crate::hub::{self, Deletion, Downloaded, Hub};
use crate::memory::{self, Memory};
use crate::progress::{self, Progress};
use crate::source::{self, Loaded, Selection};
use crate::store::Store;
use crate::tracking::Anchor;
use crate::triage::{self, Resolution};
use crate::trust::Trust;
use crate::warning::{self, Rule, Warning};
use crate::workspace::{CheckRule, WorkspaceConf};

const WATCH_PATTERN: &str = "**/*.sarif";
/// How often the download progress is reported at most.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// The answer that trusts a hub with the credentials.
const SIGN_IN: &str = "Sign in";

/// The server. Clones share the state, so they can work in background
/// tasks.
#[derive(Clone)]
pub struct Backend {
    client: Client,
    state: Arc<RwLock<State>>,
    reload_lock: Arc<Mutex<()>>,
    /// The rules of warnings, by their hub URL.
    rules: Arc<Mutex<HashMap<Url, Vec<Rule>>>>,
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
    /// Whether the analysis was hidden on request: then none is loaded
    /// until one is chosen.
    hidden: bool,
    /// The shown warnings in the current code.
    store: Store,
    documents: HashMap<Url, Document>,
    can_watch_files: bool,
    can_show_progress: bool,
    /// The hub chosen with `codesonar.connect`, instead of the configured.
    connected: Option<HubConfig>,
    /// The configuration of the workspace in its `codesonar.conf`.
    workspace: WorkspaceConf,
    /// The CodeSonar project files of the workspace.
    local: Option<LocalProject>,
    /// The hub project of the workspace, if none is chosen or configured.
    detected_project: Option<String>,
    /// The hubs that may get the credentials, besides the configured one.
    trust: Trust,
    /// The hubs of the workspace configuration that the user did not trust
    /// with the credentials in this session.
    declined: HashSet<String>,
}

impl State {
    /// Sets the settings and the hub client they describe.
    fn configure(&mut self, config: Config) -> anyhow::Result<()> {
        self.trust = state_dir(&config).map_or_else(Trust::default, |dir| Trust::open(&dir));
        self.config = config;
        self.workspace = match &self.root {
            Some(root) => {
                WorkspaceConf::read(&root.join(expand_home(&self.config.analyze.conf_file)))
            }
            None => WorkspaceConf::default(),
        };
        self.hub = match self.hub_config() {
            Some(hub) => Some(Arc::new(Hub::new(&hub)?)),
            None => None,
        };
        Ok(())
    }

    /// The hub settings: the chosen hub, else the one of the workspace
    /// configuration and the settings, with the detected project if there
    /// is no other.
    fn hub_config(&self) -> Option<HubConfig> {
        let mut hub = match &self.connected {
            Some(hub) => hub.clone(),
            None => {
                let mut hub = self.config.with_workspace(&self.workspace).hub?;

                // The credentials only go to hubs the user trusts.
                if self.untrusted_hub().is_some() {
                    hub.username = None;
                    hub.password = None;
                    hub.password_file = None;
                }

                hub
            }
        };

        if hub.project.is_none() && hub.analysis_id.is_empty() {
            hub.project = self.detected_project.clone();
        }

        Some(hub)
    }

    /// The hub that only the workspace configuration names, if the user did
    /// not trust it with the credentials: not the hub of the settings and
    /// not one trusted before.
    fn untrusted_hub(&self) -> Option<String> {
        let address = self.workspace.hub_address.as_deref()?.trim();
        let configured = self
            .config
            .hub
            .as_ref()
            .and_then(|hub| hub_address(&hub.url));

        let trusted = configured.is_some_and(|configured| configured.eq_ignore_ascii_case(address))
            || self.trust.contains(address);
        (!trusted).then(|| address.to_string())
    }

    /// The settings, with the workspace configuration, the chosen hub and
    /// the detected project.
    fn effective_config(&self) -> Config {
        Config {
            hub: self.hub_config(),
            ..self.config.with_workspace(&self.workspace)
        }
    }

    /// The workspace configuration file, if it exists.
    fn workspace_file(&self) -> Option<&Path> {
        self.workspace.file.as_deref().filter(|file| file.is_file())
    }

    /// Whether the hub project is still unknown, so it may be detected.
    fn needs_project(&self) -> bool {
        self.detected_project.is_none()
            && self
                .hub_config()
                .is_some_and(|hub| hub.project.is_none() && hub.analysis_id.is_empty())
    }

    fn prefix(&self, root: &Path) -> PathBuf {
        analyze::prefix(&self.config, root, self.local.as_ref())
    }

    /// The current text of a file: from its document if open, else from disk.
    fn text(&self, uri: &Url) -> String {
        match self.documents.get(uri) {
            Some(document) => document.text.clone(),
            None => read(uri).unwrap_or_default(),
        }
    }

    /// Matches the warnings of a file to its current text again. Warnings
    /// of checks that the workspace configuration disables are left out.
    fn relocate(&mut self, uri: &Url) {
        let text = self.text(uri);
        let changed = self.config.changed;
        let disabled = self.disabled_checks();

        if let Some(analysis) = &mut self.analysis {
            analysis.relocate(uri, &text);
            let warnings = analysis
                .warnings(uri, changed)
                .into_iter()
                .filter(|warning| {
                    !warning
                        .rule_id
                        .as_deref()
                        .is_some_and(|class| disabled.iter().any(|d| d == class))
                })
                .collect();
            self.store.set(uri, warnings);
        }
    }

    /// The checks that the workspace configuration turns off.
    fn disabled_checks(&self) -> Vec<String> {
        self.workspace
            .checks
            .iter()
            .filter(|rule| !rule.enabled)
            .map(|rule| rule.class.clone())
            .collect()
    }

    fn status(&self) -> Value {
        let hub = self.hub_config();
        let hub = hub.as_ref();
        // How analyses are run, also before one is loaded.
        let mut status = json!({
            "loaded": false,
            "hidden": self.hidden,
            "hub": hub.map(|hub| &hub.url),
            "project": hub.and_then(|hub| hub.project.as_ref()),
            "presets": self.effective_config().analyze.presets,
            "conf": self.workspace_file(),
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

/// Reports downloads until they are done, at most every
/// `PROGRESS_INTERVAL`.
async fn report(progress: Progress, mut downloads: watch::Receiver<Downloaded>) -> Progress {
    while downloads.changed().await.is_ok() {
        let (message, percentage) = progress::download(*downloads.borrow_and_update());
        progress.report(message, percentage).await;
        tokio::time::sleep(PROGRESS_INTERVAL).await;
    }

    progress
}

/// The `HUB_ADDRESS` of a hub URL: `host:port`, if the hub is not below a
/// path.
fn hub_address(url: &str) -> Option<String> {
    let address = url.split_once("://").map_or(url, |(_, rest)| rest);
    let address = address.trim_end_matches('/');
    (!address.is_empty() && !address.contains('/')).then(|| address.to_string())
}

/// The directory of the memory and the trusted hubs.
fn state_dir(config: &Config) -> Option<PathBuf> {
    let dir = config.state_dir.clone().or_else(memory::default_dir)?;
    Some(expand_home(&dir))
}

fn open_memory(config: &Config) -> Option<Memory> {
    Some(Memory::open(&state_dir(config)?))
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
            state: Arc::default(),
            reload_lock: Arc::default(),
            rules: Arc::default(),
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
        let (selection, hidden) = {
            let state = self.state.read().await;
            (state.selection.clone(), state.hidden)
        };

        if hidden {
            return;
        }

        if let Err(error) = self.load(selection.unwrap_or(Selection::Configured)).await {
            self.error(format!("{error:#}")).await;
        }
    }

    /// Asks the user before the credentials go to a hub that only the
    /// workspace configuration names, since any repository can name any
    /// hub there. The answer is remembered: for good if the user trusts
    /// the hub, else until the server stops.
    async fn confirm_hub(&self) {
        let address = {
            let state = self.state.read().await;
            let credentials = state
                .config
                .hub
                .as_ref()
                .is_some_and(|hub| hub.username.is_some());

            match state.untrusted_hub() {
                Some(address)
                    if credentials
                        && state.connected.is_none()
                        && !state.declined.contains(&address) =>
                {
                    address
                }
                _ => return,
            }
        };

        let action = |title: &str| MessageActionItem {
            title: title.to_string(),
            properties: HashMap::new(),
        };
        let answer = self
            .client
            .show_message_request(
                MessageType::WARNING,
                format!(
                    "CodeSonar: The workspace configuration names the hub {address}. \
                     Sign in to it with your credentials?"
                ),
                Some(vec![action(SIGN_IN), action("Not now")]),
            )
            .await;
        let mut state = self.state.write().await;

        if matches!(&answer, Ok(Some(item)) if item.title == SIGN_IN) {
            let trusted = state.trust.add(&address);
            let config = state.config.clone();
            let configured = state.configure(config);
            drop(state);

            if let Err(error) = trusted.and(configured) {
                self.error(format!("{error:#}")).await;
            }
        } else {
            state.declined.insert(address.clone());
            drop(state);
            self.client
                .show_message(
                    MessageType::INFO,
                    format!("CodeSonar: Using the hub {address} without signing in"),
                )
                .await;
        }
    }

    /// Loads an analysis and publishes its warnings as diagnostics.
    async fn load(&self, selection: Selection) -> anyhow::Result<()> {
        let _guard = self.reload_lock.lock().await;
        self.confirm_hub().await;
        self.detect_project().await;
        let (config, root, hub, can_show_progress) = {
            let state = self.state.read().await;
            (
                state.effective_config(),
                state.root.clone(),
                state.hub.clone(),
                state.can_show_progress,
            )
        };

        self.loading(&config, root.as_deref(), &selection).await;
        let progress = match can_show_progress {
            true => Progress::begin(&self.client, "Loading the analysis").await,
            false => None,
        };
        let loaded = self
            .download(
                &config,
                root.as_deref(),
                hub.as_deref(),
                &selection,
                progress,
            )
            .await?;
        let Some(loaded) = loaded else {
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
            state.hidden = false;
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

    /// Loads the selected analysis and reports how much of it has arrived.
    async fn download(
        &self,
        config: &Config,
        root: Option<&Path>,
        hub: Option<&Hub>,
        selection: &Selection,
        progress: Option<Progress>,
    ) -> anyhow::Result<Option<Loaded>> {
        let (sender, receiver) = watch::channel(Downloaded::default());
        let reporter = progress.map(|progress| tokio::spawn(report(progress, receiver)));
        let loaded = source::load(config, root, hub, selection.clone(), |downloaded| {
            sender.send_replace(downloaded);
        })
        .await;
        drop(sender);

        if let Some(reporter) = reporter
            && let Ok(progress) = reporter.await
        {
            progress.end().await;
        }

        loaded
    }

    /// Tells the client what starts loading.
    async fn loading(&self, config: &Config, root: Option<&Path>, selection: &Selection) {
        let hub = config.hub.as_ref();
        let params = match source::resolve(config, root, selection.clone()) {
            Some(Selection::Hub(id)) => json!({"analysis": id}),
            Some(Selection::File(file)) => json!({"file": file}),
            Some(Selection::Latest | Selection::Configured) => {
                json!({"project": hub.and_then(|hub| hub.project.as_ref())})
            }
            None => return,
        };
        self.client
            .send_notification::<commands::Loading>(params)
            .await;
    }

    /// Detects the CodeSonar project of the workspace: its project files
    /// give the prefix and, by their last analysis, the hub project. Without
    /// them, the hub project named like the workspace is used.
    async fn detect_project(&self) {
        let (hub, local, root) = {
            let mut state = self.state.write().await;
            state.local = state.root.as_deref().and_then(detect::local_project);

            if !state.needs_project() {
                return;
            }

            let Some(hub) = state.hub.clone() else {
                return;
            };
            (hub, state.local.clone(), state.root.clone())
        };

        let mut project = None;

        if let Some(id) = local.and_then(|local| local.analysis_id) {
            match hub.analysis_project(&id).await {
                Ok(found) => project = Some(found.name),
                Err(error) => {
                    self.warn(format!(
                        "cannot find the project of analysis {id}: {error:#}"
                    ))
                    .await;
                }
            }
        }

        if project.is_none()
            && let Some(name) = root.as_deref().and_then(Path::file_name)
            && let Ok(projects) = hub.projects().await
        {
            project = projects
                .into_iter()
                .find(|project| name.eq_ignore_ascii_case(project.name.as_str()))
                .map(|project| project.name);
        }

        let Some(project) = project else {
            return;
        };

        self.client
            .log_message(
                MessageType::INFO,
                format!("CodeSonar: detected hub project {project}"),
            )
            .await;
        self.state.write().await.detected_project = Some(project);
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
            let mut hub = state
                .connected
                .clone()
                .or_else(|| state.config.hub.clone())
                .unwrap_or_default();

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

            // The workspace keeps the chosen project, and its hub if
            // CodeSonar can address it.
            if let Some(project) = &args.project
                && state.workspace.file.is_some()
            {
                state.workspace.project = Some(format!("/{project}"));
                state.workspace.write()?;

                if let Some(address) = hub_address(&hub.url) {
                    state.workspace.write_setting("HUB_ADDRESS", &address)?;
                }
            }

            // Kept apart from the settings, which clients may send again.
            state.connected = Some(hub);
            let config = state.config.clone();
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
        let (hub, current, cache) = {
            let state = self.state.read().await;
            let current = state.analysis.as_ref().and_then(|a| a.hub_id.clone());
            (
                Self::hub(&state)?,
                current,
                Cache::of(&state.effective_config()),
            )
        };
        let project = self
            .state
            .read()
            .await
            .hub_config()
            .and_then(|hub| hub.project)
            .context("no hub project configured or detected")?;
        let analyses: Vec<Value> = hub
            .analyses(&project)
            .await?
            .into_iter()
            .map(|analysis| {
                let mut value = json!(analysis);
                value["finished"] = json!(analysis.is_finished());
                let id = analysis.id.to_string();
                value["current"] = json!(current.as_deref() == Some(&id));
                value["cached"] = json!(cache.as_ref().is_some_and(|cache| cache.contains(&id)));
                value
            })
            .collect();
        Ok(json!({"project": project, "analyses": analyses}))
    }

    async fn delete_analysis(&self, args: DeleteAnalysisArgs) -> anyhow::Result<Value> {
        let id = args.analysis;
        let (hub, shown) = {
            let state = self.state.read().await;
            let shown = state.analysis.as_ref().and_then(|a| a.hub_id.clone());
            (Self::hub(&state)?, shown)
        };
        if let Deletion::Refused(message) = hub.delete_analysis(id).await? {
            let mut value = self.analyses().await?;
            value["deleted"] = json!(false);
            value["message"] = json!(message);
            return Ok(value);
        }

        self.client
            .log_message(
                MessageType::INFO,
                format!("CodeSonar: deleted analysis {id}"),
            )
            .await;
        self.uncache(id).await?;

        // A hidden analysis that is gone cannot be shown again.
        {
            let mut state = self.state.write().await;

            if state.hidden && state.selection == Some(Selection::Hub(id.to_string())) {
                state.selection = None;
            }
        }

        // Loading the next analysis can take a minute, the client should not
        // wait for it.
        if shown == Some(id.to_string()) {
            let backend = self.clone();
            tokio::spawn(async move {
                if let Err(error) = backend.load(Selection::Latest).await {
                    backend.unload().await;
                    backend.warn(format!("{error:#}")).await;
                }
            });
        }

        let mut value = self.analyses().await?;
        value["deleted"] = json!(true);
        Ok(value)
    }

    /// Reads the workspace configuration again, and shows the analysis it
    /// names if that changed.
    async fn workspace_changed(&self) {
        let (reload, disabled) = {
            let mut state = self.state.write().await;
            let before = state.hub_config();
            let disabled = state.disabled_checks();
            let config = state.config.clone();

            if let Err(error) = state.configure(config) {
                drop(state);
                self.error(format!("invalid workspace configuration: {error:#}"))
                    .await;
                return;
            }

            (
                state.hub_config() != before,
                state.disabled_checks() != disabled,
            )
        };

        if reload {
            self.reload().await;
        } else if disabled {
            self.show_again().await;
        }
    }

    /// Shows the warnings of the analysis again, e.g. after checks were
    /// disabled.
    async fn show_again(&self) {
        let (uris, status) = {
            let mut state = self.state.write().await;
            let Some(analysis) = &state.analysis else {
                return;
            };
            let mut uris: Vec<Url> = state.store.files().map(|(uri, _)| uri.clone()).collect();
            uris.extend(analysis.uris().cloned());
            uris.sort();
            uris.dedup();

            for uri in &uris {
                state.relocate(uri);
            }

            (uris, state.status())
        };

        self.publish(&uris).await;
        self.client
            .send_notification::<commands::Loaded>(status)
            .await;
    }

    /// Disables a check in the workspace configuration, so later analyses
    /// leave it out and its warnings are hidden now.
    async fn disable_check(&self, args: DisableCheckArgs) -> anyhow::Result<Value> {
        let choice = ConfigureAnalysisArgs {
            presets: None,
            checks: vec![CheckChoice {
                class: args.class.clone(),
                enabled: false,
            }],
        };
        let (disabled, message) = match self.configure_analysis(choice).await {
            Ok(_) => (
                true,
                format!(
                    "Disabled the check for '{}' in {}",
                    args.class,
                    self.conf_name().await
                ),
            ),
            Err(error) => (
                false,
                format!("cannot disable the check for '{}': {error:#}", args.class),
            ),
        };

        match disabled {
            _ if args.quiet => {}
            false => self.error(&message).await,
            true => {
                self.client
                    .show_message(MessageType::INFO, format!("CodeSonar: {message}"))
                    .await;
            }
        }

        Ok(json!({"disabled": disabled, "message": message}))
    }

    /// The name of the workspace configuration file, for messages.
    async fn conf_name(&self) -> String {
        let state = self.state.read().await;
        state
            .workspace
            .file
            .as_deref()
            .and_then(Path::file_name)
            .map_or("the workspace configuration".into(), |name| {
                name.to_string_lossy().into_owned()
            })
    }

    /// The presets and checks that analyses can use, as the workspace
    /// configuration chooses them.
    async fn analysis_options(&self) -> anyhow::Result<Value> {
        let state = self.state.read().await;
        let config = state.effective_config();
        let template = analyze::template(&config.analyze.command)
            .context("cannot find the CodeSonar installation")?;
        let template = std::fs::read_to_string(&template)
            .with_context(|| format!("cannot read {}", template.display()))?;
        let rules = &state.workspace.checks;
        let checks: Vec<Value> = checks::checks(&template, &checks::LANGUAGES)
            .into_iter()
            .map(|check| {
                let enabled = rules
                    .iter()
                    .find(|rule| rule.class == check.class)
                    .map_or(check.default, |rule| rule.enabled);
                json!({
                    "class": check.class,
                    "language": check.language,
                    "default": check.default,
                    "enabled": enabled,
                })
            })
            .collect();
        Ok(json!({
            "conf": state.workspace.file,
            "presets": analyze::presets(&config)?,
            "checks": checks,
        }))
    }

    /// Writes the presets and checks into the workspace configuration.
    async fn configure_analysis(&self, args: ConfigureAnalysisArgs) -> anyhow::Result<Value> {
        let defaults: Vec<(String, bool)> = {
            let state = self.state.read().await;
            let command = state.effective_config().analyze.command;
            let template = analyze::template(&command)
                .and_then(|template| std::fs::read_to_string(template).ok())
                .unwrap_or_default();
            checks::checks(&template, &checks::LANGUAGES)
                .into_iter()
                .map(|check| (check.class, check.default))
                .collect()
        };

        let disabled = {
            let mut state = self.state.write().await;
            let disabled = state.disabled_checks();

            if state.workspace.file.is_none() {
                bail!("no workspace folder");
            }

            if let Some(presets) = args.presets {
                state.workspace.presets = Some(presets);
            }

            for check in args.checks {
                let default = defaults
                    .iter()
                    .find(|(class, _)| *class == check.class)
                    .map(|(_, default)| *default);
                state
                    .workspace
                    .checks
                    .retain(|rule| rule.class != check.class);

                // Rules only for what differs from CodeSonar's default.
                if default != Some(check.enabled) {
                    state.workspace.checks.push(CheckRule {
                        class: check.class,
                        enabled: check.enabled,
                    });
                }
            }

            state.workspace.write()?;
            state.disabled_checks() != disabled
        };

        self.workspace_changed().await;

        if disabled {
            self.show_again().await;
        }

        self.analysis_options().await
    }

    /// Removes an analysis from the cache of downloaded analyses.
    async fn uncache(&self, id: u64) -> anyhow::Result<()> {
        let cache = Cache::of(&self.state.read().await.effective_config());

        if let Some(cache) = cache {
            cache.remove(&id.to_string())?;
        }

        Ok(())
    }

    /// Hides the shown analysis and its warnings until another one is
    /// loaded. The choice of the analysis is kept, so it can be shown
    /// again.
    async fn unload(&self) {
        let uris = {
            let mut state = self.state.write().await;
            let uris: Vec<Url> = state.store.files().map(|(uri, _)| uri.clone()).collect();
            state.analysis = None;
            state.hidden = true;
            state.store = Store::default();
            uris
        };
        self.publish(&uris).await;
        let status = self.state.read().await.status();
        self.client
            .send_notification::<commands::Loaded>(status)
            .await;
    }

    /// Hides the shown analysis, or shows the hidden one again.
    async fn toggle(&self) -> anyhow::Result<Value> {
        let (hidden, selection) = {
            let state = self.state.read().await;
            (state.hidden, state.selection.clone())
        };

        if hidden {
            self.load(selection.unwrap_or(Selection::Configured))
                .await?;
        } else {
            self.unload().await;
        }

        Ok(self.state.read().await.status())
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
        let config = state.effective_config();
        let project = config.hub.as_ref().and_then(|hub| hub.project.as_deref());
        let command = analyze::command(
            &config,
            root,
            &state.prefix(root),
            project,
            args.name.as_deref(),
        )?;
        Ok(json!(command))
    }

    async fn project(&self, args: ProjectArgs) -> anyhow::Result<Value> {
        // The file may have changed without a notification.
        let changed = {
            let state = self.state.read().await;
            let file = state.workspace.file.as_deref();
            file.is_some_and(|file| WorkspaceConf::read(file) != state.workspace)
        };

        if changed {
            self.workspace_changed().await;
        }

        let state = self.state.read().await;
        let root = state.root.as_deref().context("no workspace folder")?;
        let prefix = state.prefix(root);
        let created = args.create_conf && analyze::create_conf(&state.config, &prefix)?;
        let conf = analyze::conf_file(&prefix);
        let workspace_conf = analyze::workspace_conf(&state.config, root);
        // What the workspace configuration could start with: the folder
        // name, an incremental build and the hub address of the file, not
        // what the editor settings or another workspace use.
        let project = root.file_name().map(|name| name.to_string_lossy());
        let hub_address = &state.workspace.hub_address;
        Ok(json!({
            "prefix": prefix,
            "confExists": conf.exists(),
            "conf": conf,
            "created": created,
            "incremental": analyze::is_incremental(workspace_conf.as_deref(), &prefix),
            "workspaceConf": state.workspace.file,
            "workspaceConfExists": workspace_conf.is_some(),
            "section": state.workspace.has_section,
            "suggested": {
                "project": project,
                "build": "make -B",
                "incremental": state.workspace.incremental.unwrap_or(true),
                "incrementalBuild": "make",
                "hubAddress": hub_address,
            },
        }))
    }

    /// Starts the workspace configuration with the basic settings: the
    /// section with the hub project and the build commands, and
    /// `HUB_ADDRESS`.
    async fn init_conf(&self, args: InitConfArgs) -> anyhow::Result<Value> {
        {
            let mut state = self.state.write().await;

            if state.workspace.file.is_none() {
                bail!("no workspace folder");
            }

            let project = args.project.trim().trim_end_matches('/');
            state.workspace.project = Some(match project.starts_with('/') {
                true => project.to_string(),
                false => format!("/{project}"),
            });
            let words = |command: &str| -> Vec<String> {
                command.split_whitespace().map(str::to_string).collect()
            };
            let build = words(&args.build);
            // Without incremental analyses, every analysis is a full one.
            let command = words(&args.incremental_build);
            let enabled = args.incremental.unwrap_or(!command.is_empty());
            let incremental = if enabled { command } else { Vec::new() };
            let incremental_setting = if enabled { "Yes" } else { "No" };
            state.workspace.build = Some(build).filter(|build| !build.is_empty());
            state.workspace.incremental_build = Some(incremental).filter(|b| !b.is_empty());
            state.workspace.write()?;
            state
                .workspace
                .write_setting("INCREMENTAL_BUILD", incremental_setting)?;

            if !args.hub_address.trim().is_empty() {
                state
                    .workspace
                    .write_setting("HUB_ADDRESS", args.hub_address.trim())?;
            }
        }

        self.workspace_changed().await;
        self.project(ProjectArgs::default()).await
    }

    async fn watch_files(&self) {
        let conf = self.state.read().await.config.analyze.conf_file.clone();
        let conf_pattern = conf
            .file_name()
            .map(|name| format!("**/{}", name.to_string_lossy()));
        let options = DidChangeWatchedFilesRegistrationOptions {
            watchers: [Some(WATCH_PATTERN.to_string()), conf_pattern]
                .into_iter()
                .flatten()
                .map(|pattern| FileSystemWatcher {
                    glob_pattern: GlobPattern::String(pattern),
                    kind: None,
                })
                .collect(),
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

    /// Resolves warnings on the hub and hides them. The outcome is shown
    /// to the user, since code actions have no other way to tell it.
    async fn resolve(&self, args: ResolveArgs) -> anyhow::Result<Value> {
        let (hub, warnings, analysis_id, cache) = {
            let state = self.state.read().await;
            // (fingerprint, rule, instance id) of the warnings of the hub
            let warnings: Vec<(String, String, u64)> = state
                .store
                .select(&args.uri, args.fingerprint.as_deref(), args.range)
                .into_iter()
                .filter_map(|warning| {
                    let id = hub::warning_instance_id(warning.hub_uri.as_ref()?)?;
                    let rule = warning.rule_id.clone().unwrap_or("the warning".into());
                    Some((warning.fingerprint.clone(), rule, id))
                })
                .collect();
            let analysis_id = state.analysis.as_ref().and_then(|a| a.hub_id.clone());
            (
                Self::hub(&state)?,
                warnings,
                analysis_id,
                Cache::of(&state.effective_config()),
            )
        };
        let resolution = args.resolution;
        let mut resolved = Vec::new();
        let mut failure = warnings
            .is_empty()
            .then(|| "no warning of the hub to resolve here".to_string());

        for (fingerprint, rule, id) in warnings {
            match hub.resolve(id, resolution).await {
                Ok(()) => resolved.push((fingerprint, rule, id)),
                Err(error) => {
                    failure = Some(format!("{error:#}"));
                    break;
                }
            }
        }

        if !resolved.is_empty() {
            let status = {
                let mut state = self.state.write().await;

                if let Some(analysis) = &mut state.analysis {
                    for (fingerprint, _, _) in &resolved {
                        analysis.resolve(&args.uri, fingerprint, resolution);
                    }
                }

                state.relocate(&args.uri);
                state.status()
            };
            self.publish(std::slice::from_ref(&args.uri)).await;
            self.client
                .send_notification::<commands::Loaded>(status)
                .await;

            if let (Some(cache), Some(analysis_id)) = (cache, analysis_id) {
                let ids: Vec<u64> = resolved.iter().map(|(_, _, id)| *id).collect();
                self.mark_cached(cache, analysis_id, ids, resolution).await;
            }
        }

        let message = match (&resolved[..], &failure) {
            (_, Some(failure)) => failure.clone(),
            ([(_, rule, _)], None) => {
                format!("Marked '{rule}' as {} on the hub", resolution.describe())
            }
            (resolved, None) => format!(
                "Marked {} warnings as {} on the hub",
                resolved.len(),
                resolution.describe()
            ),
        };

        match failure {
            _ if args.quiet => {}
            Some(_) => self.error(&message).await,
            None => {
                self.client
                    .show_message(MessageType::INFO, format!("CodeSonar: {message}"))
                    .await;
            }
        }

        Ok(json!({"resolved": resolved.len(), "message": message}))
    }

    /// Records resolved warnings in the cached export of their analysis, so
    /// they stay hidden when it is loaded from the cache.
    async fn mark_cached(
        &self,
        cache: Cache,
        analysis_id: String,
        ids: Vec<u64>,
        resolution: Resolution,
    ) {
        let result = tokio::task::spawn_blocking(move || {
            let Some(mut sarif) = cache.get(&analysis_id) else {
                return Ok(());
            };

            for id in ids {
                if let Some(marked) = triage::mark(&sarif, id, resolution) {
                    sarif = marked;
                }
            }

            cache.put(&analysis_id, &sarif)
        })
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.warn(format!("{error:#}")).await,
            Err(error) => self.warn(error.to_string()).await,
        }
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
            .and_then(warning::web_url)
            .ok_or_else(|| Error::invalid_params("expected the web link of a warning"))?;

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

        let can_show_progress = params
            .capabilities
            .window
            .as_ref()
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false);

        let mut state = self.state.write().await;
        state.root = workspace_root(&params);
        state.local = state.root.as_deref().and_then(detect::local_project);
        state
            .configure(config)
            .map_err(|e| Error::invalid_params(format!("{e:#}")))?;
        state.can_watch_files = can_watch_files;
        state.can_show_progress = can_show_progress;

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
        let (sarif_file, conf) = {
            let state = self.state.read().await;
            (
                state.config.sarif_file(state.root.as_deref()),
                state.workspace.file.clone(),
            )
        };
        let changed = |file: &Path| {
            params
                .changes
                .iter()
                .any(|change| change.uri.to_file_path().ok().as_deref() == Some(file))
        };

        if conf.as_deref().is_some_and(changed) {
            self.workspace_changed().await;
            return;
        }

        let Some(sarif_file) = sarif_file else {
            return;
        };

        if changed(&sarif_file) {
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
        let on_hub = state.hub.is_some()
            && state
                .analysis
                .as_ref()
                .is_some_and(|analysis| analysis.hub_id.is_some());
        let offers = code_action::Offers {
            resolve: on_hub,
            disable: state.workspace.file.is_some(),
        };
        let actions = code_action::code_actions(
            &state.store,
            &params.text_document.uri,
            params.range,
            offers,
        );
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
            commands::DELETE_ANALYSIS => self.delete_analysis(required(arguments)?).await,
            commands::LOAD => self.load_command(parse(arguments)?).await,
            commands::ANALYSIS_OPTIONS => self.analysis_options().await,
            commands::CONFIGURE_ANALYSIS => self.configure_analysis(parse(arguments)?).await,
            commands::DISABLE_CHECK => self.disable_check(required(arguments)?).await,
            commands::UNCACHE => {
                let args: DeleteAnalysisArgs = required(arguments)?;

                match self.uncache(args.analysis).await {
                    Ok(()) => self.analyses().await,
                    Err(error) => Err(error),
                }
            }
            commands::HIDE => {
                self.unload().await;
                Ok(self.state.read().await.status())
            }
            commands::TOGGLE => self.toggle().await,
            commands::EXPLAIN_COMMAND => Ok(self.explain(required(arguments)?).await),
            commands::RESOLVE_COMMAND => self.resolve(required(arguments)?).await,
            commands::STATUS => Ok(self.state.read().await.status()),
            commands::WARNINGS => Ok(self.state.read().await.warnings()),
            commands::ANALYZE_COMMAND => self.analyze_command(parse(arguments)?).await,
            commands::PROJECT => self.project(parse(arguments)?).await,
            commands::INIT_CONF => self.init_conf(required(arguments)?).await,
            commands::PRESETS => {
                let state = self.state.read().await;
                analyze::presets(&state.effective_config()).map(|presets| json!(presets))
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
