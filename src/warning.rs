//! CodeSonar warnings, converted from SARIF results.

use std::collections::HashMap;
use std::path::Path;

use percent_encoding::percent_decode_str;
use tower_lsp::lsp_types::{
    CodeDescription, Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location,
    NumberOrString, Position, Range, TextEdit, Url,
};

use crate::config::Config;
use crate::sarif::{self, ArtifactLocation, Level, Log, Region, Run, SarifResult};
use crate::tracking::Status;
use crate::triage::Resolution;

pub const SOURCE: &str = "codesonar";
const CHANGED_PREFIX: &str = "[changed since the analysis] ";

#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub uri: Url,
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub rule_id: Option<String>,
    pub rule_name: Option<String>,
    /// Long description of the warning class, if the export contains it.
    pub description: Option<String>,
    pub help_uri: Option<Url>,
    /// Link to the warning on the CodeSonar hub.
    pub hub_uri: Option<Url>,
    pub related: Vec<DiagnosticRelatedInformation>,
    pub fixes: Vec<QuickFix>,
    /// The events on the path to the warning, in order.
    pub events: Vec<Event>,
    /// Identifies the warning across analyses.
    pub fingerprint: String,
    /// Whether the code of the warning changed since the analysis.
    pub status: Status,
    /// Whether the warning was resolved on the hub, e.g. as fixed.
    pub resolution: Option<Resolution>,
}

/// A rule of a coding standard, or the warning class itself, that a
/// warning relates to, e.g. `CWE:415`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Rule {
    pub id: String,
    /// E.g. "Double Free". Only the hub knows it.
    pub title: Option<String>,
    /// How the warning relates to the rule: `intersects` if it may violate
    /// it, `follows` if it may be a symptom of an earlier violation.
    pub relationship: Option<String>,
    pub url: Option<Url>,
}

impl Rule {
    /// Whether this is the documentation of the warning class, like
    /// `ALLOC.DF`, rather than a rule of a standard, like `CWE:415`.
    pub fn is_warning_class(&self) -> bool {
        !self.id.contains(':')
    }
}

/// A step on the path to a warning, e.g. where a pointer is freed.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The number the hub shows; calls and returns have none.
    pub number: Option<u32>,
    pub location: Location,
    pub message: String,
    /// Call depth, 0 for the procedure of the warning.
    pub depth: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuickFix {
    pub title: String,
    pub changes: HashMap<Url, Vec<TextEdit>>,
}

impl Warning {
    /// The diagnostic of the warning. Warnings in changed code are hints.
    pub fn to_diagnostic(&self) -> Diagnostic {
        let href = self.help_uri.clone().or_else(|| self.hub_uri.clone());
        let changed = self.status == Status::Changed;
        let message = if changed {
            format!("{CHANGED_PREFIX}{}", self.message)
        } else {
            self.message.clone()
        };
        Diagnostic {
            range: self.range,
            severity: Some(if changed {
                DiagnosticSeverity::HINT
            } else {
                self.severity
            }),
            code: self.rule_id.clone().map(NumberOrString::String),
            code_description: href.map(|href| CodeDescription { href }),
            source: Some(SOURCE.to_string()),
            message,
            related_information: (!self.related.is_empty()).then(|| self.related.clone()),
            ..Diagnostic::default()
        }
    }
}

/// A link that may be opened in a browser: only `http` and `https`. Links
/// come from analyses and the hub, and other schemes, like `file`, can
/// start other programs.
pub fn web_url(url: Url) -> Option<Url> {
    matches!(url.scheme(), "http" | "https").then_some(url)
}

/// Converts all results of a SARIF log into warnings.
///
/// Results whose file cannot be resolved to a local path are skipped.
pub fn from_log(log: &Log, config: &Config, root: Option<&Path>) -> Vec<Warning> {
    let mut warnings = Vec::new();

    for run in &log.runs {
        let resolver = Resolver { run, config, root };

        for result in &run.results {
            if let Some(warning) = resolver.warning(result) {
                warnings.push(warning);
            }
        }
    }

    warnings
}

struct Resolver<'a> {
    run: &'a Run,
    config: &'a Config,
    root: Option<&'a Path>,
}

