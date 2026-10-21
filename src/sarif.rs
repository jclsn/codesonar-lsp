//! Subset of the SARIF 2.1.0 format as exported by CodeSonar.

use std::collections::HashMap;

use serde::Deserialize;

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
    pub hosted_viewer_uri: Option<String>,
}

impl SarifResult {
    pub fn rule_id(&self) -> Option<&str> {
        self.rule_id
            .as_deref()
            .or_else(|| self.rule.as_ref().and_then(|r| r.id.as_deref()))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Location {
    pub physical_location: Option<PhysicalLocation>,
    pub message: Option<Message>,
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
