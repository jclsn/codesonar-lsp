//! End-to-end tests that talk to the server binary over stdio.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};
use tower_lsp::lsp_types::Url;
use wiremock::MockServer;

const FIXTURE: &str = include_str!("fixtures/codesonar.sarif");
const TIMEOUT: Duration = Duration::from_secs(10);

struct Client {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    /// Notifications and requests received while waiting for something else.
    pending: Vec<Value>,
    next_id: i64,
    /// The action chosen in questions of the server; none dismisses them.
    answer: Option<String>,
    _state: tempfile::TempDir,
}

impl Client {
    fn start() -> Self {
        // Keeps the memory and the cache of the tests away from the user's.
        let state = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesonar-lsp"))
            .env("XDG_STATE_HOME", state.path())
            .env("XDG_CACHE_HOME", state.path().join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);

            while let Some(message) = read_message(&mut reader) {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin,
            messages,
            pending: Vec::new(),
            next_id: 1,
            answer: None,
            _state: state,
        }
    }

    fn initialize(root: &Path, options: Value) -> Self {
        Self::initialize_with(root, options, json!({}))
    }

    fn initialize_with(root: &Path, options: Value, capabilities: Value) -> Self {
        let mut client = Self::start();
        let root = Url::from_file_path(root).unwrap();
        let result = client.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root,
                "capabilities": capabilities,
                "initializationOptions": options,
            }),
        );
        assert_eq!(result["capabilities"]["hoverProvider"], json!(true));
        client.notify("initialized", json!({}));
        client
    }

    fn send(&mut self, message: Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let response = self.respond(method, params);
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    /// Sends a request and returns the whole response.
    fn respond(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});

        if !params.is_null() {
            request["params"] = params;
        }

        self.send(request);
        self.wait_for(|message| message["id"] == id && message.get("method").is_none())
    }

    fn command(&mut self, command: &str, argument: Value) -> Value {
        let arguments = if argument.is_null() {
            json!([])
        } else {
            json!([argument])
        };
        self.request(
            "workspace/executeCommand",
            json!({"command": command, "arguments": arguments}),
        )
    }

    /// Waits for a notification or request from the server and returns its params.
    fn expect(&mut self, method: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        self.wait_for(|message| message["method"] == method && predicate(&message["params"]))
            ["params"]
            .clone()
    }

    fn wait_for(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.pending.iter().position(&predicate) {
            return self.pending.remove(index);
        }

        loop {
            let message = self
                .messages
                .recv_timeout(TIMEOUT)
                .expect("timed out waiting for the server");
            // Answer requests from the server, like window/showDocument.

            if message.get("method").is_some() && message.get("id").is_some() {
                let id = message["id"].clone();
                let result = match message["method"].as_str() {
                    Some("window/showDocument") => json!({"success": true}),
                    Some("window/showMessageRequest") => match &self.answer {
                        Some(title) => json!({"title": title}),
                        None => Value::Null,
                    },
                    _ => Value::Null,
                };
                self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
            }

            if predicate(&message) {
                return message;
            }

            self.pending.push(message);
        }
    }

    fn shutdown(mut self) {
        self.request("shutdown", Value::Null);
        self.send(json!({"jsonrpc": "2.0", "method": "exit"}));
        drop(self.stdin);
        let status = self.child.wait().unwrap();
        assert!(status.success());
    }
}

fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;

    loop {
        let mut line = String::new();

        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }

        let line = line.trim_end();

        if line.is_empty() {
            break;
        }

        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = value.parse().ok();
        }
    }

    let mut body = vec![0; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn options(root: &Path) -> Value {
    json!({
        "sarifFile": "codesonar.sarif",
        "pathMappings": [{"from": "/build", "to": root}],
    })
}

fn uri(path: impl AsRef<Path>) -> Value {
    json!(Url::from_file_path(path).unwrap())
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("codesonar.sarif"), FIXTURE).unwrap();
    dir
}

#[test]
fn publishes_diagnostics() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));

    let loading = client.expect("codesonar/loading", |_| true);
    assert_eq!(loading, json!({"file": dir.path().join("codesonar.sarif")}));

    let params = client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);
    let diagnostics = params["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["code"], "LANG.MEM.NPD");
    assert_eq!(diagnostics[0]["source"], "codesonar");
    assert_eq!(diagnostics[0]["severity"], 1);
    assert_eq!(
        diagnostics[0]["range"]["start"],
        json!({"line": 4, "character": 4})
    );

    let util = uri(dir.path().join("src/util file.c"));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == util);

    client.shutdown();
}

#[test]
fn explains_warning_on_hover() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);

    let hover = client.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": main}, "position": {"line": 4, "character": 5}}),
    );

    assert_eq!(hover["contents"]["kind"], "markdown");
    let text = hover["contents"]["value"].as_str().unwrap();
    assert!(text.contains("Null Pointer Dereference"), "{text}");
    assert!(text.contains("Check pointers **before** use."), "{text}");

    let hover = client.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": main}, "position": {"line": 0, "character": 0}}),
    );

    assert_eq!(hover, Value::Null);

    client.shutdown();
}

#[test]
fn offers_quick_fix_and_opens_hub() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);

    let range = json!({"start": {"line": 4, "character": 4}, "end": {"line": 4, "character": 4}});
    let actions = client.request(
        "textDocument/codeAction",
        json!({"textDocument": {"uri": main}, "range": range, "context": {"diagnostics": []}}),
    );

    let actions = actions.as_array().unwrap();
    assert_eq!(actions.len(), 4);
    assert_eq!(actions[0]["kind"], "quickfix");
    assert_eq!(actions[0]["title"], "Check 'p' for NULL");
    let edits = &actions[0]["edit"]["changes"][main.as_str().unwrap()];
    assert_eq!(edits[0]["newText"], "if (p) ");

    let command = actions[2]["command"].clone();
    let result = client.request(
        "workspace/executeCommand",
        json!({"command": command["command"], "arguments": command["arguments"]}),
    );

    assert_eq!(result, Value::Null);

    let show = client.wait_for(|message| message["method"] == "window/showDocument");
    assert_eq!(
        show["params"]["uri"],
        "https://hub.example.com:7340/warninginstance/12.html"
    );
    assert_eq!(show["params"]["external"], true);

    // Only web links, other schemes can start other programs.
    let response = client.respond(
        "workspace/executeCommand",
        json!({"command": "codesonar.openInHub", "arguments": ["file:///etc/passwd"]}),
    );

    assert_eq!(response["error"]["code"], -32602, "{response}");

    client.shutdown();
}