impl Resolver<'_> {
    fn warning(&self, result: &SarifResult) -> Option<Warning> {
        let location = result.locations.first()?.physical_location.as_ref()?;
        let uri = self.uri(location.artifact_location.as_ref()?)?;
        let range = location.region.map(to_range).unwrap_or_default();
        let rule = self.run.rule_for(result);

        let level = result
            .level
            .or_else(|| rule?.default_configuration.as_ref()?.level)
            .unwrap_or(Level::Warning);
        let message = message_text(&result.message)
            .or_else(|| Some(rule?.short_description.as_ref()?.text.clone()))
            .unwrap_or_else(|| "CodeSonar warning".to_string());
        let description = rule.and_then(|rule| {
            let text = rule.help.as_ref().or(rule.full_description.as_ref())?;
            Some(text.best().to_string())
        });

        let fingerprint = result.fingerprint().map_or_else(
            || {
                let key = format!(
                    "{}|{}|{}|{}",
                    result.rule_id().unwrap_or_default(),
                    uri,
                    range.start.line,
                    message
                );
                format!("{:016x}", fnv1a(key.as_bytes()))
            },
            str::to_string,
        );

        Some(Warning {
            fingerprint,
            status: Status::default(),
            resolution: Resolution::of(&result.properties),
            uri,
            range,
            severity: to_severity(level),
            rule_id: result.rule_id().map(str::to_string),
            rule_name: rule.and_then(|rule| rule.name.clone()),
            description,
            help_uri: rule
                .and_then(|rule| rule.help_uri.as_deref())
                .and_then(|uri| Url::parse(uri).ok())
                .and_then(web_url),
            hub_uri: result
                .hosted_viewer_uri
                .as_deref()
                .and_then(|uri| Url::parse(uri).ok())
                .and_then(web_url),
            related: result
                .related_locations
                .iter()
                .filter_map(|location| self.related(location))
                .collect(),
            fixes: result
                .fixes
                .iter()
                .filter_map(|fix| self.fix(fix))
                .collect(),
            events: self.events(result, &message),
            message,
        })
    }

    /// The events of the first code flow, without the warning itself,
    /// which CodeSonar adds as the last event.
    fn events(&self, result: &SarifResult, message: &str) -> Vec<Event> {
        let locations = result
            .code_flows
            .iter()
            .flat_map(|flow| &flow.thread_flows)
            .map(|thread| &thread.locations)
            .next();
        let mut events: Vec<Event> = locations
            .into_iter()
            .flatten()
            .filter_map(|step| {
                let location = step.location.as_ref()?;
                let physical = location.physical_location.as_ref()?;
                Some(Event {
                    number: location.id,
                    location: Location {
                        uri: self.uri(physical.artifact_location.as_ref()?)?,
                        range: physical.region.map(to_range).unwrap_or_default(),
                    },
                    message: location
                        .message
                        .as_ref()
                        .and_then(message_text)
                        .map(|text| inline_references(&text))
                        .unwrap_or_default(),
                    depth: step.nesting_level.unwrap_or(1).saturating_sub(1),
                })
            })
            .collect();

        if events.last().is_some_and(|event| event.message == message) {
            events.pop();
        }

        events
    }

    fn related(&self, location: &sarif::Location) -> Option<DiagnosticRelatedInformation> {
        let physical = location.physical_location.as_ref()?;
        Some(DiagnosticRelatedInformation {
            location: Location {
                uri: self.uri(physical.artifact_location.as_ref()?)?,
                range: physical.region.map(to_range).unwrap_or_default(),
            },
            message: location
                .message
                .as_ref()
                .and_then(message_text)
                .unwrap_or_default(),
        })
    }

    /// Converts a SARIF fix. Fixes with unusable regions are dropped as a whole,
    /// so that no partial edit is ever applied.
    fn fix(&self, fix: &sarif::Fix) -> Option<QuickFix> {
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        for change in &fix.artifact_changes {
            let uri = self.uri(&change.artifact_location)?;

            for replacement in &change.replacements {
                replacement.deleted_region.start_line?;
                let new_text = replacement
                    .inserted_content
                    .as_ref()
                    .and_then(|content| content.text.clone())
                    .unwrap_or_default();
                changes.entry(uri.clone()).or_default().push(TextEdit {
                    range: to_range(replacement.deleted_region),
                    new_text,
                });
            }
        }

        if changes.is_empty() {
            return None;
        }

        let title = fix
            .description
            .as_ref()
            .and_then(message_text)
            .unwrap_or_else(|| "Apply CodeSonar fix".to_string());
        Some(QuickFix { title, changes })
    }

    fn uri(&self, location: &ArtifactLocation) -> Option<Url> {
        let reference = self.reference(location, 0)?;
        let artifact_uri = location.uri.as_deref().unwrap_or_default();
        let path = match reference.strip_prefix("file:") {
            // Skip the authority of `file://host/path`.
            Some(rest) => match rest.strip_prefix("//") {
                Some(rest) => &rest[rest.find('/').unwrap_or(rest.len())..],
                None => rest,
            },
            None => reference.as_str(),
        };
        let path = percent_decode_str(path).decode_utf8().ok()?;
        let path = self.config.map_path(&path);
        let path = Path::new(&path);

        if path.is_absolute() {
            return self.absolute_uri(path, artifact_uri);
        }

        Url::from_file_path(self.root?.join(path)).ok()
    }

    fn absolute_uri(&self, path: &Path, artifact_uri: &str) -> Option<Url> {
        if path.exists() {
            return Url::from_file_path(path).ok();
        }

        if let Some(workspace_uri) = self.workspace_uri(artifact_uri) {
            return Some(workspace_uri);
        }

        Url::from_file_path(path).ok()
    }

    /// Resolves a relative SARIF artifact against the workspace.
    fn workspace_uri(&self, artifact_uri: &str) -> Option<Url> {
        if artifact_uri.is_empty() {
            return None;
        }

        if Path::new(artifact_uri).is_absolute() {
            return None;
        }

        let root = self.root?;
        let artifact_uri = percent_decode_str(artifact_uri).decode_utf8().ok()?;
        let workspace_path = root.join(artifact_uri.as_ref());

        if !workspace_path.exists() {
            return None;
        }

        Url::from_file_path(workspace_path).ok()
    }

    /// Builds the full URI reference by following `uriBaseId` chains.
    fn reference(&self, location: &ArtifactLocation, depth: usize) -> Option<String> {
        let uri = location.uri.clone().unwrap_or_default();
        let Some(base_id) = &location.uri_base_id else {
            return Some(uri);
        };
        // Unknown base ids are treated as the workspace root.
        let Some(base) = self.run.original_uri_base_ids.get(base_id) else {
            return Some(uri);
        };

        if depth > 8 {
            return None;
        }

        let mut base = self.reference(base, depth + 1)?;

        if !base.is_empty() && !base.ends_with('/') {
            base.push('/');
        }

        Some(base + &uri)
    }
}

