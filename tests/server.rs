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
    _state: tempfile::TempDir,
}

impl Client {
    fn start() -> Self {
        // Keeps the memory of the tests away from the user's.
        let state = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesonar-lsp"))
            .env("XDG_STATE_HOME", state.path())
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
                self.send(json!({"jsonrpc": "2.0", "id": id, "result": {"success": true}}));
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
    assert_eq!(actions.len(), 3);
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
        json!({"loaded": false, "hub": null, "project": null, "presets": []})
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