#[test]
fn disables_checks() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] != json!([])
    });

    let disabled = client.command("codesonar.disableCheck", json!({"class": "LANG.MEM.NPD"}));

    assert_eq!(disabled["disabled"], true);
    assert_eq!(
        disabled["message"],
        "Disabled the check for 'LANG.MEM.NPD' in codesonar.conf"
    );

    client.expect("window/showMessage", |p| {
        p["type"] == 3
            && p["message"] == "CodeSonar: Disabled the check for 'LANG.MEM.NPD' in codesonar.conf"
    });
    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] == json!([])
    });

    let conf = std::fs::read_to_string(dir.path().join("codesonar.conf")).unwrap();
    assert!(conf.contains("WARNING_FILTER += discard class=\"LANG.MEM.NPD\""));

    // Enabled again, its warnings are back. The options it returns need a
    // CodeSonar installation, which the test does not have.
    client.respond(
        "workspace/executeCommand",
        json!({"command": "codesonar.configureAnalysis",
               "arguments": [{"checks": [{"class": "LANG.MEM.NPD", "enabled": true}]}]}),
    );

    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] != json!([])
    });

    client.shutdown();
}

#[test]
fn explains_warning() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);

    let range = json!({"start": {"line": 4, "character": 4}, "end": {"line": 4, "character": 4}});
    let actions = client.request(
        "textDocument/codeAction",
        json!({"textDocument": {"uri": main}, "range": range, "context": {"diagnostics": []}}),
    );

    let command = actions[1]["command"].clone();
    assert_eq!(command["command"], "codesonar.explain");

    let result = client.request(
        "workspace/executeCommand",
        json!({"command": command["command"], "arguments": command["arguments"]}),
    );

    assert_eq!(result[0]["title"], "Null Pointer Dereference");
    assert_eq!(result[0]["ruleId"], "LANG.MEM.NPD");
    assert_eq!(result[0]["related"][0]["line"], 3);

    let at = client.request(
        "workspace/executeCommand",
        json!({"command": "codesonar.explain", "arguments": [
            {"uri": main, "range": {"start": {"line": 4, "character": 0}, "end": {"line": 5, "character": 0}}}
        ]}),
    );

    assert_eq!(at, result);

    let response = client.respond(
        "workspace/executeCommand",
        json!({"command": "codesonar.explain", "arguments": []}),
    );

    assert_eq!(response["error"]["code"], -32602);

    client.shutdown();
}

#[test]
fn refresh_clears_fixed_warnings() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);

    let mut log: Value = serde_json::from_str(FIXTURE).unwrap();
    log["runs"][0]["results"].as_array_mut().unwrap().remove(0);
    std::fs::write(dir.path().join("codesonar.sarif"), log.to_string()).unwrap();

    client.request(
        "workspace/executeCommand",
        json!({"command": "codesonar.refresh", "arguments": []}),
    );

    let params = client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] == json!([])
    });
    assert_eq!(params["diagnostics"], json!([]));

    client.shutdown();
}

#[test]
fn hides_the_analysis_until_one_is_loaded() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] != json!([])
    });

    let status = client.command("codesonar.hide", Value::Null);
    assert_eq!(status["loaded"], false);
    assert_eq!(status["hidden"], true);

    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] == json!([])
    });
    client.expect("codesonar/loaded", |p| p["loaded"] == false);

    // Neither a refresh nor new settings show it again.
    client.pending.clear();
    client.command("codesonar.refresh", Value::Null);

    let mut settings = options(dir.path());
    settings["changed"] = json!("mark");
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings": {"codesonar": settings}}),
    );

    // The server handles messages concurrently, so a reload may finish later.
    std::thread::sleep(std::time::Duration::from_millis(300));
    client.pending.extend(client.messages.try_iter());

    assert!(
        !client
            .pending
            .iter()
            .any(|message| message["params"]["loaded"] == true),
        "not loaded again"
    );
    assert_eq!(
        client.command("codesonar.status", Value::Null)["loaded"],
        false
    );

    // Toggled, it is shown again.
    let status = client.command("codesonar.toggle", Value::Null);
    assert_eq!(status["loaded"], true);
    assert_eq!(status["hidden"], false);

    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] != json!([])
    });

    // Toggled again, it is hidden.
    let status = client.command("codesonar.toggle", Value::Null);
    assert_eq!(status["loaded"], false);
    assert_eq!(status["hidden"], true);

    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] == json!([])
    });

    client.shutdown();
}

#[test]
fn reports_invalid_source_after_configuration_change() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);

    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings": {"codesonar": {"sarifFile": "missing.sarif"}}}),
    );

    let message = client.expect("window/showMessage", |p| p["type"] == 1);
    let text = message["message"].as_str().unwrap();
    assert!(text.contains("missing.sarif"), "{text}");

    client.shutdown();
}

#[test]
fn keeps_the_analysis_when_the_analyze_settings_change() {
    let dir = workspace();
    let mut client = Client::initialize(dir.path(), options(dir.path()));
    client.expect("codesonar/loaded", |_| true);

    let mut settings = options(dir.path());
    settings["analyze"] = json!({"projectFile": "cs/app"});
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings": {"codesonar": settings}}),
    );

    let project = client.command("codesonar.project", Value::Null);
    assert_eq!(project["prefix"], json!(dir.path().join("cs/app")));

    // The server handles messages concurrently, so a reload may finish later.
    std::thread::sleep(std::time::Duration::from_millis(300));
    client.pending.extend(client.messages.try_iter());

    assert!(
        !client
            .pending
            .iter()
            .any(|message| message["method"] == "codesonar/loaded"),
        "not loaded again"
    );

    // Other settings still reload it.
    settings["changed"] = json!("mark");
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings": {"codesonar": settings}}),
    );

    client.expect("codesonar/loaded", |_| true);

    client.shutdown();
}