fn message_text(message: &sarif::Message) -> Option<String> {
    message
        .text
        .clone()
        .or_else(|| message.markdown.clone())
        .filter(|text| !text.is_empty())
}

/// A hash that stays the same across builds, unlike the one of `std`.
/// CodeSonar appends the location of a value in double brackets, e.g.
/// `malloc(n)[[utils.c:13]]`: shows it as `malloc(n) (utils.c:13)`.
fn inline_references(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("[[") {
        let Some(end) = rest[start..].find("]]") else {
            break;
        };
        result.push_str(&rest[..start]);
        result.push_str(&format!(" ({})", &rest[start + 2..start + end]));
        rest = &rest[start + end + 2..];
    }

    result.push_str(rest);
    result
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn to_severity(level: Level) -> DiagnosticSeverity {
    match level {
        Level::Error => DiagnosticSeverity::ERROR,
        Level::Warning => DiagnosticSeverity::WARNING,
        Level::Note => DiagnosticSeverity::INFORMATION,
        Level::None => DiagnosticSeverity::HINT,
    }
}

/// Converts a 1-based SARIF region into a 0-based LSP range.
///
/// Without an end column the range covers the rest of the end line.
pub fn to_range(region: Region) -> Range {
    let start_line = region.start_line.unwrap_or(1).saturating_sub(1);
    let start_char = region.start_column.unwrap_or(1).saturating_sub(1);
    let end_line = region
        .end_line
        .map_or(start_line, |line| line.saturating_sub(1))
        .max(start_line);
    let end = match region.end_column {
        Some(column) => Position::new(end_line, column.saturating_sub(1)),
        None => Position::new(end_line + 1, 0),
    };
    Range::new(Position::new(start_line, start_char), end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PathMapping;

    const FIXTURE: &str = include_str!("../tests/fixtures/codesonar.sarif");

    fn config() -> Config {
        Config {
            path_mappings: vec![PathMapping {
                from: "/build".into(),
                to: "/ws".into(),
            }],
            ..Config::default()
        }
    }

    fn warnings() -> Vec<Warning> {
        let log = Log::from_json(FIXTURE).unwrap();
        from_log(&log, &config(), Some(Path::new("/ws")))
    }

    fn url(path: &str) -> Url {
        Url::from_file_path(path).unwrap()
    }

    #[test]
    fn converts_results() {
        let warnings = warnings();
        assert_eq!(warnings.len(), 3, "result without location is skipped");

        let npd = &warnings[0];
        assert_eq!(npd.uri, url("/ws/src/main.c"));
        assert_eq!(
            npd.range,
            Range::new(Position::new(4, 4), Position::new(4, 6))
        );
        assert_eq!(npd.severity, DiagnosticSeverity::ERROR);
        assert_eq!(npd.rule_id.as_deref(), Some("LANG.MEM.NPD"));
        assert_eq!(npd.rule_name.as_deref(), Some("Null Pointer Dereference"));
        assert_eq!(
            npd.description.as_deref(),
            Some("Check pointers **before** use.")
        );
        assert_eq!(
            npd.hub_uri.as_ref().unwrap().as_str(),
            "https://hub.example.com:7340/warninginstance/12.html"
        );

        let fmt = &warnings[1];
        assert_eq!(fmt.uri, url("/ws/src/util file.c"));
        assert_eq!(fmt.severity, DiagnosticSeverity::INFORMATION);
        assert_eq!(fmt.description, None);

        assert_eq!(warnings[2].uri, url("/usr/include/stdio.h"));
        assert_eq!(warnings[2].severity, DiagnosticSeverity::WARNING);
    }

    #[test]
    fn resolves_unavailable_analysis_root_against_workspace() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/main.c"), "").unwrap();

        let log = Log::from_json(
            r#"
            {
                "runs": [
                    {
                        "originalUriBaseIds": {
                            "SRCROOT": {
                                "uri": "file:///builds/agent/project/"
                            }
                        },
                        "results": [
                            {
                                "message": { "text": "warning" },
                                "locations": [
                                    {
                                        "physicalLocation": {
                                            "artifactLocation": {
                                                "uri": "src/main.c",
                                                "uriBaseId": "SRCROOT"
                                            },
                                            "region": { "startLine": 2 }
                                        }
                                    }
                                ]
                            }
                        ]
                    }
                ]
            }
            "#,
        )
        .unwrap();

        let warnings = from_log(&log, &Config::default(), Some(root.path()));

        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].uri,
            Url::from_file_path(root.path().join("src/main.c")).unwrap()
        );
    }

    #[test]
    fn converts_related_locations() {
        let related = &warnings()[0].related;
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].message, "'p' is set to NULL");
        assert_eq!(related[0].location.uri, url("/ws/src/main.c"));
        assert_eq!(related[0].location.range.start, Position::new(2, 0));
    }

    #[test]
    fn converts_fixes() {
        let fixes = &warnings()[0].fixes;
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].title, "Check 'p' for NULL");

        let edits = &fixes[0].changes[&url("/ws/src/main.c")];
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "if (p) ");
        assert_eq!(edits[0].range.start, edits[0].range.end);
        assert_eq!(edits[0].range.start, Position::new(4, 4));
    }

    #[test]
    fn drops_fixes_without_regions() {
        let log = Log::from_json(
            r#"{"runs": [{"results": [{
                "message": {"text": "m"},
                "locations": [{"physicalLocation": {"artifactLocation": {"uri": "a.c"}}}],
                "fixes": [{"artifactChanges": [{
                    "artifactLocation": {"uri": "a.c"},
                    "replacements": [{"deletedRegion": {"charOffset": 3}}]
                }]}]
            }]}]}"#,
        )
        .unwrap();

        let warnings = from_log(&log, &Config::default(), Some(Path::new("/ws")));

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].fixes.is_empty());
        assert!(from_log(&log, &Config::default(), None).is_empty());
    }

    #[test]
    fn keeps_absolute_paths_without_root() {
        let log = Log::from_json(FIXTURE).unwrap();
        let warnings = from_log(&log, &Config::default(), None);

        assert!(warnings.iter().all(|w| w.uri.scheme() == "file"));
        assert_eq!(warnings.len(), 3);
        assert_eq!(warnings[0].uri, url("/build/src/main.c"));
    }

    #[test]
    fn builds_diagnostics() {
        let diagnostic = warnings()[0].to_diagnostic();
        assert_eq!(diagnostic.source.as_deref(), Some(SOURCE));
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String("LANG.MEM.NPD".into()))
        );
        assert_eq!(
            diagnostic.code_description.unwrap().href.as_str(),
            "https://hub.example.com:7340/doc/LANG.MEM.NPD.html"
        );
        assert_eq!(diagnostic.message, "Null pointer dereference of 'p'");
        assert_eq!(diagnostic.related_information.unwrap().len(), 1);

        let diagnostic = warnings()[1].to_diagnostic();
        assert_eq!(diagnostic.code_description, None);
        assert_eq!(diagnostic.related_information, None);
    }

    #[test]
    fn accepts_only_web_links() {
        let mut log: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        log["runs"][0]["results"][0]["hostedViewerUri"] = "file:///etc/passwd".into();
        log["runs"][0]["tool"]["driver"]["rules"][0]["helpUri"] = "javascript:alert(1)".into();
        let log = Log::from_json(&log.to_string()).unwrap();

        let warnings = from_log(&log, &config(), Some(Path::new("/ws")));

        assert_eq!(warnings[0].hub_uri, None);
        assert_eq!(warnings[0].help_uri, None);

        assert!(web_url(Url::parse("https://hub:7340/").unwrap()).is_some());
        assert!(web_url(Url::parse("http://hub:7340/").unwrap()).is_some());
        assert!(web_url(Url::parse("file:///etc/passwd").unwrap()).is_none());
    }

    #[test]
    fn reads_the_resolution_on_the_hub() {
        assert!(warnings().iter().all(|w| w.resolution.is_none()));

        let mut log: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        log["runs"][0]["results"][0]["properties"] =
            serde_json::json!({"state": "Fixed", "finding": "None"});
        let log = Log::from_json(&log.to_string()).unwrap();

        let warnings = from_log(&log, &config(), Some(Path::new("/ws")));

        assert_eq!(warnings[0].resolution, Some(Resolution::Fixed));
        assert_eq!(warnings[1].resolution, None);
    }

    #[test]
    fn marks_warnings_in_changed_code() {
        let warning = Warning {
            status: Status::Changed,
            ..warnings()[0].clone()
        };

        let diagnostic = warning.to_diagnostic();

        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::HINT));
        assert_eq!(
            diagnostic.message,
            "[changed since the analysis] Null pointer dereference of 'p'"
        );
    }

    #[test]
    fn identifies_warnings_without_fingerprint() {
        let warnings = warnings();
        assert_eq!(warnings[0].fingerprint.len(), 16);
        assert_ne!(warnings[0].fingerprint, warnings[1].fingerprint);
        assert_eq!(
            warnings[0].fingerprint,
            self::warnings()[0].fingerprint,
            "stable"
        );
    }

    #[test]
    fn converts_regions() {
        let region = |start_line, start_column, end_line, end_column| Region {
            start_line,
            start_column,
            end_line,
            end_column,
        };

        let range = |a, b, c, d| Range::new(Position::new(a, b), Position::new(c, d));

        assert_eq!(
            to_range(region(Some(3), None, None, None)),
            range(2, 0, 3, 0)
        );
        assert_eq!(
            to_range(region(Some(3), Some(2), Some(4), Some(9))),
            range(2, 1, 3, 8)
        );
        assert_eq!(
            to_range(region(Some(5), None, Some(1), None)),
            range(4, 0, 5, 0)
        );
        assert_eq!(to_range(Region::default()), range(0, 0, 1, 0));
        assert_eq!(
            to_range(region(Some(0), Some(0), None, Some(0))),
            range(0, 0, 0, 0)
        );
    }

    #[test]
    fn converts_events_without_the_warning_itself() {
        let warning = &warnings()[0];
        assert_eq!(warning.events.len(), 2);

        let first = &warning.events[0];
        assert_eq!(first.number, Some(1));
        assert_eq!(first.location.uri, url("/ws/src/main.c"));
        assert_eq!(first.location.range.start.line, 2);
        assert_eq!(first.message, "p is set to lookup() (util.c:4).");
        assert_eq!(first.depth, 0);

        let call = &warning.events[1];
        assert_eq!(call.number, None);
        assert_eq!(call.location.uri, url("/ws/src/util file.c"));
        assert_eq!(call.depth, 1);
        assert!(warnings()[1].events.is_empty());
    }

    #[test]
    fn inlines_references() {
        assert_eq!(
            inline_references("a[[x.c:1]] b[[y.c:2]]"),
            "a (x.c:1) b (y.c:2)"
        );
        assert_eq!(inline_references("no [[end"), "no [[end");
    }
}
