//! End-to-end tests that talk to the server binary over stdio.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};
use tower_lsp::lsp_types::Url;

const FIXTURE: &str = include_str!("fixtures/codesonar.sarif");
const TIMEOUT: Duration = Duration::from_secs(10);

struct Client {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    /// Notifications and requests received while waiting for something else.
    pending: Vec<Value>,
    next_id: i64,
}

impl Client {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codesonar-lsp"))
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
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});

        if !params.is_null() {
            request["params"] = params;
        }

        self.send(request);
        let response =
            self.wait_for(|message| message["id"] == id && message.get("method").is_none());
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
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
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["kind"], "quickfix");
    assert_eq!(actions[0]["title"], "Check 'p' for NULL");
    let edits = &actions[0]["edit"]["changes"][main.as_str().unwrap()];
    assert_eq!(edits[0]["newText"], "if (p) ");

    let command = actions[1]["command"].clone();
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