#[test]
fn reloads_when_sarif_file_changes() {
    let dir = workspace();
    let capabilities =
        json!({"workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}}});
    let mut client = Client::initialize_with(dir.path(), options(dir.path()), capabilities);

    let registration = client.expect("client/registerCapability", |_| true);
    let registration = &registration["registrations"][0];
    assert_eq!(registration["method"], "workspace/didChangeWatchedFiles");
    assert_eq!(
        registration["registerOptions"]["watchers"][0]["globPattern"],
        "**/*.sarif"
    );

    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);

    let mut log: Value = serde_json::from_str(FIXTURE).unwrap();
    log["runs"][0]["results"].as_array_mut().unwrap().remove(0);
    let sarif = dir.path().join("codesonar.sarif");
    std::fs::write(&sarif, log.to_string()).unwrap();

    // Changes to other files are ignored.
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(dir.path().join("other.sarif")), "type": 2}]}),
    );

    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&sarif), "type": 2}]}),
    );

    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] == json!([])
    });

    client.shutdown();
}

const MAIN: &str = "#include <stdlib.h>\n\nint main(void)\n{\n    *p = 1;\n    return 0;\n}\n";

/// A workspace with the analyzed source and a SARIF log of the given time.
fn workspace_with_source(time: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut log: Value = serde_json::from_str(FIXTURE).unwrap();
    log["runs"][0]["invocations"] = json!([{"endTimeUtc": time}]);
    std::fs::write(dir.path().join("codesonar.sarif"), log.to_string()).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.c"), MAIN).unwrap();
    dir
}

fn tracking_options(root: &Path) -> Value {
    let mut options = options(root);
    options["stateDir"] = json!(root.join("state"));
    options
}

fn lines(diagnostics: &Value) -> Vec<u64> {
    diagnostics
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["range"]["start"]["line"].as_u64().unwrap())
        .collect()
}

#[test]
fn follows_edits_and_hides_changed_warnings() {
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut client = Client::initialize(dir.path(), tracking_options(dir.path()));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);

    let document = json!({"uri": main, "languageId": "c", "version": 1, "text": MAIN});
    client.notify("textDocument/didOpen", json!({"textDocument": document}));
    let params = client.expect("textDocument/publishDiagnostics", |p| p["version"] == 1);
    assert_eq!(lines(&params["diagnostics"]), [4]);

    let change = |version: i32, text: String| {
        json!({
            "textDocument": {"uri": main, "version": version},
            "contentChanges": [{"text": text}],
        })
    };

    client.notify(
        "textDocument/didChange",
        change(2, format!("// moved\n{MAIN}")),
    );

    let params = client.expect("textDocument/publishDiagnostics", |p| p["version"] == 2);
    assert_eq!(lines(&params["diagnostics"]), [5]);

    let range = json!({"start": {"line": 5, "character": 4}, "end": {"line": 5, "character": 4}});
    let actions = client.request(
        "textDocument/codeAction",
        json!({"textDocument": {"uri": main}, "range": range, "context": {"diagnostics": []}}),
    );

    let edits = &actions[0]["edit"]["changes"][main.as_str().unwrap()];
    assert_eq!(edits[0]["range"]["start"]["line"], 5, "the fix moved along");

    let fixed = MAIN.replace("*p = 1;", "if (p) *p = 1;");
    client.notify("textDocument/didChange", change(3, fixed));
    let params = client.expect("textDocument/publishDiagnostics", |p| p["version"] == 3);
    assert_eq!(params["diagnostics"], json!([]));

    client.shutdown();
}

#[test]
fn remembers_analyzed_code_after_restart() {
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let main = uri(dir.path().join("src/main.c"));
    let mut client = Client::initialize(dir.path(), tracking_options(dir.path()));
    client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);

    client.shutdown();

    // Without the memory, the edited file would count as analyzed code.
    let fixed = MAIN.replace("*p = 1;", "if (p) *p = 1;");
    std::fs::write(dir.path().join("src/main.c"), fixed).unwrap();

    let mut client = Client::initialize(dir.path(), tracking_options(dir.path()));
    let params = client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);
    assert_eq!(params["diagnostics"], json!([]));

    client.shutdown();
}

#[test]
fn does_not_learn_from_files_edited_after_the_analysis() {
    let dir = workspace_with_source("2000-01-01T00:00:00Z");
    let main = uri(dir.path().join("src/main.c"));
    let mut options = tracking_options(dir.path());
    options["changed"] = json!("mark");
    let mut client = Client::initialize(dir.path(), options);

    let params = client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);
    assert_eq!(lines(&params["diagnostics"]), [4]);
    assert_eq!(
        params["diagnostics"][0]["severity"], 1,
        "unverified, not changed"
    );
    assert!(!dir.path().join("state/memory.json").exists());

    client.shutdown();
}

