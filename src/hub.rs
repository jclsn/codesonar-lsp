//! Client for the CodeSonar hub web API.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use reqwest::header::ACCEPT;
use reqwest::{Certificate, Client, RequestBuilder, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::HubConfig;
use crate::sarif::Log;
use crate::warning::Rule;

pub struct Hub {
    client: Client,
    base: Url,
    config: HubConfig,
    /// Bearer token of the current session.
    token: Mutex<Option<String>>,
}

/// Session returned by `/session/create-basic-auth/`.
#[derive(Deserialize)]
struct Session {
    bearer: String,
}

/// Error body returned by the hub, e.g. for wrong credentials.
#[derive(Deserialize)]
struct HubError {
    error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Project {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Analysis {
    pub id: u64,
    pub name: String,
    pub started: String,
    /// E.g. `ANALYZING`, `DAEMON` or `FINISHED`.
    pub state: String,
}

impl Analysis {
    /// Whether the analysis has results to show.
    pub fn is_finished(&self) -> bool {
        matches!(self.state.as_str(), "FINISHED" | "DAEMON")
    }
}

/// A file of an analysis with the URL of its analyzed source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedFile {
    pub file: String,
    pub url: String,
}

impl Hub {
    pub fn new(config: &HubConfig) -> anyhow::Result<Self> {
        let mut base = Url::parse(&config.url).context("invalid hub url")?;

        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }

        let mut builder =
            Client::builder().danger_accept_invalid_certs(config.accept_invalid_certs);

        if let Some(path) = &config.ca_certificate {
            let pem =
                std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
            builder = builder.add_root_certificate(Certificate::from_pem(&pem)?);
        }

        Ok(Self {
            client: builder.build()?,
            base,
            config: config.clone(),
            token: Mutex::default(),
        })
    }

    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    /// Downloads the SARIF export of an analysis.
    pub async fn sarif(&self, analysis_id: &str) -> anyhow::Result<Log> {
        let path = self.config.sarif_path(analysis_id);
        let text = self.get(&path).await?;
        Log::from_json(&text).with_context(|| format!("invalid SARIF from {path}"))
    }

    pub async fn projects(&self) -> anyhow::Result<Vec<Project>> {
        let rows = self.rows("/project_search.json").await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(Project {
                    id: id_from_url(row.get("url")?.as_str()?)?,
                    name: text(row, "project"),
                })
            })
            .collect())
    }

    /// The analyses of a project, given by name or id, newest first.
    pub async fn analyses(&self, project: &str) -> anyhow::Result<Vec<Analysis>> {
        let projects = self.projects().await?;
        let Some(found) = projects
            .iter()
            .find(|p| p.name == project || p.id.to_string() == project)
        else {
            bail!("hub project {project} not found");
        };

        let rows = self.rows(&format!("/project/{}.json", found.id)).await?;
        let mut analyses: Vec<Analysis> = rows
            .iter()
            .filter_map(|row| {
                Some(Analysis {
                    id: id_from_url(row.get("url")?.as_str()?)?,
                    name: text(row, "analysis"),
                    started: text(row, "started"),
                    state: text(row, "state"),
                })
            })
            .collect();
        analyses.sort_by_key(|analysis| std::cmp::Reverse(analysis.id));
        Ok(analyses)
    }

    /// The files of an analysis.
    pub async fn files(&self, analysis_id: &str) -> anyhow::Result<Vec<AnalyzedFile>> {
        let rows = self
            .rows(&format!("/analysis/{analysis_id}-files.json"))
            .await?;
        Ok(rows
            .iter()
            .map(|row| AnalyzedFile {
                file: text(row, "file"),
                url: text(row, "url"),
            })
            .filter(|file| !file.file.is_empty() && !file.url.is_empty())
            .collect())
    }

    /// The analyzed source of a file listing, by its URL.
    pub async fn source(&self, url: &str) -> anyhow::Result<Vec<String>> {
        Ok(source_lines(&self.json(url).await?))
    }

    /// The rules a warning instance relates to, with their titles.
    pub async fn warning_rules(&self, instance_id: u64) -> anyhow::Result<Vec<Rule>> {
        let value = self
            .json(&format!("/warninginstance/{instance_id}.json"))
            .await?;
        let categories = value
            .get("categories")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(categories
            .iter()
            .map(|category| Rule {
                id: text(category, "link"),
                title: Some(text(category, "rule_text")).filter(|t| !t.is_empty()),
                relationship: Some(text(category, "relationship")).filter(|r| !r.is_empty()),
                url: Some(text(category, "url"))
                    .filter(|url| !url.is_empty())
                    .and_then(|url| self.url(&url).ok()),
            })
            .filter(|rule| !rule.id.is_empty())
            .collect())
    }

    /// The page of an analysis.
    pub fn analysis_url(&self, analysis_id: &str) -> anyhow::Result<Url> {
        self.url(&format!("/analysis/{analysis_id}.html"))
    }

    /// Signs in once with the configured credentials, then requests `path`.
    /// An expired session is renewed once.
    pub async fn get(&self, path: &str) -> anyhow::Result<String> {
        let url = self.url(path)?;
        let mut renewed = false;

        loop {
            let token = self.token(renewed).await?;
            let mut request = self.client.get(url.clone());

            if let Some(token) = &token {
                request = request.bearer_auth(token);
            }

            let response = request
                .send()
                .await
                .with_context(|| format!("cannot download {url}"))?;
            let status = response.status();

            if token.is_some()
                && !renewed
                && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            {
                renewed = true;
                continue;
            }

            return Ok(check(response)
                .await
                .with_context(|| format!("cannot download {url}"))?
                .text()
                .await?);
        }
    }

    async fn json(&self, path: &str) -> anyhow::Result<Value> {
        let text = self.get(path).await?;
        serde_json::from_str(&text).with_context(|| format!("invalid JSON from {path}"))
    }

    async fn rows(&self, path: &str) -> anyhow::Result<Vec<Value>> {
        let value = self.json(path).await?;
        Ok(value
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// The bearer token, signing in if there is none yet or `renew` is set.
    async fn token(&self, renew: bool) -> anyhow::Result<Option<String>> {
        let Some(username) = &self.config.username else {
            return Ok(None);
        };

        let mut token = self.token.lock().await;

        if renew || token.is_none() {
            let password = self.config.password().unwrap_or_default();
            *token = Some(self.sign_in(username, &password).await?);
        }

        Ok(token.clone())
    }

    /// Creates a session with HTTP basic auth, like the hub's sign-in page,
    /// and returns its bearer token.
    async fn sign_in(&self, username: &str, password: &str) -> anyhow::Result<String> {
        let request = self
            .client
            .post(self.url("/session/create-basic-auth/")?)
            .basic_auth(username, Some(password))
            .header(ACCEPT, "application/json")
            .form(&[("key", "bearer"), ("pool", "web")]);
        let response = send(request).await.context("hub sign in failed")?;
        let session: Session = response
            .json()
            .await
            .context("hub sign in returned no session")?;
        Ok(session.bearer)
    }

    fn url(&self, path: &str) -> anyhow::Result<Url> {
        Ok(self.base.join(path.trim_start_matches('/'))?)
    }
}

/// The analyzed file of a local path: the one whose mapped path is the
/// local path, else the one sharing the most trailing path components.
pub fn find_file<'a>(
    files: &'a [AnalyzedFile],
    path: &Path,
    map_path: impl Fn(&str) -> String,
) -> Option<&'a AnalyzedFile> {
    let local: Vec<_> = path.components().rev().collect();

    files
        .iter()
        .map(|file| {
            let mapped = PathBuf::from(map_path(&file.file));

            if mapped == path {
                return (usize::MAX, file);
            }

            let shared = mapped
                .components()
                .rev()
                .zip(&local)
                .take_while(|(a, b)| a == *b)
                .count();
            (shared, file)
        })
        .filter(|(shared, _)| *shared > 0)
        .max_by_key(|(shared, _)| *shared)
        .map(|(_, file)| file)
}

