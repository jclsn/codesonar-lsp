# codesonar-lsp

A language server that shows CodeSonar warnings directly in the editor.

## Features

- **Diagnostics** for every CodeSonar warning, including severity, warning class
  and related locations.
- **Explanations** on hover: warning class, message, description and links to
  the documentation and to the warning on the hub.
- **Quick fixes**, if the SARIF export contains fixes.
- **Open in CodeSonar Hub** as a code action.
- **Reload** when the SARIF file changes or via the `codesonar.refresh` command.

The warnings are read from a SARIF export, either from a local file or
directly from the CodeSonar hub.

## Installation

```sh
cargo install --path .
```

## Settings

The settings are passed as `initializationOptions`. They can be changed later
via `workspace/didChangeConfiguration` under the `codesonar` key.

```json
{
  "sarifFile": "codesonar.sarif",
  "hub": {
    "url": "https://hub.example.com:7340",
    "analysisId": "42",
    "username": "alice",
    "password": "secret",
    "sarifPath": "/analysis/{analysisId}-allwarnings.sarif",
    "caCertificate": "/etc/ssl/hub-ca.pem",
    "acceptInvalidCerts": false
  },
  "pathMappings": [
    { "from": "C:\\build\\project", "to": "/home/alice/project" }
  ]
}
```

| Setting        | Description                                                        |
| -------------- | ------------------------------------------------------------------ |
| `sarifFile`    | SARIF file, absolute or relative to the workspace root.            |
| `hub`          | Download the warnings from the hub. Takes precedence over `sarifFile`. |
| `pathMappings` | Maps paths from the analysis machine to local paths.               |

If `hub.password` is not set, the `CODESONAR_PASSWORD` environment variable is
used. Without `hub.username` the hub is accessed anonymously.

The file watcher covers `**/*.sarif`. Use `codesonar.refresh` for other files
or for the hub.

## Editor setup

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
cargo fmt
cargo clippy --all-targets
cargo test
```