/// A mock hub with project `test` whose analysis 3 found the warnings of
/// the fixture in `MAIN`.
fn mock_hub(runtime: &tokio::runtime::Runtime, time: &str) -> MockServer {
    use wiremock::matchers::path;
    use wiremock::{Mock, ResponseTemplate};

    let mut log: Value = serde_json::from_str(FIXTURE).unwrap();
    log["runs"][0]["invocations"] = json!([{"endTimeUtc": time}]);

    let source: Vec<Value> = MAIN
        .lines()
        .enumerate()
        .map(|(i, line)| json!({"type": "wdom_line", "number": i + 1, "children": [format!("{line}\n")]}))
        .collect();

    let routes = [
        (
            "/project_search.json",
            json!({"rows": [{"project": "test", "url": "/project/1.json"}]}),
        ),
        (
            "/project/1.json",
            json!({"rows": [
                {"analysis": "running", "state": "ANALYZING", "url": "/analysis/4.json"},
                {"analysis": "done", "state": "FINISHED", "url": "/analysis/3.json"},
            ]}),
        ),
        ("/analysis/3-allwarnings.sarif", log),
        (
            "/analysis/3-files.json",
            json!({"rows": [{"file": "/build/src/main.c", "url": "/analysis/3-file1.json"}]}),
        ),
        ("/analysis/3-file1.json", json!({"children": source})),
        (
            "/analysis/3.json",
            json!({"id": 3, "project": {"id": 1, "name": "test"}}),
        ),
        (
            "/warninginstance/12.json",
            json!({"categories": [
                {"link": "LANG.MEM.NPD", "url": "/doc/NPD.html", "relationship": null, "rule_text": null},
                {"link": "CWE:476", "url": "https://cwe/476/", "relationship": "intersects",
                 "rule_text": "NULL Pointer Dereference"},
            ]}),
        ),
    ];

    runtime.block_on(async {
        let server = MockServer::start().await;

        for (route, body) in routes {
            Mock::given(path(route))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }

        server
    })
}

fn hub_options(root: &Path, hub: &MockServer) -> Value {
    json!({
        "hub": {"url": hub.uri(), "project": "test"},
        "pathMappings": [{"from": "/build", "to": root}],
        "stateDir": root.join("state"),
    })
}

#[test]
fn learns_analyzed_code_from_the_hub() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2000-01-01T00:00:00Z");
    let dir = workspace_with_source("2000-01-01T00:00:00Z");
    let fixed = MAIN.replace("*p = 1;", "if (p) *p = 1;");
    std::fs::write(dir.path().join("src/main.c"), fixed).unwrap();

    let mut client = Client::initialize(dir.path(), hub_options(dir.path(), &hub));
    let main = uri(dir.path().join("src/main.c"));

    let params = client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);
    assert_eq!(lines(&params["diagnostics"]), [4], "unverified at first");

    let params = client.expect("textDocument/publishDiagnostics", |p| p["uri"] == main);
    assert_eq!(
        params["diagnostics"],
        json!([]),
        "changed since the analysis"
    );

    client.shutdown();
}

#[test]
fn chooses_analyses_with_commands() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut client = Client::initialize(dir.path(), hub_options(dir.path(), &hub));
    let loaded = client.expect("codesonar/loaded", |_| true);
    assert_eq!(loaded["hubId"], "3", "newest finished analysis");
    assert_eq!(loaded["name"], "done");
    assert_eq!(loaded["url"], format!("{}/analysis/3.html", hub.uri()));

    let analyses = client.command("codesonar.analyses", Value::Null);
    assert_eq!(analyses["project"], "test");
    let analyses = analyses["analyses"].as_array().unwrap();
    assert_eq!(analyses.len(), 2);
    assert_eq!(analyses[0]["id"], 4);
    assert_eq!(analyses[0]["finished"], false);
    assert_eq!(analyses[1]["current"], true);

    let status = client.command("codesonar.load", json!({"analysis": 3}));
    assert_eq!(status["loaded"], true);
    assert_eq!(status["total"], 3);
    assert_eq!(status["counts"]["changed"], 0);

    let warnings = client.command("codesonar.warnings", Value::Null);
    let main = &warnings.as_array().unwrap()[0];
    assert_eq!(main["uri"], uri(dir.path().join("src/main.c")));
    assert_eq!(main["status"], "unchanged");
    assert_eq!(main["code"], "LANG.MEM.NPD");

    let status = client.command(
        "codesonar.load",
        json!({"file": dir.path().join("codesonar.sarif")}),
    );

    assert_eq!(status["hubId"], Value::Null);
    assert_eq!(client.command("codesonar.status", Value::Null), status);

    let projects = client.command("codesonar.projects", Value::Null);
    assert_eq!(projects, json!([{"id": 1, "name": "test"}]));

    client.shutdown();
}

#[test]
fn reports_the_download_progress() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let capabilities = json!({"window": {"workDoneProgress": true}});
    let mut client =
        Client::initialize_with(dir.path(), hub_options(dir.path(), &hub), capabilities);

    let create = client.expect("window/workDoneProgress/create", |_| true);
    let token = create["token"].clone();
    let progress = |kind: &'static str| {
        let token = token.clone();
        move |p: &Value| p["token"] == token && p["value"]["kind"] == kind
    };

    let begin = client.expect("$/progress", progress("begin"));
    assert_eq!(begin["value"]["title"], "CodeSonar");
    assert_eq!(begin["value"]["message"], "Loading the analysis");

    let report = client.expect("$/progress", progress("report"));
    let message = report["value"]["message"].as_str().unwrap();
    assert!(message.starts_with("Downloading "), "{message}");

    client.expect("$/progress", progress("end"));
    client.expect("codesonar/loaded", |_| true);

    client.shutdown();
}

#[test]
fn shows_no_progress_without_the_capability() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut client = Client::initialize(dir.path(), hub_options(dir.path(), &hub));
    client.expect("codesonar/loaded", |_| true);

    assert!(
        client
            .pending
            .iter()
            .all(|message| message["method"] != "window/workDoneProgress/create"),
        "{:?}",
        client.pending
    );

    client.shutdown();
}

#[test]
fn caches_finished_analyses() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut options = hub_options(dir.path(), &hub);
    options["cacheDir"] = json!(dir.path().join("cache"));

    let mut client = Client::initialize(dir.path(), options);
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    let downloads = || {
        runtime
            .block_on(hub.received_requests())
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/analysis/3-allwarnings.sarif")
            .count()
    };

    let analyses = client.command("codesonar.analyses", Value::Null);
    assert_eq!(analyses["analyses"][1]["cached"], true, "finished");
    assert_eq!(analyses["analyses"][0]["cached"], false, "running");

    client.command("codesonar.load", json!({"analysis": 3}));
    assert_eq!(downloads(), 1, "loaded from the cache");

    let analyses = client.command("codesonar.uncache", json!({"analysis": 3}));
    assert_eq!(analyses["analyses"][1]["cached"], false);

    client.command("codesonar.load", json!({"analysis": 3}));
    assert_eq!(downloads(), 2);

    client.shutdown();
}

