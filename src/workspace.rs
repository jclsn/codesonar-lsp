//! The configuration of a workspace in its `codesonar.conf`: the settings
//! of CodeSonar, which `codesonar analyze` reads with `-conf-file`, and a
//! section that codesonar-lsp writes. CodeSonar has no settings for the hub
//! project, the presets and the build command, so the section keeps them
//! in comments, next to real settings like `HUB_ADDRESS` and the rules of
//! checks:
//!
//! ```text
//! # BEGIN codesonar-lsp: written by codesonar-lsp, e.g. :CodeSonarPresets
//! # codesonar-lsp: project = /mixxx
//! # codesonar-lsp: presets = security concurrency
//! # codesonar-lsp: build = cmake --build build
//! WARNING_FILTER += allow class="Function Too Long"
//! # END codesonar-lsp
//! HUB_ADDRESS = 192.168.184.28:7340
//! ```
//!
//! `HUB_ADDRESS` is CodeSonar's own setting, so it stays outside the
//! section.

use std::path::{Path, PathBuf};

use anyhow::Context;

const BEGIN: &str = "# BEGIN codesonar-lsp";
const END: &str = "# END codesonar-lsp";
const HEADER: &str = "# BEGIN codesonar-lsp: written by codesonar-lsp, e.g. :CodeSonarPresets";
const DIRECTIVE: &str = "# codesonar-lsp:";

/// The settings of a workspace in its configuration file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceConf {
    pub file: Option<PathBuf>,
    /// The hub project path, e.g. `/mixxx`.
    pub project: Option<String>,
    pub presets: Option<Vec<String>>,
    pub build: Option<Vec<String>>,
    pub incremental_build: Option<Vec<String>>,
    /// The last `HUB_ADDRESS` of the file, which CodeSonar uses.
    pub hub_address: Option<String>,
    /// Whether the file has the section.
    pub has_section: bool,
    /// The last `INCREMENTAL_BUILD` of the file.
    pub incremental: Option<bool>,
    /// The checks of the section that differ from their default.
    pub checks: Vec<CheckRule>,
    /// Other lines of the section, kept as they are.
    pub other: Vec<String>,
}

/// A rule that turns the warnings of a check on or off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRule {
    pub class: String,
    pub enabled: bool,
}

impl CheckRule {
    fn parse(line: &str) -> Option<Self> {
        let (key, rule) = line.split_once("+=")?;

        if key.trim() != "WARNING_FILTER" {
            return None;
        }

        let (action, class) = rule.trim().split_once(' ')?;
        let class = class.trim().strip_prefix("class=\"")?.strip_suffix('"')?;
        let enabled = match action {
            "allow" => true,
            "discard" => false,
            _ => return None,
        };
        (!class.contains('"')).then(|| Self {
            class: class.to_string(),
            enabled,
        })
    }

    fn line(&self) -> String {
        let action = if self.enabled { "allow" } else { "discard" };
        format!("WARNING_FILTER += {action} class=\"{}\"", self.class)
    }
}

