//! Commands for clients, run with `workspace/executeCommand`.
//!
//! Clients build their own views on them, e.g. a list of analyses to
//! choose from. Each takes at most one argument, an object.

use serde::Deserialize;
use serde_json::Value;
use tower_lsp::lsp_types::notification::Notification;
use tower_lsp::lsp_types::{Range, Url};

pub use crate::code_action::{EXPLAIN_COMMAND, OPEN_IN_HUB_COMMAND, REFRESH_COMMAND};

/// Sets the hub and signs in: `{url, username, password, project}`, all
/// optional. Returns the hub projects.
pub const CONNECT: &str = "codesonar.connect";
/// Returns the hub projects: `[{id, name}]`.
pub const PROJECTS: &str = "codesonar.projects";
/// Returns the analyses of the hub project, newest first:
/// `{project, analyses: [{id, name, started, state, finished, current,
/// cached}]}`. Cached analyses were downloaded before, so they show up
/// quickly.
pub const ANALYSES: &str = "codesonar.analyses";
/// Deletes an analysis from the hub: `{analysis: id}`. Returns the
/// analyses like [`ANALYSES`] with `deleted`, and the reason in `message`
/// if the hub refused, e.g. without permission. If the shown analysis was
/// deleted, the newest finished one is loaded afterwards, or none is shown
/// without one.
pub const DELETE_ANALYSIS: &str = "codesonar.deleteAnalysis";
/// Removes an analysis from the cache of downloaded analyses:
/// `{analysis: id}`. Returns the analyses like [`ANALYSES`].
pub const UNCACHE: &str = "codesonar.uncache";
/// Loads an analysis: `{analysis: id}`, `{file: path}`, `{latest: true}`
/// or nothing for the configured one. Returns the status.
pub const LOAD: &str = "codesonar.load";
/// Hides the shown analysis and its warnings, until an analysis is loaded
/// with [`LOAD`]. Returns the status.
pub const HIDE: &str = "codesonar.hide";
/// Hides the shown analysis like [`HIDE`], or shows the hidden one again.
/// Returns the status.
pub const TOGGLE: &str = "codesonar.toggle";
/// Returns the status of the loaded analysis, see [`LOADED`].
pub const STATUS: &str = "codesonar.status";
/// Returns the shown warnings: `[{uri, range, severity, code, message,
/// status}]`.
pub const WARNINGS: &str = "codesonar.warnings";
/// Returns the command line of an analysis of the workspace: `{name}` is
/// optional. Returns `{command, cwd, temporaryFiles}`.
pub const ANALYZE_COMMAND: &str = "codesonar.analyzeCommand";
/// Returns the CodeSonar project of the workspace: `{prefix, conf,
/// confExists, created, incremental, workspaceConf, workspaceConfExists,
/// section, suggested: {project, build, incremental, incrementalBuild,
/// hubAddress}}`.
/// With `{createConf: true}`, the configuration file is created from
/// CodeSonar's template if it does not exist. `workspaceConf` is the
/// configuration file of the workspace, `section` whether it has the
/// section of codesonar-lsp, and `suggested` what [`INIT_CONF`] could start
/// it with.
pub const PROJECT: &str = "codesonar.project";
/// Starts the workspace configuration: `{project, build, incremental,
/// incrementalBuild, hubAddress}`. The project and the build commands are
/// written into its section, `INCREMENTAL_BUILD` and `HUB_ADDRESS` outside
/// of it.
/// Returns the project like [`PROJECT`].
pub const INIT_CONF: &str = "codesonar.initConf";

/// Returns the presets and checks that analyses can use: `{conf, presets:
/// [{name, description, selected}], checks: [{class, language, default,
/// enabled}]}`. Both come from the CodeSonar installation, the choice from
/// the workspace configuration `conf`.
pub const ANALYSIS_OPTIONS: &str = "codesonar.analysisOptions";
/// Writes presets and checks into the section of codesonar-lsp in the
/// workspace configuration: `{presets: [name], checks: [{class, enabled}]}`,
/// both optional. Returns the options like [`ANALYSIS_OPTIONS`].
pub const CONFIGURE_ANALYSIS: &str = "codesonar.configureAnalysis";
/// Returns the presets of the CodeSonar installation: `[{name,
/// description, selected}]`. The setting `analyze.presets` selects them.
pub const PRESETS: &str = "codesonar.presets";

pub const ALL: [&str; 19] = [
    REFRESH_COMMAND,
    OPEN_IN_HUB_COMMAND,
    EXPLAIN_COMMAND,
    CONNECT,
    PROJECTS,
    ANALYSES,
    DELETE_ANALYSIS,
    UNCACHE,
    LOAD,
    HIDE,
    TOGGLE,
    STATUS,
    WARNINGS,
    ANALYZE_COMMAND,
    PROJECT,
    INIT_CONF,
    PRESETS,
    ANALYSIS_OPTIONS,
    CONFIGURE_ANALYSIS,
];

/// Sent after an analysis was loaded and once more after the analyzed
/// code was read from the hub, with the status:
/// `{loaded, hidden, hub, project, presets, conf, name, key, hubId, url,
/// total, shown, counts}`. Without a loaded analysis, only the first six
/// are set. `hidden` tells whether the analysis was hidden with [`HIDE`],
/// `conf` is the workspace configuration, if it exists.
pub enum Loaded {}

impl Notification for Loaded {
    type Params = Value;
    const METHOD: &'static str = "codesonar/loaded";
}

/// Sent when an analysis starts loading, which can take a minute for large
/// ones: `{analysis}` with its hub id, `{project}` for its newest analysis
/// or `{file}` for a SARIF file.
pub enum Loading {}

impl Notification for Loading {
    type Params = Value;
    const METHOD: &'static str = "codesonar/loading";
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConfigureAnalysisArgs {
    pub presets: Option<Vec<String>>,
    pub checks: Vec<CheckChoice>,
}

#[derive(Debug, Deserialize)]
pub struct CheckChoice {
    pub class: String,
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct DeleteAnalysisArgs {
    pub analysis: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConnectArgs {
    pub url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LoadArgs {
    pub analysis: Option<Value>,
    pub file: Option<String>,
    pub latest: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AnalyzeArgs {
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectArgs {
    pub create_conf: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitConfArgs {
    pub project: String,
    /// The full rebuild of the first analysis.
    #[serde(default)]
    pub build: String,
    /// Whether analyses are incremental (`INCREMENTAL_BUILD`), by default
    /// if there is an incremental build. Without, every analysis is a full
    /// one.
    #[serde(default)]
    pub incremental: Option<bool>,
    /// The build of later, incremental analyses.
    #[serde(default)]
    pub incremental_build: String,
    #[serde(default)]
    pub hub_address: String,
}

/// Explains the warnings of a file: `{uri, fingerprint}` picks one by its
/// fingerprint, `{uri, range}` those in a range. Returns
/// `[{title, ruleId, message, description, status, related: [{uri, line,
/// message}], events: [{number, uri, line, message, depth}], rules: [{id,
/// title, relationship, url}], helpUri, hubUri, markdown}]`. The titles of
/// the rules come from the hub.
#[derive(Debug, Deserialize)]
pub struct ExplainArgs {
    pub uri: Url,
    pub fingerprint: Option<String>,
    pub range: Option<Range>,
}

/// Parses the first argument of a command; a missing one is the default.
pub fn args<T: Default + for<'de> Deserialize<'de>>(arguments: &[Value]) -> Result<T, String> {
    match arguments.first() {
        None | Some(Value::Null) => Ok(T::default()),
        Some(value) => T::deserialize(value).map_err(|error| error.to_string()),
    }
}