#[test]
fn deletes_analyses() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let deleted = json!({"change": {"results": [{"action": "remove_analysis",
        "item": {"type": "analysis", "id": 3}, "result": "success", "message": "Removed"}]}});

    runtime.block_on(async {
        // The analyses until they are deleted: the project of 5, which the
        // user may not delete, is looked up twice.
        for (id, lookups) in [(3, 1), (4, 1), (5, 2)] {
            Mock::given(method("GET"))
                .and(path(format!("/analysis/{id}.json")))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({"id": id, "state": "FINISHED", "project": {"id": 1, "name": "test"}}),
                ))
                .up_to_n_times(lookups)
                .with_priority(1)
                .mount(&hub)
                .await;

            Mock::given(method("GET"))
                .and(path(format!("/analysis/{id}.json")))
                .respond_with(ResponseTemplate::new(404))
                .with_priority(2)
                .mount(&hub)
                .await;
        }

        Mock::given(method("POST"))
            .and(path("/project/1.json"))
            .and(body_string_contains("remove_analysis_id=5"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"status": 403, "error": "Permission Denied"})),
            )
            .with_priority(1)
            .mount(&hub)
            .await;

        for id in [3, 4] {
            Mock::given(method("POST"))
                .and(path("/project/1.json"))
                .and(body_string_contains(format!("remove_analysis_id={id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(&deleted))
                .with_priority(1)
                .expect(1)
                .mount(&hub)
                .await;
        }
    });

    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut client = Client::initialize(dir.path(), hub_options(dir.path(), &hub));
    let main = uri(dir.path().join("src/main.c"));
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    // Another analysis than the shown one.
    let analyses = client.command("codesonar.deleteAnalysis", json!({"analysis": 4}));
    assert_eq!(analyses["project"], "test");
    assert_eq!(analyses["deleted"], true);

    // One the user may not delete.
    let refused = client.command("codesonar.deleteAnalysis", json!({"analysis": 5}));
    assert_eq!(refused["deleted"], false);
    assert_eq!(
        refused["message"],
        "anonymous hub users may not delete analysis 5 (Permission Denied)"
    );
    assert_eq!(refused["analyses"].as_array().unwrap().len(), 2);

    assert_eq!(
        client.command("codesonar.status", Value::Null)["hubId"],
        "3"
    );

    // The shown analysis, without another finished one.
    runtime.block_on(async {
        Mock::given(path("/project/1.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"rows": [
                {"analysis": "running", "state": "ANALYZING", "url": "/analysis/4.json"},
            ]})))
            .with_priority(1)
            .mount(&hub)
            .await;
    });

    client.command("codesonar.deleteAnalysis", json!({"analysis": 3}));

    let status = client.expect("codesonar/loaded", |p| p["loaded"] == false);
    assert_eq!(status["hubId"], Value::Null);

    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] == json!([])
    });

    client.shutdown();
}

#[test]
fn resolves_warnings_on_the_hub() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");

    runtime.block_on(async {
        // The user may change the state, but not the finding.
        Mock::given(method("POST"))
            .and(path("/warninginstance/12.json"))
            .and(body_string_contains("sel_finding="))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"status": 403, "error": "Permission Denied"})),
            )
            .with_priority(1)
            .mount(&hub)
            .await;

        Mock::given(method("POST"))
            .and(path("/warninginstance/12.json"))
            .and(body_string_contains("sel_state=%22Fixed%22"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 12})))
            .with_priority(1)
            .expect(1)
            .mount(&hub)
            .await;

        // The warning after the change.
        Mock::given(method("GET"))
            .and(path("/warninginstance/12.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"id": 12, "group": {"state": {"id": 3, "label": "Fixed"}}}),
                ),
            )
            .with_priority(1)
            .mount(&hub)
            .await;
    });

    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut options = hub_options(dir.path(), &hub);
    options["cacheDir"] = json!(dir.path().join("cache"));

    let mut client = Client::initialize(dir.path(), options);
    let main = uri(dir.path().join("src/main.c"));
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    // The code actions offer it.
    let range = json!({"start": {"line": 4, "character": 4}, "end": {"line": 4, "character": 4}});
    let actions = client.request(
        "textDocument/codeAction",
        json!({"textDocument": {"uri": main}, "range": range, "context": {"diagnostics": []}}),
    );

    let titles: Vec<&str> = actions
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|action| action["title"].as_str())
        .collect();
    assert!(titles.contains(&"Mark as fixed"));

    // The user may not change the finding.
    let refused = client.command(
        "codesonar.resolve",
        json!({"uri": main, "range": range, "resolution": "falsePositive"}),
    );

    assert_eq!(refused["resolved"], 0);
    assert_eq!(
        refused["message"],
        "anonymous hub users may not change warning 12 (Permission Denied)"
    );

    client.expect("window/showMessage", |p| {
        p["type"] == 1 && p["message"].as_str().unwrap().contains("may not change")
    });

    // The user may mark it as fixed.
    let resolved = client.command(
        "codesonar.resolve",
        json!({"uri": main, "range": range, "resolution": "fixed"}),
    );

    assert_eq!(resolved["resolved"], 1);
    assert_eq!(
        resolved["message"],
        "Marked 'LANG.MEM.NPD' as fixed on the hub"
    );

    client.expect("window/showMessage", |p| {
        p["type"] == 3 && p["message"] == "CodeSonar: Marked 'LANG.MEM.NPD' as fixed on the hub"
    });
    client.expect("textDocument/publishDiagnostics", |p| {
        p["uri"] == main && p["diagnostics"] == json!([])
    });

    let status = client.command("codesonar.status", Value::Null);
    assert_eq!(status["counts"]["resolved"], 1);

    // Still resolved when loaded from the cache.
    client.command("codesonar.load", json!({"analysis": 3}));

    let downloads = runtime
        .block_on(hub.received_requests())
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/analysis/3-allwarnings.sarif")
        .count();
    assert_eq!(downloads, 1, "loaded from the cache");

    assert_eq!(
        client.command("codesonar.status", Value::Null)["counts"]["resolved"],
        1
    );

    client.shutdown();
}

