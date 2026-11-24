# codesonar-lsp

A language server that shows CodeSonar warnings in your editor, from a
CodeSonar hub or from an exported SARIF file.

- **Diagnostics** for every warning, with severity, warning class and
  related locations.
- **Follows your edits**: warnings move along with their code, and warnings
  whose line you edited disappear, since they are usually fixed.
- **Explanations** on hover, with links to the documentation and the hub.
- **Quick fixes**, if the SARIF export contains them.
- **Resolve** warnings as fixed or as false positives on the hub, as code
  actions. Resolved warnings are hidden, also in later analyses.
- **Disable checks** whose warnings you don't want, as a code action: the
  check is turned off in `codesonar.conf` and its warnings are hidden.
- **Analyses**: choose, show or hide, and delete the analyses of a hub
  project, or start a new, incremental one for the workspace.

## Installation

Download it from the
[releases](https://github.com/jclsn/codesonar-lsp/releases). The Linux
binary is static, so its packages install on any release of Debian,
Ubuntu, RHEL or AlmaLinux:

```sh
sudo apt install ./codesonar-lsp_0.1.0-1_amd64.deb    # Debian, Ubuntu
sudo dnf install ./codesonar-lsp-0.1.0-1.x86_64.rpm   # RHEL, AlmaLinux
```

On other systems, put the binary of the `.tar.gz` (Linux) or the `.zip`
(Windows) into a directory in your `PATH`.

Or build it yourself:

```sh
cargo install --path .
```

This installs `codesonar-lsp` into `~/.cargo/bin`, which must be in `$PATH`.

## Setup with Vim and yegappan/lsp

1. Install [yegappan/lsp] and [codesonar.vim], e.g. with vim-plug:

   ```vim
   Plug 'yegappan/lsp'
   Plug 'jclsn/codesonar.vim'
   ```

2. Add the server like your other language servers. Only your hub
   credentials go here:

   ```vim
   let codesonar = #{
         \ name: 'codesonar',
         \ filetype: ['c', 'cpp'],
         \ path: expand('~/.cargo/bin/codesonar-lsp'),
         \ initializationOptions: #{
         \   hub: #{
         \     username: 'alice',
         \     passwordFile: '~/.config/codesonar/password',
         \   },
         \ },
         \ }
   call LspAddServer([codesonar])
   ```

   If you collect your servers in a list, `call add(lspServers, codesonar)`
   works just as well.

3. Open a file of your project and run `:CodeSonarConf`. It asks for the
   hub address, the hub project and the build commands and writes them into
   the `codesonar.conf` of the workspace, see below.

4. `:CodeSonarAnalyses` lists the analyses of the project; `:CodeSonarAnalyze`
   runs a new one. See `:help codesonar` for all commands.

The warnings are regular diagnostics, so `:LspDiag next`, `:LspHover` and
`:LspCodeAction` work as usual. Without codesonar.vim, the server works with
plain yegappan/lsp too, but then the commands for analyses are missing.

[yegappan/lsp]: https://github.com/yegappan/lsp
[codesonar.vim]: https://github.com/jclsn/codesonar.vim

## The workspace configuration

Everything about the analysis of a project lives in `codesonar.conf` in the
workspace root, which `codesonar analyze` also reads. The server keeps its
own settings in a section of comments that it writes for you:

```
# BEGIN codesonar-lsp: written by codesonar-lsp, e.g. :CodeSonarPresets
# codesonar-lsp: project = /my_project
# codesonar-lsp: presets = security
# codesonar-lsp: build = make -B                 # full rebuild, for the first analysis
# codesonar-lsp: incremental-build = make        # what changed, for later analyses
# END codesonar-lsp

HUB_ADDRESS = hub.example.com:7340
INCREMENTAL_BUILD = Yes
```

- `project`: the hub project whose newest analysis is shown.
- `presets`: CodeSonar presets of new analyses (`:CodeSonarPresets`).
- `build`, `incremental-build`: the build commands. Only the first analysis
  needs a full rebuild, since CodeSonar only sees what is compiled.
- Checks chosen with `:CodeSonarChecks` or disabled with the code action
  become `WARNING_FILTER` rules in the section. Warnings of disabled checks
  are not shown.

`HUB_ADDRESS` and `INCREMENTAL_BUILD` are CodeSonar's own settings and stay
outside the section. You can edit the section by hand; everything else in
the file is left alone.

Any repository can name any hub in `HUB_ADDRESS`. So before the server
signs in to a hub with your credentials, it asks you, unless it is the hub
of `hub.url` or one you trusted before. It remembers "Sign in" in its state
directory; otherwise it uses the hub without signing in.

An analysis runs the build commands of the section, just as `make` runs a
Makefile, so only analyze repositories you trust.

## Settings

The settings are the `initializationOptions` of the server. Usually only the
credentials are needed, the rest comes from `codesonar.conf`.

| Setting                  | Description                                                    |
| ------------------------ | -------------------------------------------------------------- |
| `hub.url`                | Hub URL, if `codesonar.conf` has no `HUB_ADDRESS`, or to use `https`. |
| `hub.username`           | Hub user. Without one, the hub is accessed anonymously.        |
| `hub.passwordFile`       | File with the password. Alternatives: `hub.password`, `$CODESONAR_PASSWORD`. |
| `hub.project`            | Hub project, if `codesonar.conf` names none.                   |
| `hub.caCertificate`      | PEM file with an additional trusted CA certificate.            |
| `hub.acceptInvalidCerts` | Do not verify the hub's certificate.                           |
| `sarifFile`              | SARIF file to show instead of a hub, relative to the workspace root. |
| `pathMappings`           | `[{from, to}]`: paths of the analysis machine to local paths.  |
| `changed`                | Warnings in changed code: `hide` (default) or `mark` them as hints. |
| `cachedAnalyses`         | Downloaded analyses kept per project. Default: `10`, `0` turns it off. |
| `analyze.command`        | The CodeSonar command. Default: `codesonar`.                   |
| `analyze.args`           | Further arguments of `codesonar analyze`.                      |

Without a project in `codesonar.conf` or `hub.project`, the server uses the
project files of an earlier `codesonar analyze` in the workspace, or else the
hub project named like the workspace folder.

For all settings, the commands and notifications for editor clients, and how
changed code is detected, see [docs/protocol.md](docs/protocol.md).

## Other editors

CLion: install the [codesonar-clion] plugin. It starts the server for C and
C++ files and adds its commands to the IDE as actions, e.g. to choose
analyses, explain and resolve warnings and run new analyses.

[codesonar-clion]: https://github.com/jclsn/codesonar-clion

Neovim:

```lua
vim.lsp.config("codesonar", {
  cmd = { "codesonar-lsp" },
  filetypes = { "c", "cpp" },
  root_markers = { "codesonar.conf", ".git" },
  init_options = { hub = { username = "alice", passwordFile = "~/.config/codesonar/password" } },
})
vim.lsp.enable("codesonar")
```

Helix:

```toml
[language-server.codesonar]
command = "codesonar-lsp"
config = { hub = { username = "alice", passwordFile = "~/.config/codesonar/password" } }

[[language]]
name = "c"
language-servers = ["clangd", "codesonar"]
```

## Examples

- `examples/demo`: a C file with typical defects and a matching SARIF file.
  Open it with the setting `{ "sarifFile": "codesonar.sarif" }`. After
  changing `main.c`, run `python3 gen_sarif.py main.c codesonar.sarif`.
- `examples/test_project`: a C program with a `codesonar.conf` for the hub
  project `test_project`.

## Development

```sh
cargo fmt
cargo clippy --all-targets
cargo test
```

The tests of codesonar.vim run against the installed server, so run them
after `cargo install --path .` as well.

To release a version, set it in `Cargo.toml`, commit, and push a tag
named after it:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The release workflow then runs the tests, builds the binaries for Linux
and Windows and the Debian and RPM packages, installs the packages on old
and new releases of the distributions, and publishes everything as a
GitHub release with checksums.
