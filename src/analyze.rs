//! The command line of a CodeSonar analysis, which clients run in a
//! terminal so that its output can be watched.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Serialize;

use crate::config::{Config, expand_home};
use crate::detect::LocalProject;

/// The project prefix of workspaces without project files.
const DEFAULT_PREFIX: &str = ".codesonar/project";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeCommand {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    /// Files the client deletes once the analysis is done, e.g. a file
    /// with the hub password.
    pub temporary_files: Vec<PathBuf>,
}

/// Builds the `codesonar analyze` command for the workspace at `root`,
/// with the project files at `prefix` and the hub project `project`.
pub fn command(
    config: &Config,
    root: &Path,
    prefix: &Path,
    project: Option<&str>,
    name: Option<&str>,
) -> anyhow::Result<AnalyzeCommand> {
    let Some(hub) = &config.hub else {
        bail!("no hub configured");
    };
    let analyze = &config.analyze;
    create_dir(prefix)?;

    let mut temporary_files = Vec::new();
    let mut command = vec![
        analyze.command.clone(),
        "analyze".into(),
        path(prefix),
        "-foreground".into(),
    ];

    let project = analyze
        .project
        .clone()
        .or_else(|| project.map(|project| format!("/{project}")));

    if let Some(project) = project {
        command.extend(["-project".into(), project]);
    }

    if let Some(name) = name.filter(|name| !name.is_empty()) {
        command.extend(["-name".into(), name.into()]);
    }

    command.extend(["-srcroot".into(), path(root)]);

    if let Some(username) = &hub.username {
        let password_file = match (&hub.password_file, &hub.password) {
            (Some(file), None) => expand_home(file),
            _ => {
                let password = hub.password().context("no hub password configured")?;
                let file = password_file(&password)?;
                temporary_files.push(file.clone());
                file
            }
        };
        command.extend([
            "-auth".into(),
            "password".into(),
            "-hubuser".into(),
            username.clone(),
            "-hubpwfile".into(),
            path(&password_file),
        ]);
    }

    for preset in &analyze.presets {
        command.extend(["-preset".into(), preset.clone()]);
    }

    let workspace_conf = workspace_conf(config, root);

    if let Some(conf) = &workspace_conf {
        command.extend(["-conf-file".into(), path(conf)]);
    }

    command.extend(analyze.args.iter().cloned());
    command.push(hub.url.clone());

    command.extend(analyze.build_command.iter().cloned());

    Ok(AnalyzeCommand {
        command,
        cwd: root.to_path_buf(),
        temporary_files,
    })
}

/// The project prefix: CodeSonar names the project files after it, e.g.
/// `<prefix>.conf` and `<prefix>.prj_files`. The configured one, else the
/// one of the project files in the workspace.
pub fn prefix(config: &Config, root: &Path, local: Option<&LocalProject>) -> PathBuf {
    match (&config.analyze.project_file, local) {
        (Some(file), _) => root.join(expand_home(file)),
        (None, Some(local)) => local.prefix.clone(),
        (None, None) => root.join(DEFAULT_PREFIX),
    }
}

/// The configuration file of the workspace, if it exists.
pub fn workspace_conf(config: &Config, root: &Path) -> Option<PathBuf> {
    let conf = root.join(expand_home(&config.analyze.conf_file));
    conf.is_file().then_some(conf)
}

/// The configuration file of the project.
pub fn conf_file(prefix: &Path) -> PathBuf {
    let mut file = prefix.as_os_str().to_owned();
    file.push(".conf");
    file.into()
}

/// Creates the configuration file of the project from CodeSonar's
/// template, as the first analysis would. Returns whether it was created.
pub fn create_conf(config: &Config, prefix: &Path) -> anyhow::Result<bool> {
    let conf = conf_file(prefix);

    if conf.exists() {
        return Ok(false);
    }

    let template = template(&config.analyze.command).with_context(|| {
        format!(
            "cannot find the template.conf of {}",
            config.analyze.command
        )
    })?;
    create_dir(&conf)?;
    std::fs::copy(&template, &conf)
        .with_context(|| format!("cannot copy {} to {}", template.display(), conf.display()))?;
    Ok(true)
}

/// CodeSonar creates the project files, but not their directory.
fn create_dir(file: &Path) -> anyhow::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }

    Ok(())
}

/// The directory of a CodeSonar installation, with `template.conf` and
/// the `presets`: the parent of the `bin` directory of the command.
fn installation(command: &str) -> Option<PathBuf> {
    let command = expand_home(Path::new(command));
    let command = if command.components().count() > 1 {
        command
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(&command))
            .find(|path| path.is_file())?
    };
    // The command is often a link into the installation.
    let resolved = command.canonicalize().ok();

    [Some(command), resolved]
        .into_iter()
        .flatten()
        .filter_map(|path| Some(path.parent()?.parent()?.to_path_buf()))
        .find(|dir| dir.join("template.conf").is_file())
}