#[test]
fn explains_rules_with_titles_from_the_hub() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut client = Client::initialize(dir.path(), hub_options(dir.path(), &hub));
    client.expect("codesonar/loaded", |_| true);

    let main = uri(dir.path().join("src/main.c"));
    let range = json!({"start": {"line": 4, "character": 0}, "end": {"line": 5, "character": 0}});
    let explanations = client.command("codesonar.explain", json!({"uri": main, "range": range}));

    assert_eq!(
        explanations[0]["rules"],
        json!([{"id": "CWE:476", "title": "NULL Pointer Dereference",
                "relationship": "intersects", "url": "https://cwe/476/"}])
    );

    client.shutdown();
}

#[test]
fn detects_the_project_from_its_project_files() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    std::fs::create_dir(dir.path().join("app.prj_files")).unwrap();
    std::fs::write(dir.path().join("app.prj_files/aid.txt"), "3\n").unwrap();

    let mut options = hub_options(dir.path(), &hub);
    options["hub"] = json!({"url": hub.uri()});

    let mut client = Client::initialize(dir.path(), options);

    let loading = client.expect("codesonar/loading", |_| true);
    assert_eq!(loading, json!({"project": "test"}));

    let loaded = client.expect("codesonar/loaded", |_| true);
    assert_eq!(loaded["project"], "test");
    assert_eq!(loaded["hubId"], "3");

    client.command("codesonar.load", json!({"analysis": 3}));
    let loading = client.expect("codesonar/loading", |_| true);
    assert_eq!(loading, json!({"analysis": "3"}));

    let command = client.command("codesonar.analyzeCommand", Value::Null);
    let line: Vec<&str> = command["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part.as_str().unwrap())
        .collect();
    assert_eq!(line[2], dir.path().join("app").to_str().unwrap());
    assert!(line.windows(2).any(|w| w == ["-project", "/test"]));

    client.shutdown();
}

#[test]
fn detects_the_project_by_the_workspace_name() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Test");
    std::fs::create_dir(&root).unwrap();

    let mut options = hub_options(&root, &hub);
    options["hub"] = json!({"url": hub.uri()});

    let mut client = Client::initialize(&root, options);

    let loaded = client.expect("codesonar/loaded", |_| true);
    assert_eq!(loaded["project"], "test");
    assert_eq!(loaded["hubId"], "3");

    client.shutdown();
}

#[test]
fn keeps_the_chosen_project_when_the_settings_are_sent_again() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut options = hub_options(dir.path(), &hub);
    options["hub"] = json!({"url": hub.uri(), "project": "other"});

    let mut client = Client::initialize(dir.path(), options.clone());
    client.expect("window/showMessage", |p| p["type"] == 1);

    client.command("codesonar.connect", json!({"project": "test"}));
    client.expect("codesonar/loaded", |p| p["project"] == "test");

    options["analyze"] = json!({"presets": ["cert"]});
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings": {"codesonar": options}}),
    );

    // The server handles messages concurrently.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let status = client.command("codesonar.status", Value::Null);
    assert_eq!(status["project"], "test");
    assert_eq!(status["presets"], json!(["cert"]));

    client.shutdown();
}

/// A workspace whose `codesonar.conf` names the mock hub, with settings that
/// have credentials but no hub, like the setup of the README.
fn workspace_naming_the_hub(
    runtime: &tokio::runtime::Runtime,
    hub: &MockServer,
) -> (tempfile::TempDir, Value) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    runtime.block_on(
        Mock::given(method("POST"))
            .and(path("/session/create-basic-auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"bearer": "token"})))
            .mount(hub),
    );

    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    std::fs::write(
        dir.path().join("codesonar.conf"),
        format!("HUB_ADDRESS = {}\n", hub_address(hub)),
    )
    .unwrap();

    let options = json!({
        "hub": {"username": "alice", "password": "secret", "project": "test"},
        "pathMappings": [{"from": "/build", "to": dir.path()}],
        "stateDir": dir.path().join("state"),
    });
    (dir, options)
}

/// How often the client signed in to the hub with the credentials.
fn sign_ins(runtime: &tokio::runtime::Runtime, hub: &MockServer) -> usize {
    runtime
        .block_on(hub.received_requests())
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/session/create-basic-auth/")
        .count()
}

#[test]
fn asks_before_signing_in_to_the_hub_of_the_workspace() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let (dir, options) = workspace_naming_the_hub(&runtime, &hub);

    let mut client = Client::initialize(dir.path(), options);

    let question = client.expect("window/showMessageRequest", |_| true);
    assert!(
        question["message"]
            .as_str()
            .unwrap()
            .contains(&format!("names the hub {}", hub_address(&hub))),
        "{question}"
    );

    // Dismissed, the hub is used without the credentials.
    client.expect("window/showMessage", |p| {
        p["message"]
            == format!(
                "CodeSonar: Using the hub {} without signing in",
                hub_address(&hub)
            )
    });
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    assert_eq!(sign_ins(&runtime, &hub), 0);

    client.shutdown();
}