impl WorkspaceConf {
    /// The settings in `file`, none if it does not exist.
    pub fn read(file: &Path) -> Self {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        Self {
            file: Some(file.to_path_buf()),
            ..Self::parse(&text)
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut conf = Self {
            hub_address: setting(text, "HUB_ADDRESS"),
            has_section: section(text).is_some(),
            incremental: setting(text, "INCREMENTAL_BUILD")
                .map(|value| value.eq_ignore_ascii_case("yes")),
            ..Self::default()
        };

        for line in section(text).unwrap_or_default() {
            let trimmed = line.trim();

            if let Some(directive) = trimmed.strip_prefix(DIRECTIVE)
                && let Some((key, value)) = directive.split_once('=')
            {
                // A comment after the value explains it.
                let value = value.split(" #").next().unwrap_or_default().trim();
                let words = || value.split_whitespace().map(str::to_string).collect();

                match key.trim() {
                    "project" => conf.project = Some(value.to_string()).filter(|v| !v.is_empty()),
                    "presets" => conf.presets = Some(words()),
                    "build" => conf.build = Some(words()),
                    "incremental-build" => conf.incremental_build = Some(words()),
                    _ => conf.other.push(line.to_string()),
                }
                continue;
            }

            if let Some(rule) = CheckRule::parse(trimmed) {
                conf.checks.retain(|other| other.class != rule.class);
                conf.checks.push(rule);
            } else if !trimmed.is_empty() {
                conf.other.push(line.to_string());
            }
        }

        conf
    }

    /// The hub project name, e.g. `mixxx` of `/mixxx`.
    pub fn project_name(&self) -> Option<&str> {
        let project = self.project.as_deref()?.trim_end_matches('/');
        project.rsplit('/').next().filter(|name| !name.is_empty())
    }

    /// The lines of the section.
    fn lines(&self) -> Vec<String> {
        let mut lines = vec![HEADER.to_string()];
        let directive = |key: &str, value: &str| format!("{DIRECTIVE} {key} = {value}");

        if let Some(project) = &self.project {
            lines.push(directive("project", project));
        }

        if let Some(presets) = &self.presets {
            lines.push(directive("presets", &presets.join(" ")));
        }

        let explained = |line: String, comment: &str| format!("{line:<48} # {comment}");

        if let Some(build) = &self.build {
            lines.push(explained(
                directive("build", &build.join(" ")),
                "full rebuild, for the first analysis",
            ));
        }

        if let Some(build) = &self.incremental_build {
            lines.push(explained(
                directive("incremental-build", &build.join(" ")),
                "what changed, for later analyses",
            ));
        }

        lines.extend(self.other.iter().cloned());
        lines.extend(self.checks.iter().map(CheckRule::line));
        lines.push(END.to_string());
        lines
    }

    /// Writes the section into the file: in place of the old one, else at
    /// the top. Creates the file if it does not exist.
    pub fn write(&self) -> anyhow::Result<()> {
        let file = self.file.as_deref().context("no configuration file")?;
        let text = std::fs::read_to_string(file).unwrap_or_default();
        std::fs::write(file, replace_section(&text, &self.lines()))
            .with_context(|| format!("cannot write {}", file.display()))
    }

    /// Sets a CodeSonar setting like `HUB_ADDRESS` in the file, outside the
    /// section: in place of the last one, else after a blank line below the
    /// section.
    pub fn write_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let file = self.file.as_deref().context("no configuration file")?;
        let text = std::fs::read_to_string(file).unwrap_or_default();
        let line = format!("{key} = {value}");
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let mut inside = false;
        let mut last = None;
        let mut end = None;

        for (i, l) in lines.iter().enumerate() {
            if l.trim_start().starts_with(BEGIN) {
                inside = true;
            } else if inside && l.trim() == END {
                inside = false;
                end = Some(i);
            } else if !inside && key_value(l.trim(), key).is_some() {
                last = Some(i);
            }
        }

        match (last, end) {
            (Some(i), _) => lines[i] = line,
            // After a blank line below the section.
            (None, Some(end)) if lines.get(end + 1).is_some_and(|l| l.trim().is_empty()) => {
                lines.insert(end + 2, line)
            }
            (None, Some(end)) => lines
                .splice(end + 1..end + 1, [String::new(), line])
                .for_each(drop),
            (None, None) => lines.insert(0, line),
        }

        std::fs::write(file, lines.join("\n") + "\n")
            .with_context(|| format!("cannot write {}", file.display()))
    }
}

/// The lines between the markers of the section.
fn section(text: &str) -> Option<Vec<&str>> {
    let mut lines = text.lines();
    lines.find(|line| line.trim_start().starts_with(BEGIN))?;
    Some(lines.take_while(|line| line.trim() != END).collect())
}

fn replace_section(text: &str, section: &[String]) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let begin = lines
        .iter()
        .position(|line| line.trim_start().starts_with(BEGIN));
    let end = begin.and_then(|begin| {
        lines[begin..]
            .iter()
            .position(|line| line.trim() == END)
            .map(|end| begin + end)
    });
    let mut result: Vec<String> = Vec::new();

    match (begin, end) {
        (Some(begin), Some(end)) => {
            result.extend(lines[..begin].iter().map(|line| line.to_string()));
            result.extend(section.iter().cloned());
            result.extend(lines[end + 1..].iter().map(|line| line.to_string()));
        }
        _ => {
            result.extend(section.iter().cloned());

            if !lines.is_empty() {
                result.push(String::new());
            }

            result.extend(lines.iter().map(|line| line.to_string()));
        }
    }

    result.join("\n") + "\n"
}

/// The value of `KEY = value` in a line, if it is not a comment.
fn key_value(line: &str, key: &str) -> Option<String> {
    let (name, value) = line.split_once('=')?;
    (name.trim() == key && !name.trim_start().starts_with('#')).then(|| value.trim().to_string())
}

