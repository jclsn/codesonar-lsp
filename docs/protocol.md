# Reference

Details for editor clients and for settings beyond the [README](../README.md).

## Settings

The settings are passed as `initializationOptions`. They can be changed later
via `workspace/didChangeConfiguration` under the `codesonar` key. Changes of
the `analyze` settings keep the shown analysis.

```json
{
  "hub": {
    "url": "https://hub.example.com:7340",
    "project": "my_project",
    "username": "alice",
    "passwordFile": "~/.config/codesonar/password",
    "caCertificate": "/etc/ssl/hub-ca.pem"
  },
  "pathMappings": [
    { "from": "C:\\build\\project", "to": "/home/alice/project" }
  ],
  "changed": "hide",
  "analyze": { "buildCommand": "make -B" }
}
```

| Setting                  | Description                                                     |
| ------------------------ | --------------------------------------------------------------- |
| `hub.url`                | Hub URL.                                                        |
| `hub.project`            | Hub project, by name or id. Its newest finished analysis is shown. Detected if not set, see below. |
| `hub.analysisId`         | Show this analysis instead.                                     |
| `hub.username`           | Hub user. Without one, the hub is accessed anonymously.         |
| `hub.password`           | Hub password.                                                   |
| `hub.passwordFile`       | File with the hub password. Without either, `CODESONAR_PASSWORD` is used. |
| `hub.sarifPath`          | Hub path of the SARIF export. Default: `/analysis/{analysisId}-allwarnings.sarif`. |
| `hub.caCertificate`      | PEM file with an additional trusted CA certificate.             |
| `hub.acceptInvalidCerts` | Do not verify the hub's certificate.                            |
| `sarifFile`              | SARIF file, absolute or relative to the workspace root. Used without `hub`. |
| `pathMappings`           | Maps paths from the analysis machine to local paths.            |
| `changed`                | Warnings in code changed since the analysis: `hide` or `mark` them as hints. |
| `stateDir`               | Where the analyzed code is remembered, readable only by you. Default: `$XDG_STATE_HOME/codesonar-lsp`. |
| `cacheDir`               | Where downloaded analyses are kept, readable only by you. Default: `$XDG_CACHE_HOME/codesonar-lsp`. |
| `cachedAnalyses`         | How many finished analyses of each hub project are kept downloaded, the most recently shown ones. `0` turns the cache off. Default: `10`. |
| `analyze.command`        | The CodeSonar command. Default: `codesonar`.                    |
| `analyze.projectFile`    | Project prefix, relative to the workspace root: CodeSonar names the project files after it, e.g. `<prefix>.conf`. Detected if not set, else `.codesonar/project`. |
| `analyze.buildCommand`   | Build command, a string or a list. It must build everything, since CodeSonar only sees what is compiled. Default: `make -B`. |
| `analyze.incrementalBuildCommand` | Build command of incremental analyses, which only need what changed since the last analysis. Default: `make`. |
| `analyze.confFile`       | Configuration file of the workspace, relative to its root, passed with `-conf-file` if it exists. Default: `codesonar.conf`. |
| `analyze.project`        | Hub project path. Default: `/` and `hub.project`.               |
| `analyze.presets`        | CodeSonar presets of analyses, e.g. `["misrac2025", "cert"]`. |
| `analyze.args`           | Further arguments of `codesonar analyze`.                       |

The file watcher covers `**/*.sarif`. Use `codesonar.refresh` for other files
or for the hub.

## The workspace configuration

What an analysis is about belongs to the workspace, not to the editor:
the server reads it from `codesonar.conf` in the workspace root
(`analyze.confFile`), which `codesonar analyze` gets with `-conf-file`.
CodeSonar's own settings stay as they are, e.g. `HUB_ADDRESS` names the
hub. CodeSonar has no settings for the hub project, the presets and the
build command, so the server keeps them in comments of a section that it
writes, e.g. for `codesonar.configureAnalysis` or `codesonar.connect`:

```
# BEGIN codesonar-lsp: written by codesonar-lsp, e.g. :CodeSonarPresets
# codesonar-lsp: project = /mixxx
# codesonar-lsp: presets = security concurrency
# codesonar-lsp: build = cmake --build build --clean-first # full rebuild, for the first analysis
# codesonar-lsp: incremental-build = cmake --build build    # what changed, for later analyses
WARNING_FILTER += allow class="Function Too Long"
# END codesonar-lsp

HUB_ADDRESS = hub.example.com:7340
INCREMENTAL_BUILD = Yes
```

`HUB_ADDRESS` is CodeSonar's own setting, so the server keeps it outside
the section. `codesonar.initConf` starts the configuration with the
project, the build commands, `INCREMENTAL_BUILD` and `HUB_ADDRESS`. Only the first analysis
needs the full rebuild, since CodeSonar only sees what is compiled; later
ones are incremental and take the rest from their parent.

The section wins over the editor settings: `HUB_ADDRESS` over `hub.url`
(keeping its scheme), `project` over `hub.project` and
`analyze.project`, `presets`, `build` and `incremental-build` over the
`analyze` settings. So the editor settings only need the credentials. The
checks are rules for the warning classes that differ from CodeSonar's
default; the presets and checks on offer come from the CodeSonar
installation (`codesonar.analysisOptions`). Lines edited by hand in the
section are kept, everything outside it is left alone.