/// The id of a warning instance from its URL, like
/// `/warninginstance/305.html`.
pub fn warning_instance_id(url: &Url) -> Option<u64> {
    let mut segments = url.path_segments()?.rev();
    let id = segments.next()?.strip_suffix(".html")?.parse().ok()?;
    (segments.next()? == "warninginstance").then_some(id)
}

/// The id in hub URLs like `/project/12.json`.
fn id_from_url(url: &str) -> Option<u64> {
    let name = url.rsplit('/').next()?;
    name.strip_suffix(".json")?.parse().ok()
}

fn text(row: &Value, key: &str) -> String {
    match row.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(value) => value.to_string(),
    }
}

/// The lines of a hub file listing, a tree of `wdom_line` nodes whose
/// nested children hold the text.
pub fn source_lines(listing: &Value) -> Vec<String> {
    fn flatten(node: &Value, out: &mut String) {
        match node {
            Value::String(text) => out.push_str(text),
            Value::Object(_) => {
                for child in node
                    .get("children")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    flatten(child, out);
                }
            }
            _ => {}
        }
    }

    let mut lines = Vec::new();

    for node in listing
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if node.get("type").and_then(Value::as_str) != Some("wdom_line") {
            continue;
        }

        let number = node
            .get("number")
            .and_then(Value::as_u64)
            .map_or(lines.len() + 1, |n| n as usize);

        while lines.len() + 1 < number {
            lines.push(String::new());
        }

        let mut line = String::new();
        flatten(node, &mut line);
        lines.push(line.trim_end_matches(['\r', '\n']).to_string());
    }

    lines
}

