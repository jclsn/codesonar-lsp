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
/// `{project, analyses: [{id, name, started, state, finished, current}]}`.
pub const ANALYSES: &str = "codesonar.analyses";
/// Loads an analysis: `{analysis: id}`, `{file: path}`, `{latest: true}`
/// or nothing for the configured one. Returns the status.
pub const LOAD: &str = "codesonar.load";
/// Returns the status of the loaded analysis, see [`LOADED`].
pub const STATUS: &str = "codesonar.status";
/// Returns the shown warnings: `[{uri, range, severity, code, message,
/// status}]`.
pub const WARNINGS: &str = "codesonar.warnings";
/// Returns the command line of an analysis of the workspace: `{name}` is
/// optional. Returns `{command, cwd, temporaryFiles}`.
pub const ANALYZE_COMMAND: &str = "codesonar.analyzeCommand";
/// Returns the CodeSonar project of the workspace: `{prefix, conf,
/// confExists, created}`. With `{createConf: true}`, the configuration file
/// is created from CodeSonar's template if it does not exist.
pub const PROJECT: &str = "codesonar.project";

pub const ALL: [&str; 11] = [
    REFRESH_COMMAND,
    OPEN_IN_HUB_COMMAND,
    EXPLAIN_COMMAND,
    CONNECT,
    PROJECTS,
    ANALYSES,
    LOAD,
    STATUS,
    WARNINGS,
    ANALYZE_COMMAND,
    PROJECT,
];

/// Sent after an analysis was loaded and once more after the analyzed
/// code was read from the hub, with the status:
/// `{loaded, name, key, hubId, url, total, counts}`.
pub enum Loaded {}

impl Notification for Loaded {
    type Params = Value;
    const METHOD: &'static str = "codesonar/loaded";
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
