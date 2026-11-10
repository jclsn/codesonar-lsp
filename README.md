# codesonar-lsp

CodeSonar warnings in the editor: a language server that does all the work,
and editor clients that build on it.

| Folder              | Content                                              |
| ------------------- | ---------------------------------------------------- |
| `server`            | The language server, in Rust                         |
| `vim`               | Vim plugin, on top of [yegappan/lsp]                 |
| `examples/demo`     | C file with a matching SARIF file                    |
| `examples/test_project` | C program whose analyses are in the hub project `test_project` |

[yegappan/lsp]: https://github.com/yegappan/lsp

## Features

- **Diagnostics** for every CodeSonar warning, including severity, warning class
  and related locations.
- **Changed code**: analyses take minutes, so the code has often changed since
  the shown one ran. Warnings move along with their code while it is edited,
  and warnings whose line was edited disappear, because they are usually
  fixed. The server remembers the analyzed code, so this also works after a
  restart.
- **Explanations** on hover: warning class, message, description and links to
  the documentation and to the warning on the hub.
- **Explain** as a code action: the explanation as structured data, for
  clients that show it in their own view.
- **Quick fixes**, if the SARIF export contains fixes.
- **Open in CodeSonar Hub** as a code action.
- **Commands** for clients: sign in to a hub, list projects and analyses,
  show an analysis and get the command line of a new analysis.

The warnings come from the CodeSonar hub or from an exported SARIF file.

## Installation

```sh
cargo install --path .
```

For the Vim plugin, add the `vim` folder to the runtime path, e.g. with
vim-plug:

```vim
Plug '~/Workspace/codesonar-lsp', {'rtp': 'vim'}
```

See `:help codesonar` for its commands.

## Settings

The settings are passed as `initializationOptions`. They can be changed later
via `workspace/didChangeConfiguration` under the `codesonar` key.

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
| `hub.project`            | Hub project, by name or id. Its newest finished analysis is shown. |
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
| `stateDir`               | Where the analyzed code is remembered. Default: `$XDG_STATE_HOME/codesonar-lsp`. |
| `analyze.command`        | The CodeSonar command. Default: `codesonar`.                    |
| `analyze.projectFile`    | CodeSonar project file, relative to the workspace root. Default: `.codesonar/project`. |
| `analyze.buildCommand`   | Build command, a string or a list. Default: `make -B`.          |
| `analyze.project`        | Hub project path. Default: `/` and `hub.project`.               |
| `analyze.args`           | Further arguments of `codesonar analyze`.                       |

The file watcher covers `**/*.sarif`. Use `codesonar.refresh` for other files
or for the hub.

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
| `codesonar.analyses`        |                                                  | `{project, analyses: [{id, name, started, state, finished, current}]}` |
| `codesonar.load`            | `{analysis: id}`, `{file: path}`, `{latest: true}` or none | The status |
| `codesonar.status`          |                                                  | `{loaded, name, key, hubId, url, total, shown, counts}` |
| `codesonar.warnings`        |                                                  | The shown diagnostics with `uri`, `status` and `hubUri` |
| `codesonar.analyzeCommand`  | `{name}`, optional                               | `{command, cwd, temporaryFiles}`: run it, then delete the files |
| `codesonar.explain`         | `{uri, fingerprint}` or `{uri, range}`           | `[{title, ruleId, message, description, status, related, helpUri, hubUri, markdown}]` |
| `codesonar.refresh`         |                                                  | Loads the shown analysis again |
| `codesonar.openInHub`       | The warning URL                                  | Opens it with `window/showDocument` |

After an analysis was shown, the server sends the notification
`codesonar/loaded` with the status.

## Other editors

### Neovim

```lua
vim.lsp.config("codesonar", {
  cmd = { "codesonar-lsp" },
  filetypes = { "c", "cpp", "java", "cs" },
  root_markers = { ".git" },
  init_options = { sarifFile = "codesonar.sarif" },
})
vim.lsp.enable("codesonar")
```

### Helix

```toml
[language-server.codesonar]
command = "codesonar-lsp"
config = { sarifFile = "codesonar.sarif" }

[[language]]
name = "c"
language-servers = ["clangd", "codesonar"]
```

## Demo

`examples/demo` contains a C file with typical defects and a matching
SARIF file in the CodeSonar format. Open the folder in your editor with
this setting:

```json
{ "sarifFile": "codesonar.sarif" }
```

After changes to `main.c`, regenerate the SARIF file with
`python3 gen_sarif.py main.c codesonar.sarif`. The hub links in the file
are placeholders and only work with a matching analysis.

## Development

```sh
cd server
cargo fmt
cargo clippy --all-targets
cargo test

cd ../vim
test/run.sh    # needs yegappan/lsp in ~/.vim/plugged/lsp or $LSP_PLUGIN
```