#[test]
fn signs_in_to_a_trusted_hub_of_the_workspace() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let (dir, options) = workspace_naming_the_hub(&runtime, &hub);

    let mut client = Client::initialize(dir.path(), options.clone());
    client.answer = Some("Sign in".into());

    client.expect("window/showMessageRequest", |_| true);
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    assert_eq!(sign_ins(&runtime, &hub), 1);

    client.shutdown();

    // The trust is remembered, so the next session does not ask.
    let mut client = Client::initialize(dir.path(), options);
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    assert_eq!(sign_ins(&runtime, &hub), 2);
    assert!(
        !client
            .pending
            .iter()
            .any(|message| message["method"] == "window/showMessageRequest"),
        "asked again"
    );

    client.shutdown();
}

#[test]
fn trusts_the_hub_of_the_settings() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let (dir, mut options) = workspace_naming_the_hub(&runtime, &hub);
    options["hub"]["url"] = json!(hub.uri());

    let mut client = Client::initialize(dir.path(), options);
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    assert_eq!(sign_ins(&runtime, &hub), 1);
    assert!(
        !client
            .pending
            .iter()
            .any(|message| message["method"] == "window/showMessageRequest"),
        "asked"
    );

    client.shutdown();
}

#[test]
fn connects_to_a_hub() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut client = Client::initialize(dir.path(), tracking_options(dir.path()));
    client.expect("codesonar/loaded", |_| true);

    let projects = client.command(
        "codesonar.connect",
        json!({"url": hub.uri(), "project": "test"}),
    );

    assert_eq!(projects[0]["name"], "test");

    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    // The workspace keeps the choice.
    let conf = std::fs::read_to_string(dir.path().join("codesonar.conf")).unwrap();
    assert!(
        conf.contains("# codesonar-lsp: project = /test\n"),
        "{conf}"
    );
    assert!(conf.contains(&format!(
        "# END codesonar-lsp\n\nHUB_ADDRESS = {}\n",
        hub_address(&hub)
    )));

    let response = client.respond(
        "workspace/executeCommand",
        json!({"command": "codesonar.connect", "arguments": [{"url": "not a url"}]}),
    );

    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.contains("invalid hub url"), "{message}");

    client.shutdown();
}

#[test]
fn creates_the_project_configuration() {
    let dir = workspace();
    let install = dir.path().join("install");
    std::fs::create_dir_all(install.join("bin")).unwrap();
    std::fs::write(install.join("bin/codesonar"), "").unwrap();
    std::fs::write(install.join("template.conf"), "# template\n").unwrap();

    let mut options = options(dir.path());
    options["analyze"] = json!({"command": install.join("bin/codesonar"), "projectFile": "cs/app"});

    let mut client = Client::initialize(dir.path(), options);

    let project = client.command("codesonar.project", Value::Null);
    let conf = dir.path().join("cs/app.conf");
    assert_eq!(project["prefix"], json!(dir.path().join("cs/app")));
    assert_eq!(project["conf"], json!(conf));
    assert_eq!(project["confExists"], false);
    assert!(!conf.exists());

    let project = client.command("codesonar.project", json!({"createConf": true}));
    assert_eq!(project["created"], true);
    assert_eq!(project["confExists"], true);
    assert_eq!(std::fs::read_to_string(conf).unwrap(), "# template\n");

    client.shutdown();
}

#[test]
fn reports_the_hub_and_presets_in_the_status() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let mut options = hub_options(dir.path(), &hub);
    options["analyze"] = json!({"presets": ["cert"]});

    let mut client = Client::initialize(dir.path(), options);
    let loaded = client.expect("codesonar/loaded", |_| true);
    assert_eq!(loaded["hub"], hub.uri());
    assert_eq!(loaded["project"], "test");
    assert_eq!(loaded["presets"], json!(["cert"]));
    assert_eq!(loaded["name"], "done");

    client.shutdown();

    let dir = workspace();
    let mut client = Client::initialize(dir.path(), json!({"sarifFile": "missing.sarif"}));
    client.expect("window/showMessage", |_| true);

    assert_eq!(
        client.command("codesonar.status", Value::Null),
        json!({"loaded": false, "hidden": false, "conf": null, "hub": null, "project": null, "presets": []})
    );

    client.shutdown();
}

/// A workspace whose `codesonar.conf` names the hub of the mock, its project
/// `test`, the preset `security` and the build command `ninja`, with a
/// CodeSonar installation. The editor settings know nothing about the hub.
fn configured_workspace(hub: &MockServer) -> (tempfile::TempDir, std::path::PathBuf, Value) {
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let install = dir.path().join("install");
    std::fs::create_dir_all(install.join("bin")).unwrap();
    std::fs::create_dir_all(install.join("presets")).unwrap();
    std::fs::write(install.join("bin/codesonar"), "").unwrap();
    std::fs::write(
        install.join("template.conf"),
        include_str!("fixtures/template_checks.conf"),
    )
    .unwrap();
    std::fs::write(install.join("presets/security.conf"), "# Security\n").unwrap();

    let conf = dir.path().join("codesonar.conf");
    std::fs::write(
        &conf,
        format!(
            "# BEGIN codesonar-lsp\n# codesonar-lsp: project = /test\n\
             # codesonar-lsp: presets = security\n# codesonar-lsp: build = ninja\n\
             # END codesonar-lsp\nHUB_ADDRESS = {}\n",
            hub_address(hub)
        ),
    )
    .unwrap();

    let mut options = hub_options(dir.path(), hub);
    options.as_object_mut().unwrap().remove("hub");
    options["analyze"] = json!({"command": install.join("bin/codesonar")});

    (dir, conf, options)
}

fn hub_address(hub: &MockServer) -> String {
    hub.uri().trim_start_matches("http://").to_string()
}