The credentials only go to the hub of `hub.url` and to hubs the user
trusts. Before the server signs in to another hub that `HUB_ADDRESS` names,
it asks with `window/showMessageRequest` whether to "Sign in". That answer
is remembered in `trusted_hubs.json` in the state directory; any other
answer uses the hub without signing in until the server stops.

## How the project is detected

Without a project in the workspace configuration, `hub.project` and
`analyze.projectFile`, the server looks for the
project files of an earlier `codesonar analyze` in the workspace root and
in `.codesonar`, e.g. `mixxx.prj_files`. Their prefix is used for new
analyses, and the hub project of their last analysis is shown. Without
project files, the hub project named like the workspace folder is shown.

## How changed code is detected

For each warning the server records the analyzed line and the closest
non-blank lines around it. It reads them from the file while the file is
unchanged since the analysis, and otherwise from the analyzed sources that
the hub keeps. A warning then follows its line through edits. Once the line
itself was edited or deleted, the warning counts as changed.

## Commands

Clients run these with `workspace/executeCommand`. Each takes at most one
argument, an object.

| Command                     | Argument                                         | Result |
| --------------------------- | ------------------------------------------------ | ------ |
| `codesonar.connect`         | `{url, username, password, project}`, all optional | The hub projects; with `project`, its newest analysis is shown |
| `codesonar.projects`        |                                                  | `[{id, name}]` |
| `codesonar.analyses`        |                                                  | `{project, analyses: [{id, name, started, state, finished, current, cached}]}` |
| `codesonar.deleteAnalysis`  | `{analysis}`                                     | Deletes the analysis from the hub and returns the analyses like `codesonar.analyses`, with `deleted` and the reason in `message` if the hub refused, e.g. without permission. If it was shown, the newest finished one is loaded afterwards |
| `codesonar.uncache`         | `{analysis}`                                     | Removes the analysis from the cache of downloaded analyses and returns the analyses like `codesonar.analyses` |
| `codesonar.load`            | `{analysis: id}`, `{file: path}`, `{latest: true}` or none | The status |
| `codesonar.hide`            |                                                  | Hides the shown analysis and its warnings until one is loaded with `codesonar.load`. Returns the status |
| `codesonar.toggle`          |                                                  | Hides the shown analysis like `codesonar.hide`, or shows the hidden one again. Returns the status |
| `codesonar.status`          |                                                  | `{loaded, hidden, hub, project, presets, conf, name, key, hubId, url, total, shown, counts}`; `conf` is the workspace configuration, if it exists, `counts` has `unchanged`, `moved`, `changed`, `unverified` and `resolved` warnings |
| `codesonar.warnings`        |                                                  | The shown diagnostics with `uri`, `status` and `hubUri` |
| `codesonar.analyzeCommand`  | `{name}`, optional                               | `{command, cwd, temporaryFiles}`: run it, then delete the files |
| `codesonar.project`         | `{createConf}`, optional                         | `{prefix, conf, confExists, created, incremental, workspaceConf, workspaceConfExists, section, suggested: {project, build, incremental, incrementalBuild, hubAddress}}`: the project files and the workspace configuration; `createConf` creates the configuration file of the project from CodeSonar's template, `section` tells whether the workspace configuration has the section of the server |
| `codesonar.initConf`        | `{project, build, incremental, incrementalBuild, hubAddress}` | Starts the workspace configuration with them and returns the project |
| `codesonar.presets`         |                                                  | `[{name, description, selected}]`: the presets of the CodeSonar installation |
| `codesonar.analysisOptions` |                                                  | `{conf, presets: [{name, description, selected}], checks: [{class, language, default, enabled}]}`: the presets and checks of the CodeSonar installation, as the workspace configuration chooses them |
| `codesonar.configureAnalysis` | `{presets: [name], checks: [{class, enabled}]}`, both optional | Writes them into the workspace configuration and returns the options |
| `codesonar.explain`         | `{uri, fingerprint}` or `{uri, range}`           | `[{title, ruleId, message, description, status, related, events, rules, helpUri, hubUri, markdown}]` |
| `codesonar.resolve`         | `{uri, fingerprint, resolution}` or `{uri, range, resolution}`; `resolution` is `fixed` or `falsePositive`, `quiet` is optional | Marks the warnings on the hub: the state `Fixed` or the finding `False Positive`. The hub keeps it for later analyses, and resolved warnings are not shown. Returns `{resolved, message}` and shows the message, unless `quiet` is set |
| `codesonar.disableCheck`    | `{class}`, `quiet` is optional                   | Disables the check in the section of the workspace configuration, for later analyses, and hides its warnings now. Returns `{disabled, message}` and shows the message, unless `quiet` is set |
| `codesonar.refresh`         |                                                  | Loads the shown analysis again |
| `codesonar.openInHub`       | The warning URL                                  | Opens it with `window/showDocument` |

When an analysis starts loading, the server sends the notification
`codesonar/loading` with `{analysis}`, `{project}` or `{file}`. Large
analyses take a minute, because the hub creates the SARIF export while
sending it. If the client supports `window.workDoneProgress`, the server
reports the download as work done progress, e.g. `Downloading 4.2 MB`.
Finished analyses are kept downloaded (`cachedAnalyses`), so they show up
quickly the next time. After an analysis was shown, it sends `codesonar/loaded` with the status.