/// Sends a request and turns error responses into errors.
async fn send(request: RequestBuilder) -> anyhow::Result<Response> {
    check(request.send().await?).await
}

/// Turns error responses into errors with the hub's message.
async fn check(response: Response) -> anyhow::Result<Response> {
    let status = response.status();

    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<HubError>(&body)
        .map(|error| error.error)
        .unwrap_or(body);
    let message = message.trim();

    if message.is_empty() {
        bail!("{status}");
    }

    bail!("{status}: {message}");
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use serde_json::json;

    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/codesonar.sarif");

    fn config(server: &MockServer, prefix: &str) -> HubConfig {
        HubConfig {
            url: format!("{}{prefix}", server.uri()),
            analysis_id: "7".into(),
            ..HubConfig::default()
        }
    }

    #[tokio::test]
    async fn fetches_sarif_anonymously() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/analysis/7-allwarnings.sarif"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
            .expect(1)
            .mount(&server)
            .await;

        let log = Hub::new(&config(&server, ""))
            .unwrap()
            .sarif("7")
            .await
            .unwrap();
        assert_eq!(log.runs[0].results.len(), 4);
    }

    #[tokio::test]
    async fn signs_in_with_basic_auth() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/cs/session/create-basic-auth/"))
            // base64 of "alice:secret"
            .and(header("authorization", "Basic YWxpY2U6c2VjcmV0"))
            .and(header("accept", "application/json"))
            .and(body_string_contains("key=bearer"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"bearer": "9/token", "user": {"name": "alice"}}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/cs/analysis/7-allwarnings.sarif"))
            .and(header("authorization", "Bearer 9/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
            .expect(1)
            .mount(&server)
            .await;

        let config = HubConfig {
            username: Some("alice".into()),
            password: Some("secret".into()),
            ..config(&server, "/cs")
        };

        let log = Hub::new(&config)
            .unwrap()
            .sarif(&config.analysis_id)
            .await
            .unwrap();

        assert_eq!(log.runs.len(), 1);
    }

    #[tokio::test]
    async fn reports_failed_sign_in() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/session/create-basic-auth/"))
            .respond_with(ResponseTemplate::new(403).set_body_string(
                r#"{"status":403,"error":"Permission Denied: Invalid user name or password."}"#,
            ))
            .mount(&server)
            .await;

        let config = HubConfig {
            username: Some("alice".into()),
            password: Some("wrong".into()),
            ..config(&server, "")
        };

        let error = Hub::new(&config)
            .unwrap()
            .sarif(&config.analysis_id)
            .await
            .unwrap_err();

        let error = format!("{error:#}");

        assert!(error.contains("hub sign in failed"), "{error}");
        assert!(error.contains("Invalid user name or password"), "{error}");
    }

    #[tokio::test]
    async fn reports_sign_in_without_session() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/session/create-basic-auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let config = HubConfig {
            username: Some("alice".into()),
            ..config(&server, "")
        };

        let error = Hub::new(&config)
            .unwrap()
            .sarif(&config.analysis_id)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("no session"), "{error:#}");
    }

    #[tokio::test]
    async fn reports_http_and_parse_errors() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/analysis/7-allwarnings.sarif"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Analysis 7 not found"))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/analysis/8-allwarnings.sarif"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;

        let error = Hub::new(&config(&server, ""))
            .unwrap()
            .sarif("7")
            .await
            .unwrap_err();

        let error = format!("{error:#}");

        assert!(error.contains("404"), "{error}");
        assert!(error.contains("Analysis 7 not found"), "{error}");

        let config = HubConfig {
            analysis_id: "8".into(),
            ..config(&server, "")
        };

        let error = Hub::new(&config)
            .unwrap()
            .sarif(&config.analysis_id)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("invalid SARIF"), "{error:#}");
    }

    async fn mount_json(server: &MockServer, route: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn lists_analyses_of_a_project() {
        let server = MockServer::start().await;

        mount_json(
            &server,
            "/project_search.json",
            json!({"rows": [
                {"project": "other", "url": "/project/1.json"},
                {"project": "test_project", "url": "/project/2.json"},
            ]}),
        )
        .await;

        mount_json(
            &server,
            "/project/2.json",
            json!({"rows": [
                {"analysis": "first", "started": "Mon", "state": "FINISHED", "url": "/analysis/3.json"},
                {"analysis": "second", "started": "Tue", "state": "ANALYZING", "url": "/analysis/5.json"},
            ]}),
        )
        .await;

        let hub = Hub::new(&config(&server, "")).unwrap();
        let projects = hub.projects().await.unwrap();

        assert_eq!(
            projects[1],
            Project {
                id: 2,
                name: "test_project".into()
            }
        );

        let analyses = hub.analyses("test_project").await.unwrap();
        assert_eq!(analyses.len(), 2);
        assert_eq!(analyses[0].id, 5, "newest first");
        assert!(!analyses[0].is_finished());
        assert_eq!(analyses[1].name, "first");
        assert!(analyses[1].is_finished());
        assert_eq!(hub.analyses("2").await.unwrap().len(), 2, "by id");

        let error = hub.analyses("missing").await.unwrap_err();
        assert!(error.to_string().contains("not found"), "{error}");
    }

    #[tokio::test]
    async fn reads_analyzed_sources() {
        let server = MockServer::start().await;

        mount_json(
            &server,
            "/analysis/3-files.json",
            json!({"rows": [{"file": "/src/main.c", "url": "/analysis/3-file1.json"}]}),
        )
        .await;

        mount_json(
            &server,
            "/analysis/3-file1.json",
            json!({"children": [
                {"type": "wdom_file"},
                {"type": "wdom_line", "number": 1, "children": ["int ", {"children": ["main"]}, "(void)\n"]},
                {"type": "wdom_line", "number": 3, "children": ["}\r\n"]},
            ]}),
        )
        .await;

        let hub = Hub::new(&config(&server, "")).unwrap();
        let files = hub.files("3").await.unwrap();

        assert_eq!(files[0].file, "/src/main.c");

        let lines = hub.source(&files[0].url).await.unwrap();
        assert_eq!(lines, ["int main(void)", "", "}"]);
        assert_eq!(
            hub.analysis_url("3").unwrap().as_str(),
            format!("{}/analysis/3.html", server.uri())
        );
    }

    #[tokio::test]
    async fn keeps_and_renews_the_session() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/session/create-basic-auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"bearer": "old"}"#))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/create-basic-auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"bearer": "new"}"#))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(header("authorization", "Bearer old"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"rows": []}"#))
            .up_to_n_times(2)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(header("authorization", "Bearer old"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(header("authorization", "Bearer new"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"rows": []}"#))
            .expect(1)
            .mount(&server)
            .await;

        let config = HubConfig {
            username: Some("alice".into()),
            password: Some("secret".into()),
            ..config(&server, "")
        };

        let hub = Hub::new(&config).unwrap();

        for _ in 0..3 {
            assert!(hub.projects().await.unwrap().is_empty());
        }
    }

    #[test]
    fn finds_analyzed_files() {
        let file = |name: &str| AnalyzedFile {
            file: name.into(),
            url: format!("/{name}.json"),
        };

        let files = [
            file("/build/app/main.c"),
            file("/build/lib/main.c"),
            file("/build/app/util.c"),
        ];

        let identity = |name: &str| name.to_string();

        let found = find_file(&files, Path::new("/home/me/app/main.c"), identity);
        assert_eq!(
            found.unwrap().file,
            "/build/app/main.c",
            "most shared components"
        );

        let mapped = |name: &str| name.replace("/build/lib", "/src");
        let found = find_file(&files, Path::new("/src/main.c"), mapped);

        assert_eq!(found.unwrap().file, "/build/lib/main.c", "mapped path");

        assert!(find_file(&files, Path::new("/home/me/other.c"), identity).is_none());
    }

    #[test]
    fn rejects_invalid_settings() {
        let config = HubConfig {
            url: "not a url".into(),
            ..HubConfig::default()
        };
        assert!(Hub::new(&config).is_err());

        let config = HubConfig {
            url: "https://hub".into(),
            ca_certificate: Some("/does/not/exist.pem".into()),
            ..HubConfig::default()
        };
        assert!(Hub::new(&config).is_err());
    }
}
