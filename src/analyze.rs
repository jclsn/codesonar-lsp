//! The command line of a CodeSonar analysis, which clients run in a
//! terminal so that its output can be watched.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Serialize;

use crate::config::{Config, expand_home};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeCommand {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    /// Files the client deletes once the analysis is done, e.g. a file
    /// with the hub password.
    pub temporary_files: Vec<PathBuf>,
}

/// Builds the `codesonar analyze` command for the workspace at `root`.
pub fn command(config: &Config, root: &Path, name: Option<&str>) -> anyhow::Result<AnalyzeCommand> {
    let Some(hub) = &config.hub else {
        bail!("no hub configured");
    };
    let analyze = &config.analyze;
    let mut temporary_files = Vec::new();
    let mut command = vec![
        analyze.command.clone(),
        "analyze".into(),
        path(&root.join(expand_home(&analyze.project_file))),
        "-foreground".into(),
    ];

    let project = analyze
        .project
        .clone()
        .or_else(|| hub.project.as_ref().map(|project| format!("/{project}")));

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

    command.extend(analyze.args.iter().cloned());
    command.push(hub.url.clone());
    command.extend(analyze.build_command.iter().cloned());

    Ok(AnalyzeCommand {
        command,
        cwd: root.to_path_buf(),
        temporary_files,
    })
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