/// The last setting of a CodeSonar parameter, which is the one CodeSonar
/// uses.
pub fn setting(text: &str, key: &str) -> Option<String> {
    text.lines().rev().find_map(|line| key_value(line, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = "\
# Mixxx
IGNORED_COMPILATIONS += /build/
# BEGIN codesonar-lsp: written by codesonar-lsp
# codesonar-lsp: project = /tree/mixxx
# codesonar-lsp: presets = security concurrency
# codesonar-lsp: build = cmake --build build
WARNING_FILTER += allow class=\"Function Too Long\"
WARNING_FILTER += discard class=\"Unused Value\"
SOMETHING = by hand
# END codesonar-lsp
HUB_ADDRESS = hub:7340
#HUB_ADDRESS = commented:1
INCREMENTAL_BUILD = No
";

    #[test]
    fn reads_the_section_and_the_settings() {
        let conf = WorkspaceConf::parse(CONF);
        assert_eq!(conf.project.as_deref(), Some("/tree/mixxx"));

        assert_eq!(conf.project_name(), Some("mixxx"));
        assert_eq!(
            conf.presets,
            Some(vec!["security".into(), "concurrency".into()])
        );
        assert_eq!(
            conf.build,
            Some(vec!["cmake".into(), "--build".into(), "build".into()])
        );
        assert_eq!(conf.incremental_build, None);

        assert!(conf.has_section);
        assert_eq!(
            conf.hub_address.as_deref(),
            Some("hub:7340"),
            "comments do not count"
        );
        assert_eq!(conf.incremental, Some(false));
        assert_eq!(
            conf.checks,
            [
                CheckRule {
                    class: "Function Too Long".into(),
                    enabled: true
                },
                CheckRule {
                    class: "Unused Value".into(),
                    enabled: false
                },
            ]
        );

        assert_eq!(conf.other, ["SOMETHING = by hand"]);
    }

    #[test]
    fn the_last_hub_address_wins() {
        let conf = WorkspaceConf::parse(&format!("{CONF}HUB_ADDRESS = other:1\n"));
        assert_eq!(conf.hub_address.as_deref(), Some("other:1"));
        assert!(!WorkspaceConf::parse("HUB_ADDRESS = hub:1\n").has_section);
    }

    #[test]
    fn writes_settings_outside_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("codesonar.conf");
        std::fs::write(&file, CONF).unwrap();
        WorkspaceConf::read(&file)
            .write_setting("HUB_ADDRESS", "new:2")
            .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();

        assert!(
            text.contains("# END codesonar-lsp\nHUB_ADDRESS = new:2\n#HUB_ADDRESS"),
            "{text}"
        );

        // Without one, after a blank line below the section.
        std::fs::write(&file, format!("{HEADER}\n{END}\nINCREMENTAL_BUILD = Yes\n")).unwrap();
        WorkspaceConf::read(&file)
            .write_setting("HUB_ADDRESS", "hub:1")
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{HEADER}\n{END}\n\nHUB_ADDRESS = hub:1\nINCREMENTAL_BUILD = Yes\n")
        );
    }

    #[test]
    fn rewrites_only_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("codesonar.conf");
        std::fs::write(&file, CONF).unwrap();
        let mut conf = WorkspaceConf::read(&file);
        conf.presets = Some(vec!["cert".into()]);
        conf.write().unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.starts_with("# Mixxx\nIGNORED_COMPILATIONS += /build/\n# BEGIN"),
            "{text}"
        );
        assert!(text.ends_with(
            "# END codesonar-lsp\nHUB_ADDRESS = hub:7340\n#HUB_ADDRESS = commented:1\nINCREMENTAL_BUILD = No\n"
        ));
        assert!(text.contains("# codesonar-lsp: presets = cert\n"));
        assert!(text.contains("SOMETHING = by hand\n"), "kept");
        assert_eq!(WorkspaceConf::read(&file), conf, "read back");
    }

    #[test]
    fn explains_the_build_commands() {
        let conf = WorkspaceConf {
            build: Some(vec!["make".into(), "-B".into()]),
            incremental_build: Some(vec!["make".into()]),
            ..WorkspaceConf::default()
        };

        let lines = conf.lines();

        assert_eq!(
            lines[1],
            "# codesonar-lsp: build = make -B                 # full rebuild, for the first analysis"
        );
        assert_eq!(
            lines[2],
            "# codesonar-lsp: incremental-build = make        # what changed, for later analyses"
        );

        assert_eq!(
            WorkspaceConf::parse(&(lines.join("\n") + "\n")).build,
            conf.build,
            "read back"
        );
        assert_eq!(
            WorkspaceConf::parse(&lines.join("\n")).incremental_build,
            conf.incremental_build
        );
    }

    #[test]
    fn creates_the_section_at_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("codesonar.conf");
        std::fs::write(&file, "INCREMENTAL_BUILD = Yes\n").unwrap();

        let conf = WorkspaceConf {
            project: Some("/app".into()),
            ..WorkspaceConf::read(&file)
        };

        conf.write().unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!(
                "{HEADER}\n# codesonar-lsp: project = /app\n{END}\n\nINCREMENTAL_BUILD = Yes\n"
            )
        );

        let missing = dir.path().join("new.conf");

        WorkspaceConf {
            presets: Some(vec![]),
            ..WorkspaceConf::read(&missing)
        }
        .write()
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&missing).unwrap(),
            format!("{HEADER}\n# codesonar-lsp: presets = \n{END}\n")
        );
    }
}