#[test]
fn starts_the_workspace_configuration() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let dir = workspace_with_source("2100-01-01T00:00:00Z");
    let conf = dir.path().join("codesonar.conf");
    std::fs::write(&conf, "INCREMENTAL_BUILD = Yes\n").unwrap();

    let mut client = Client::initialize(dir.path(), hub_options(dir.path(), &hub));
    client.expect("codesonar/loaded", |_| true);

    let project = client.command("codesonar.project", Value::Null);
    assert_eq!(project["workspaceConf"], json!(conf));
    assert_eq!(project["workspaceConfExists"], true);
    assert_eq!(project["section"], false);

    let folder = dir.path().file_name().unwrap().to_str().unwrap();
    assert_eq!(
        project["suggested"],
        json!({"project": folder, "build": "make -B", "incremental": true,
            "incrementalBuild": "make", "hubAddress": null})
    );

    let project = client.command(
        "codesonar.initConf",
        json!({"project": "test", "build": "make -B -j4", "incrementalBuild": "make -j4",
            "hubAddress": hub_address(&hub)}),
    );

    assert_eq!(project["section"], true);

    let text = std::fs::read_to_string(&conf).unwrap();
    assert!(
        text.contains("# codesonar-lsp: project = /test\n"),
        "{text}"
    );
    assert!(text.contains("# codesonar-lsp: build = make -B -j4 "));
    assert!(text.contains("# full rebuild, for the first analysis\n"));
    assert!(text.contains("# codesonar-lsp: incremental-build = make -j4 "));
    assert!(text.contains(&format!(
        "# END codesonar-lsp\n\nHUB_ADDRESS = {}\n",
        hub_address(&hub)
    )));
    assert!(text.ends_with("\nINCREMENTAL_BUILD = Yes\n"), "in place");

    // Without incremental analyses, every analysis is a full one.
    client.command(
        "codesonar.initConf",
        json!({"project": "test", "build": "make -B", "incremental": false,
            "incrementalBuild": "make"}),
    );

    let text = std::fs::read_to_string(&conf).unwrap();
    assert!(!text.contains("incremental-build"), "{text}");
    assert!(text.ends_with("\nINCREMENTAL_BUILD = No\n"));

    client.shutdown();
}

#[test]
fn reads_the_workspace_configuration() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let (dir, conf, options) = configured_workspace(&hub);
    let mut client = Client::initialize(dir.path(), options);
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    let status = client.command("codesonar.status", Value::Null);
    assert_eq!(status["hub"], hub.uri());
    assert_eq!(status["project"], "test");
    assert_eq!(status["presets"], json!(["security"]));
    assert_eq!(status["conf"], json!(conf));

    let command = client.command("codesonar.analyzeCommand", Value::Null)["command"].clone();
    let command: Vec<&str> = command
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(
        command.windows(2).any(|w| w == ["-project", "/test"]),
        "{command:?}"
    );
    assert!(command.windows(2).any(|w| w == ["-preset", "security"]));
    assert!(
        command
            .windows(2)
            .any(|w| w[0] == "-conf-file" && w[1] == conf.to_str().unwrap())
    );
    assert_eq!(command.last(), Some(&"ninja"));

    client.shutdown();
}

#[test]
fn configures_the_analysis_in_the_workspace_configuration() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = mock_hub(&runtime, "2100-01-01T00:00:00Z");
    let (dir, conf, options) = configured_workspace(&hub);
    let mut client = Client::initialize(dir.path(), options);
    client.expect("codesonar/loaded", |p| p["hubId"] == "3");

    let options = client.command("codesonar.analysisOptions", Value::Null);
    assert_eq!(
        options["presets"],
        json!([{"name": "security", "description": "Security", "selected": true}])
    );
    assert_eq!(options["checks"].as_array().unwrap().len(), 3);
    assert_eq!(
        options["checks"][2],
        json!({"class": "Function Too Long", "language": "C and C++", "default": false, "enabled": false})
    );

    let options = client.command(
        "codesonar.configureAnalysis",
        json!({"presets": [], "checks": [
            {"class": "Function Too Long", "enabled": true},
            {"class": "Buffer Overrun", "enabled": true},
        ]}),
    );

    assert_eq!(options["checks"][2]["enabled"], true);

    let text = std::fs::read_to_string(&conf).unwrap();
    assert!(text.contains("# codesonar-lsp: presets = \n"), "{text}");
    assert!(text.contains("WARNING_FILTER += allow class=\"Function Too Long\"\n"));
    assert!(
        !text.contains("Buffer Overrun"),
        "the default needs no rule"
    );
    assert!(
        text.contains(&format!("HUB_ADDRESS = {}\n", hub_address(&hub))),
        "kept"
    );

    assert_eq!(
        client.command("codesonar.status", Value::Null)["presets"],
        json!([])
    );

    client.shutdown();
}

#[test]
fn lists_presets() {
    let dir = workspace();
    let install = dir.path().join("install");
    std::fs::create_dir_all(install.join("bin")).unwrap();
    std::fs::create_dir_all(install.join("presets")).unwrap();
    std::fs::write(install.join("bin/codesonar"), "").unwrap();
    std::fs::write(install.join("template.conf"), "").unwrap();
    std::fs::write(install.join("presets/cert.conf"), "# CERT rules\n").unwrap();

    let mut options = options(dir.path());
    options["analyze"] = json!({"command": install.join("bin/codesonar"), "presets": ["cert"]});

    let mut client = Client::initialize(dir.path(), options);

    let presets = client.command("codesonar.presets", Value::Null);
    assert_eq!(
        presets,
        json!([{"name": "cert", "description": "CERT rules", "selected": true}])
    );

    client.shutdown();
}

#[test]
fn returns_analyze_command() {
    let dir = workspace();
    let mut options = options(dir.path());
    options["hub"] = json!({"url": "http://hub:7340", "project": "test"});

    let mut client = Client::initialize(dir.path(), options);

    let command = client.command("codesonar.analyzeCommand", json!({"name": "from vim"}));
    let line: Vec<&str> = command["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part.as_str().unwrap())
        .collect();
    assert_eq!(line[..2], ["codesonar", "analyze"]);
    assert!(line.windows(2).any(|w| w == ["-name", "from vim"]));
    assert_eq!(command["cwd"], json!(dir.path()));

    let response = client.respond(
        "workspace/executeCommand",
        json!({"command": "codesonar.load", "arguments": [{"analysis": [1]}]}),
    );

    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid analysis")
    );

    client.shutdown();
}