fn template(command: &str) -> Option<PathBuf> {
    Some(installation(command)?.join("template.conf"))
}

/// A preset of CodeSonar, which configures an analysis for a purpose, e.g.
/// the rules of a coding standard.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Preset {
    pub name: String,
    pub description: Option<String>,
    /// Whether analyses use it.
    pub selected: bool,
}

/// The presets of the CodeSonar installation, by name.
pub fn presets(config: &Config) -> anyhow::Result<Vec<Preset>> {
    let command = &config.analyze.command;
    let dir = installation(command)
        .with_context(|| format!("cannot find the CodeSonar installation of {command}"))?
        .join("presets");
    let entries =
        std::fs::read_dir(&dir).with_context(|| format!("cannot read {}", dir.display()))?;
    let mut presets: Vec<Preset> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.strip_suffix(".conf")?;

            // Parts that other presets include.
            if name.ends_with("_inc") {
                return None;
            }

            Some(Preset {
                description: std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| description(&text)),
                selected: config.analyze.presets.iter().any(|p| p == name),
                name: name.to_string(),
            })
        })
        .collect();
    presets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(presets)
}

/// The first comment of a preset, after the note that it was generated.
fn description(text: &str) -> Option<String> {
    let words: Vec<&str> = text
        .lines()
        .map(str::trim)
        .skip_while(|line| {
            line.is_empty() || *line == "#" || line.starts_with("# This file was generated")
        })
        .map_while(|line| line.strip_prefix('#'))
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// Writes the password to a file only the user can read.
fn password_file(password: &str) -> anyhow::Result<PathBuf> {
    let mut file = tempfile::Builder::new()
        .prefix("codesonar-password-")
        .tempfile()
        .context("cannot create the password file")?;
    writeln!(file, "{password}")?;
    let (_, path) = file.keep()?;
    Ok(path)
}

fn path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The command with the prefix and project of the settings.
    fn run(config: &Config, root: &Path, name: Option<&str>) -> anyhow::Result<AnalyzeCommand> {
        let project = config.hub.as_ref().and_then(|hub| hub.project.as_deref());
        command(config, root, &prefix(config, root, None), project, name)
    }

    fn config(hub: serde_json::Value) -> Config {
        Config::from_value(&json!({"hub": hub, "analyze": {"buildCommand": "make -j4"}})).unwrap()
    }

    #[test]
    fn builds_analyze_command() {
        let config = config(json!({
            "url": "http://hub:7340",
            "project": "app",
            "username": "alice",
            "passwordFile": "/secret/pw",
        }));

        let root = tempfile::tempdir().unwrap();
        let ws = root.path().display().to_string();

        let command = run(&config, root.path(), Some("nightly")).unwrap();

        assert_eq!(
            command.command,
            [
                "codesonar",
                "analyze",
                &format!("{ws}/.codesonar/project"),
                "-foreground",
                "-project",
                "/app",
                "-name",
                "nightly",
                "-srcroot",
                &ws,
                "-auth",
                "password",
                "-hubuser",
                "alice",
                "-hubpwfile",
                "/secret/pw",
                "http://hub:7340",
                "make",
                "-j4",
            ]
        );
        assert_eq!(command.cwd, root.path());
        assert!(command.temporary_files.is_empty());
    }

    #[test]
    fn writes_temporary_password_file() {
        let config = config(json!({"url": "http://hub", "username": "alice", "password": "pw"}));
        let root = tempfile::tempdir().unwrap();
        let command = run(&config, root.path(), None).unwrap();

        assert!(!command.command.contains(&"-name".to_string()));
        assert!(!command.command.contains(&"-project".to_string()));

        let file = &command.temporary_files[0];
        assert_eq!(std::fs::read_to_string(file).unwrap(), "pw\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn runs_anonymously_and_needs_a_hub() {
        let root = tempfile::tempdir().unwrap();
        let command = run(&config(json!({"url": "http://hub"})), root.path(), None).unwrap();

        assert!(!command.command.contains(&"-auth".to_string()));
        assert!(run(&Config::default(), Path::new("/ws"), None).is_err());
    }

    #[test]
    fn creates_the_project_directory() {
        let root = tempfile::tempdir().unwrap();

        let config = Config::from_value(&json!({
            "hub": {"url": "http://hub"},
            "analyze": {"projectFile": "build/cs/project"},
        }))
        .unwrap();

        run(&config, root.path(), None).unwrap();

        assert!(root.path().join("build/cs").is_dir());
        assert!(!root.path().join("build/cs/project").exists());
    }

    /// A CodeSonar installation with a template, linked from `bin`.
    fn install(dir: &Path) -> PathBuf {
        let install = dir.join("opt/codesonar");
        std::fs::create_dir_all(install.join("bin")).unwrap();
        std::fs::write(install.join("bin/codesonar"), "").unwrap();
        std::fs::write(install.join("template.conf"), "# template\n").unwrap();
        std::fs::create_dir_all(install.join("presets")).unwrap();
        let presets = [
            (
                "misrac2025",
                "# This file was generated from 'x.in'\n#\n\n# Enables MISRA C:2025\n# rules.\n\n# Other\nA = 1\n",
            ),
            ("dry_run", "DRY_RUN = Yes\n"),
            ("misra_inc", "# Included by others\n"),
        ];
        for (name, text) in presets {
            std::fs::write(install.join(format!("presets/{name}.conf")), text).unwrap();
        }
        std::fs::write(install.join("presets/README"), "").unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(install.join("bin/codesonar"), dir.join("bin/codesonar"))
            .unwrap();
        install.join("bin/codesonar")
    }

    #[test]
    fn prefers_the_configured_prefix_to_the_detected() {
        let local = LocalProject {
            prefix: "/ws/mixxx".into(),
            analysis_id: None,
        };

        let configured =
            Config::from_value(&json!({"analyze": {"projectFile": "cs/app"}})).unwrap();
        let root = Path::new("/ws");

        assert_eq!(
            prefix(&configured, root, Some(&local)),
            Path::new("/ws/cs/app")
        );
        assert_eq!(
            prefix(&Config::default(), root, Some(&local)),
            Path::new("/ws/mixxx")
        );
        assert_eq!(
            prefix(&Config::default(), root, None),
            Path::new("/ws/.codesonar/project")
        );
    }

    #[test]
    fn names_the_project_files_after_the_prefix() {
        let config = Config::from_value(&json!({"analyze": {"projectFile": "cs/app"}})).unwrap();
        let prefix = prefix(&config, Path::new("/ws"), None);

        assert_eq!(prefix, Path::new("/ws/cs/app"));
        assert_eq!(conf_file(&prefix), Path::new("/ws/cs/app.conf"));
    }

    #[test]
    fn finds_the_template_of_the_installation() {
        let dir = tempfile::tempdir().unwrap();
        let command = install(dir.path());
        let expected = dir.path().join("opt/codesonar/template.conf");

        assert_eq!(template(command.to_str().unwrap()), Some(expected.clone()));

        #[cfg(unix)]

        assert_eq!(
            template(dir.path().join("bin/codesonar").to_str().unwrap()),
            Some(expected.canonicalize().unwrap())
        );
        assert_eq!(template("/nonexistent/bin/codesonar"), None);
    }

    #[test]
    fn creates_the_configuration_from_the_template() {
        let dir = tempfile::tempdir().unwrap();
        let command = install(dir.path());
        let root = dir.path().join("ws");
        let config = Config::from_value(&json!({"analyze": {"command": command}})).unwrap();

        assert!(create_conf(&config, &root.join(".codesonar/project")).unwrap());

        let conf = root.join(".codesonar/project.conf");
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "# template\n");

        std::fs::write(&conf, "edited\n").unwrap();

        assert!(
            !create_conf(&config, &root.join(".codesonar/project")).unwrap(),
            "keeps an existing file"
        );
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "edited\n");

        let config = Config::from_value(&json!({"analyze": {"command": "/nonexistent"}})).unwrap();
        let error = create_conf(&config, &dir.path().join("other/project")).unwrap_err();

        assert!(error.to_string().contains("template.conf"), "{error}");
    }

    #[test]
    fn lists_the_presets_of_the_installation() {
        let dir = tempfile::tempdir().unwrap();
        let command = install(dir.path());

        let config = Config::from_value(&json!({
            "analyze": {"command": command, "presets": ["misrac2025"]},
        }))
        .unwrap();

        assert_eq!(
            presets(&config).unwrap(),
            [
                Preset {
                    name: "dry_run".into(),
                    description: None,
                    selected: false,
                },
                Preset {
                    name: "misrac2025".into(),
                    description: Some("Enables MISRA C:2025 rules.".into()),
                    selected: true,
                },
            ]
        );

        let config = Config::from_value(&json!({"analyze": {"command": "/nonexistent"}})).unwrap();
        assert!(presets(&config).is_err());
    }

    #[test]
    fn passes_the_presets() {
        let root = tempfile::tempdir().unwrap();

        let config = Config::from_value(&json!({
            "hub": {"url": "http://hub"},
            "analyze": {"presets": ["misrac2025", "cert"], "args": ["-x"]},
        }))
        .unwrap();

        let command = run(&config, root.path(), None).unwrap().command;
        let at = command.iter().position(|arg| arg == "-preset").unwrap();

        assert_eq!(
            command[at..at + 5],
            ["-preset", "misrac2025", "-preset", "cert", "-x"]
        );
    }
}
