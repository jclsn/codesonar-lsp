//! The command line of a CodeSonar analysis, which clients run in a
//! terminal so that its output can be watched.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Serialize;

use crate::config::{Config, expand_home};
use crate::detect::LocalProject;
use crate::workspace::setting;

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

    // An incremental analysis takes what was not compiled from its parent.
    let incremental =
        is_incremental(workspace_conf.as_deref(), prefix) && has_project_files(prefix);
    let build = match incremental {
        true => &analyze.incremental_build_command,
        false => &analyze.build_command,
    };
    command.extend(build.iter().cloned());

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

/// Whether analyses of the project are incremental: the last setting of
/// `INCREMENTAL_BUILD`, in the workspace configuration, which CodeSonar
/// reads last, or else in the project configuration. CodeSonar's default
/// is incremental.
pub fn is_incremental(workspace_conf: Option<&Path>, prefix: &Path) -> bool {
    [
        workspace_conf.map(Path::to_path_buf),
        Some(conf_file(prefix)),
    ]
    .into_iter()
    .flatten()
    .find_map(|conf| setting(&std::fs::read_to_string(conf).ok()?, "INCREMENTAL_BUILD"))
    .map(|value| value.eq_ignore_ascii_case("yes"))
    .unwrap_or(true)
}

/// Whether the project was analyzed before, so that a new analysis can be
/// its incremental child.
fn has_project_files(prefix: &Path) -> bool {
    let mut files = prefix.as_os_str().to_owned();
    files.push(".prj_files");
    Path::new(&files).join("aid.txt").is_file()
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

pub fn template(command: &str) -> Option<PathBuf> {
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
        .tempfile_in(password_dir())
        .context("cannot create the password file")?;
    writeln!(file, "{password}")?;
    let (_, path) = file.keep()?;
    Ok(path)
}

/// Where the password file goes: the runtime directory of the user, which
/// only the user can read and which is cleared when the user logs out, so
/// that a file left behind by a crash does not stay. Without one, the
/// temporary directory.
fn password_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && dir.is_dir())
        .unwrap_or_else(std::env::temp_dir)
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

    fn build(root: &Path, config: &Config) -> Vec<String> {
        let command = run(config, root, None).unwrap().command;
        let url = command
            .iter()
            .position(|part| part == "http://hub")
            .unwrap();
        command[url + 1..].to_vec()
    }

    #[test]
    fn passes_the_workspace_configuration() {
        let config = config(json!({"url": "http://hub"}));
        let root = tempfile::tempdir().unwrap();

        assert!(
            !run(&config, root.path(), None)
                .unwrap()
                .command
                .contains(&"-conf-file".into())
        );

        let conf = root.path().join("codesonar.conf");
        std::fs::write(&conf, "WARNING_FILTER += discard path:/build/\n").unwrap();
        let command = run(&config, root.path(), None).unwrap().command;

        let at = command
            .iter()
            .position(|part| part == "-conf-file")
            .unwrap();

        assert_eq!(command[at + 1], conf.display().to_string());
    }

    #[test]
    fn builds_incrementally_after_an_analysis() {
        let mut config = config(json!({"url": "http://hub"}));
        config.analyze.build_command = vec!["make".into(), "-B".into()];
        config.analyze.incremental_build_command = vec!["make".into(), "-j4".into()];
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            build(root.path(), &config),
            ["make", "-B"],
            "the first analysis"
        );

        let files = root.path().join(".codesonar/project.prj_files");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("aid.txt"), "20\n").unwrap();

        assert_eq!(
            build(root.path(), &config),
            ["make", "-j4"],
            "incremental by default"
        );

        // The workspace configuration wins over the project's, commented
        // settings do not count.
        std::fs::write(
            root.path().join(".codesonar/project.conf"),
            "INCREMENTAL_BUILD = Yes\n",
        )
        .unwrap();

        std::fs::write(
            root.path().join("codesonar.conf"),
            "INCREMENTAL_BUILD = No\n#INCREMENTAL_BUILD = Yes\n",
        )
        .unwrap();

        assert_eq!(build(root.path(), &config), ["make", "-B"]);

        std::fs::write(
            root.path().join("codesonar.conf"),
            "INCREMENTAL_BUILD=yes\n",
        )
        .unwrap();

        assert_eq!(build(root.path(), &config), ["make", "-j4"]);
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
        assert_eq!(file.parent(), Some(password_dir().as_path()));

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
