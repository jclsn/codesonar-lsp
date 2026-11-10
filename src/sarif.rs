//! Subset of the SARIF 2.1.0 format as exported by CodeSonar.

use std::collections::HashMap;

use serde::Deserialize;

const CODESONAR_FINGERPRINT: &str = "codesonarFingerprint";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Log {
    pub runs: Vec<Run>,
}

impl Log {
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(text)?)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Run {
    pub tool: Tool,
    pub invocations: Vec<Invocation>,
    pub results: Vec<SarifResult>,
    pub original_uri_base_ids: HashMap<String, ArtifactLocation>,
}

impl Run {
    /// Finds the rule a result refers to, by index or by id.
    pub fn rule_for(&self, result: &SarifResult) -> Option<&ReportingDescriptor> {
        let rules = &self.tool.driver.rules;
        let index = result
            .rule_index
            .or_else(|| result.rule.as_ref().and_then(|r| r.index));

        if let Some(rule) = index.and_then(|i| rules.get(i)) {
            return Some(rule);
        }

        let id = result.rule_id()?;

        rules.iter().find(|rule| rule.id == id)
    }

    /// When the analysis finished, as an RFC 3339 timestamp.
    pub fn end_time(&self) -> Option<&str> {
        let invocation = self.invocations.first()?;

        invocation
            .end_time_utc
            .as_deref()
            .or(invocation.start_time_utc.as_deref())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Invocation {
    pub start_time_utc: Option<String>,
    pub end_time_utc: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Tool {
    pub driver: ToolComponent,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ToolComponent {
    pub name: String,
    pub rules: Vec<ReportingDescriptor>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReportingDescriptor {
    pub id: String,
    pub name: Option<String>,
    pub short_description: Option<MultiformatMessage>,
    pub full_description: Option<MultiformatMessage>,
    pub help: Option<MultiformatMessage>,
    pub help_uri: Option<String>,
    pub default_configuration: Option<ReportingConfiguration>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ReportingConfiguration {
    pub level: Option<Level>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ReportingDescriptorReference {
    pub id: Option<String>,
    pub index: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MultiformatMessage {
    pub text: String,
    pub markdown: Option<String>,
}

impl MultiformatMessage {
    /// Returns the markdown variant if present, otherwise the plain text.
    pub fn best(&self) -> &str {
        self.markdown.as_deref().unwrap_or(&self.text)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Message {
    pub text: Option<String>,
    pub markdown: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    None,
    Note,
    Warning,
    Error,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SarifResult {
    pub rule_id: Option<String>,
    pub rule_index: Option<usize>,
    pub rule: Option<ReportingDescriptorReference>,
    pub level: Option<Level>,
    pub message: Message,
    pub locations: Vec<Location>,
    pub related_locations: Vec<Location>,
    pub fixes: Vec<Fix>,
    pub code_flows: Vec<CodeFlow>,
    pub hosted_viewer_uri: Option<String>,
    pub fingerprints: HashMap<String, String>,
    pub partial_fingerprints: HashMap<String, String>,
}

impl SarifResult {
    /// CodeSonar's fingerprint, which identifies a warning across analyses.
    pub fn fingerprint(&self) -> Option<&str> {
        self.fingerprints
            .get(CODESONAR_FINGERPRINT)
            .or_else(|| self.partial_fingerprints.get(CODESONAR_FINGERPRINT))
            .or_else(|| self.fingerprints.values().next())
            .map(String::as_str)
    }

    pub fn rule_id(&self) -> Option<&str> {
        self.rule_id
            .as_deref()
            .or_else(|| self.rule.as_ref().and_then(|r| r.id.as_deref()))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Location {
    /// Numbers the events of a warning, as the hub shows them.
    pub id: Option<u32>,
    pub physical_location: Option<PhysicalLocation>,
    pub message: Option<Message>,
}

/// The path through the code that leads to a warning.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CodeFlow {
    pub thread_flows: Vec<ThreadFlow>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThreadFlow {
    pub locations: Vec<ThreadFlowLocation>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThreadFlowLocation {
    pub location: Option<Location>,
    /// Call depth, 1 for the procedure of the warning.
    pub nesting_level: Option<u32>,
    pub kinds: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PhysicalLocation {
    pub artifact_location: Option<ArtifactLocation>,
    pub region: Option<Region>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArtifactLocation {
    pub uri: Option<String>,
    pub uri_base_id: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Region {
    pub start_line: Option<u32>,
    pub start_column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Fix {
    pub description: Option<Message>,
    pub artifact_changes: Vec<ArtifactChange>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArtifactChange {
    pub artifact_location: ArtifactLocation,
    pub replacements: Vec<Replacement>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Replacement {
    pub deleted_region: Region,
    pub inserted_content: Option<ArtifactContent>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ArtifactContent {
    pub text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r#"{
        "version": "2.1.0",
        "runs": [{
            "tool": {"driver": {
                "name": "CodeSonar",
                "rules": [
                    {"id": "LANG.MEM.NPD", "name": "Null Pointer Dereference",
                     "fullDescription": {"text": "A null pointer is dereferenced."},
                     "defaultConfiguration": {"level": "error"}},
                    {"id": "LANG.MEM.BO", "name": "Buffer Overrun"}
                ]
            }},
            "invocations": [{"startTimeUtc": "2026-07-24T19:11:18Z",
                             "endTimeUtc": "2026-07-24T19:11:35.654875Z"}],
            "originalUriBaseIds": {"SRCROOT": {"uri": "file:///build/"}},
            "results": [
                {"ruleId": "LANG.MEM.BO", "ruleIndex": 1,
                 "message": {"text": "Buffer overrun"},
                 "fingerprints": {"codesonarCluster": "9f6c", "codesonarFingerprint": "2d2c"},
                 "locations": [{"physicalLocation": {
                    "artifactLocation": {"uri": "src/main.c", "uriBaseId": "SRCROOT"},
                    "region": {"startLine": 3, "startColumn": 5}}}],
                 "unknownField": true},
                {"rule": {"id": "LANG.MEM.NPD"}, "level": "warning",
                 "message": {"text": "Null pointer"}}
            ]
        }]
    }"#;

    #[test]
    fn parses_codesonar_log() {
        let log = Log::from_json(LOG).unwrap();
        let run = &log.runs[0];

        assert_eq!(run.tool.driver.name, "CodeSonar");
        assert_eq!(run.results.len(), 2);
        assert_eq!(
            run.original_uri_base_ids["SRCROOT"].uri.as_deref(),
            Some("file:///build/")
        );

        let region = run.results[0].locations[0]
            .physical_location
            .as_ref()
            .and_then(|p| p.region)
            .unwrap();
        assert_eq!(region.start_line, Some(3));
        assert_eq!(region.start_column, Some(5));
        assert_eq!(region.end_line, None);
        assert_eq!(run.results[1].level, Some(Level::Warning));
    }

    #[test]
    fn reads_fingerprints() {
        let log = Log::from_json(LOG).unwrap();
        let run = &log.runs[0];

        assert_eq!(run.results[0].fingerprint(), Some("2d2c"));
        assert_eq!(run.results[1].fingerprint(), None);

        let partial = SarifResult {
            partial_fingerprints: [("codesonarFingerprint".into(), "abcd".into())].into(),
            ..SarifResult::default()
        };
        assert_eq!(partial.fingerprint(), Some("abcd"));

        let other = SarifResult {
            fingerprints: [("otherTool".into(), "1234".into())].into(),
            ..SarifResult::default()
        };
        assert_eq!(other.fingerprint(), Some("1234"));
    }

    #[test]
    fn reads_analysis_time() {
        let log = Log::from_json(LOG).unwrap();
        assert_eq!(log.runs[0].end_time(), Some("2026-07-24T19:11:35.654875Z"));

        let run = Run {
            invocations: vec![Invocation {
                start_time_utc: Some("2026-07-24T19:11:18Z".into()),
                end_time_utc: None,
            }],
            ..Run::default()
        };
        assert_eq!(run.end_time(), Some("2026-07-24T19:11:18Z"));
        assert_eq!(Run::default().end_time(), None);
    }

    #[test]
    fn finds_rule_by_index_and_id() {
        let log = Log::from_json(LOG).unwrap();
        let run = &log.runs[0];

        assert_eq!(run.rule_for(&run.results[0]).unwrap().id, "LANG.MEM.BO");
        assert_eq!(run.rule_for(&run.results[1]).unwrap().id, "LANG.MEM.NPD");
    }

    #[test]
    fn prefers_markdown_message() {
        let message = MultiformatMessage {
            text: "plain".into(),
            markdown: Some("**md**".into()),
        };
        assert_eq!(message.best(), "**md**");

        let message = MultiformatMessage {
            text: "plain".into(),
            markdown: None,
        };
        assert_eq!(message.best(), "plain");
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(Log::from_json("not json").is_err());
    }
}
