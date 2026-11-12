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

/// How much of a download has arrived.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Downloaded {
    pub received: u64,
    /// The size, if the hub tells it.
    pub total: Option<u64>,
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

    /// Downloads the SARIF export of an analysis and tells `progress` how
    /// much has arrived. The hub creates the export while sending it, which
    /// takes long for big analyses.
    pub async fn sarif(
        &self,
        analysis_id: &str,
        progress: impl Fn(Downloaded),
    ) -> anyhow::Result<Log> {
        let text = self.sarif_text(analysis_id, progress).await?;
        Log::from_json(&text)
            .with_context(|| format!("invalid SARIF from {}", self.config.sarif_path(analysis_id)))
    }

    /// Downloads the SARIF export of an analysis as it is, see
    /// [`Hub::sarif`].
    pub async fn sarif_text(
        &self,
        analysis_id: &str,
        progress: impl Fn(Downloaded),
    ) -> anyhow::Result<String> {
        let path = self.config.sarif_path(analysis_id);
        let mut response = self.response(&path).await?;
        let mut downloaded = Downloaded {
            received: 0,
            total: response.content_length(),
        };
        let mut body = Vec::new();
        progress(downloaded);

        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("cannot download {path}"))?
        {
            body.extend_from_slice(&chunk);
            downloaded.received = body.len() as u64;
            progress(downloaded);
        }

        String::from_utf8(body).with_context(|| format!("invalid SARIF from {path}"))
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

    /// The project of an analysis.
    pub async fn analysis_project(&self, analysis_id: &str) -> anyhow::Result<Project> {
        let value = self.json(&format!("/analysis/{analysis_id}.json")).await?;
        let project = value.get("project").context("analysis without project")?;
        Ok(Project {
            id: project
                .get("id")
                .and_then(Value::as_u64)
                .context("project without id")?,
            name: text(project, "name"),
        })
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

    /// Deletes an analysis and its warnings from the hub. Refusals, e.g.
    /// for missing permissions, are no error.
    pub async fn delete_analysis(&self, analysis_id: u64) -> anyhow::Result<Deletion> {
        // The hub only deletes analyses with the form of their project page,
        // the analysis page ignores it.
        let project = self.analysis_project(&analysis_id.to_string()).await?;
        let path = format!("/project/{}.json", project.id);
        let id = analysis_id.to_string();
        let result = self
            .send(&path, "delete analysis at", |url| {
                self.client
                    .post(url)
                    .form(&[("remove_analysis_id", id.as_str())])
            })
            .await;
        let user = match &self.config.username {
            Some(username) => format!("the hub user {username}"),
            None => "anonymous hub users".to_string(),
        };
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                return match error.downcast_ref::<StatusError>() {
                    Some(status)
                        if matches!(
                            status.status,
                            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                        ) =>
                    {
                        Ok(Deletion::Refused(format!(
                            "{user} may not delete analysis {analysis_id}{}",
                            reason(&status.message)
                        )))
                    }
                    _ => Err(error),
                };
            }
        };
        let body = response.text().await.unwrap_or_default();
        let value: Value = serde_json::from_str(&body).unwrap_or_default();
        let failure = value
            .pointer("/change/results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|result| text(result, "result") != "success");

        Ok(match failure {
            Some(failure) => {
                let details = text(failure, "details");
                let message = [text(failure, "message"), details]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(": ");
                Deletion::Refused(format!(
                    "the hub did not delete analysis {analysis_id}{}",
                    reason(&message)
                ))
            }
            None => self.deletion_of(analysis_id).await?,
        })
    }

    /// Whether the analysis is gone after the hub accepted its deletion,
    /// since the hub may also accept requests it ignores.
    async fn deletion_of(&self, analysis_id: u64) -> anyhow::Result<Deletion> {
        let analysis = match self.json(&format!("/analysis/{analysis_id}.json")).await {
            Ok(analysis) => analysis,
            Err(error) => {
                return match error.downcast_ref::<StatusError>() {
                    Some(status) if status.status == StatusCode::NOT_FOUND => Ok(Deletion::Deleted),
                    _ => Err(error),
                };
            }
        };

        // Large analyses may take a while.
        Ok(match text(&analysis, "state").contains("DELET") {
            true => Deletion::Deleted,
            false => Deletion::Refused(format!("the hub kept analysis {analysis_id}")),
        })
    }

    /// Requests `path` and returns the response body.
    pub async fn get(&self, path: &str) -> anyhow::Result<String> {
        self.response(path)
            .await?
            .text()
            .await
            .with_context(|| format!("cannot download {path}"))
    }

    async fn response(&self, path: &str) -> anyhow::Result<Response> {
        self.send(path, "download", |url| self.client.get(url))
            .await
    }

    /// Signs in once with the configured credentials, then sends the
    /// request that `request` builds for the URL of `path`. An expired
    /// session is renewed once. `action` describes the request in errors.
    async fn send(
        &self,
        path: &str,
        action: &str,
        request: impl Fn(Url) -> RequestBuilder,
    ) -> anyhow::Result<Response> {
        let url = self.url(path)?;
        let mut renewed = false;

        loop {
            let token = self.token(renewed).await?;
            let mut request = request(url.clone());

            if let Some(token) = &token {
                request = request.bearer_auth(token);
            }

            let response = request
                .send()
                .await
                .with_context(|| format!("cannot {action} {url}"))?;
            let status = response.status();

            if token.is_some()
                && !renewed
                && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            {
                renewed = true;
                continue;
            }

            return check(response)
                .await
                .with_context(|| format!("cannot {action} {url}"));
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

/// The outcome of deleting an analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deletion {
    Deleted,
    /// The hub refused, with a message for the user.
    Refused(String),
}

/// The hub's reason in parentheses, if it gave one.
fn reason(message: &str) -> String {
    match message.trim() {
        "" => String::new(),
        message => format!(" ({message})"),
    }
}

/// An error response of the hub.
#[derive(Debug)]
pub struct StatusError {
    pub status: StatusCode,
    /// The hub's message, may be empty.
    pub message: String,
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.message.is_empty() {
            true => write!(f, "{}", self.status),
            false => write!(f, "{}: {}", self.status, self.message),
        }
    }
}

impl std::error::Error for StatusError {}

/// Turns error responses into a [`StatusError`] with the hub's message.
async fn check(response: Response) -> anyhow::Result<Response> {
    let status = response.status();

    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<HubError>(&body)
        .map(|error| error.error)
        .unwrap_or(body);
    Err(StatusError {
        status,
        message: message.trim().to_string(),
    }
    .into())
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
            .sarif("7", |_| {})
            .await
            .unwrap();
        assert_eq!(log.runs[0].results.len(), 4);
    }

    #[tokio::test]
    async fn reports_the_download_progress() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/analysis/7-allwarnings.sarif"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
            .mount(&server)
            .await;

        let downloads = std::sync::Mutex::new(Vec::new());

        Hub::new(&config(&server, ""))
            .unwrap()
            .sarif("7", |downloaded| downloads.lock().unwrap().push(downloaded))
            .await
            .unwrap();

        let downloads = downloads.into_inner().unwrap();
        let size = Some(FIXTURE.len() as u64);

        assert_eq!(
            downloads.first(),
            Some(&Downloaded {
                received: 0,
                total: size
            })
        );
        assert_eq!(
            downloads.last(),
            Some(&Downloaded {
                received: FIXTURE.len() as u64,
                total: size
            })
        );
    }

    /// Analysis `id` of project 1 on `server`, which is gone after its
    /// project was looked up if `deleted`, and the project page answering
    /// its deletion with `response`.
    async fn deletable(server: &MockServer, id: u64, deleted: bool, response: ResponseTemplate) {
        let analysis = json!({"id": id, "state": "FINISHED", "project": {"id": 1, "name": "p"}});
        let mut found = Mock::given(method("GET"))
            .and(path(format!("/analysis/{id}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(analysis))
            .with_priority(1);

        if deleted {
            found = found.up_to_n_times(1);
        }

        found.mount(server).await;
        Mock::given(method("GET"))
            .and(path(format!("/analysis/{id}.json")))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(
                    json!({"error": format!("Analysis {id} not found"), "status": 404}),
                ),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/project/1.json"))
            .and(body_string_contains(format!("remove_analysis_id={id}")))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn removed(id: u64, result: &str, message: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"change": {"results": [
            {"action": "remove_analysis", "item": {"type": "analysis", "id": id},
             "result": result, "message": message, "details": "kill it first"},
        ]}}))
    }

    #[tokio::test]
    async fn deletes_analyses_with_the_project_form() {
        let server = MockServer::start().await;
        deletable(&server, 7, true, removed(7, "success", "Removed")).await;

        let hub = Hub::new(&config(&server, "")).unwrap();
        assert_eq!(hub.delete_analysis(7).await.unwrap(), Deletion::Deleted);

        let posts = server.received_requests().await.unwrap();
        assert!(
            posts
                .iter()
                .any(|r| r.method.as_str() == "POST" && r.url.path() == "/project/1.json")
        );
    }

    #[tokio::test]
    async fn reports_analyses_that_cannot_be_deleted() {
        let server = MockServer::start().await;

        deletable(
            &server,
            7,
            false,
            removed(7, "failure", "Analysis is running"),
        )
        .await;

        deletable(
            &server,
            8,
            false,
            ResponseTemplate::new(403)
                .set_body_string(r#"{"status":403,"error":"Permission Denied"}"#),
        )
        .await;

        // Accepted but ignored.
        deletable(
            &server,
            9,
            false,
            ResponseTemplate::new(200).set_body_json(json!({"id": 9})),
        )
        .await;

        let hub = Hub::new(&config(&server, "")).unwrap();
        assert_eq!(
            hub.delete_analysis(7).await.unwrap(),
            Deletion::Refused(
                "the hub did not delete analysis 7 (Analysis is running: kill it first)".into()
            )
        );
        assert_eq!(
            hub.delete_analysis(8).await.unwrap(),
            Deletion::Refused(
                "anonymous hub users may not delete analysis 8 (Permission Denied)".into()
            )
        );
        assert_eq!(
            hub.delete_analysis(9).await.unwrap(),
            Deletion::Refused("the hub kept analysis 9".into())
        );

        let error = format!("{:#}", hub.delete_analysis(10).await.unwrap_err());
        assert!(error.contains("404"), "{error}");
    }

    #[tokio::test]
    async fn names_the_user_who_may_not_delete() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/session/create-basic-auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"bearer": "1/t"}"#))
            .mount(&server)
            .await;

        deletable(&server, 7, false, ResponseTemplate::new(403)).await;

        let config = HubConfig {
            username: Some("alice".into()),
            password: Some("secret".into()),
            ..config(&server, "")
        };
        assert_eq!(
            Hub::new(&config).unwrap().delete_analysis(7).await.unwrap(),
            Deletion::Refused("the hub user alice may not delete analysis 7".into())
        );
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
            .sarif(&config.analysis_id, |_| {})
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
            .sarif(&config.analysis_id, |_| {})
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
            .sarif(&config.analysis_id, |_| {})
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
            .sarif("7", |_| {})
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
            .sarif(&config.analysis_id, |_| {})
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
    async fn gets_the_rules_of_a_warning() {
        let server = MockServer::start().await;

        mount_json(
            &server,
            "/warninginstance/305.json",
            json!({"categories": [
                {"link": "ALLOC.DF", "url": "/doc/ALLOC.DF.html", "relationship": null, "rule_text": null},
                {"link": "CWE:415", "url": "https://links.example.com/cwe/415/",
                 "relationship": "intersects", "rule_text": "Double Free"},
                {"link": "", "url": ""},
            ]}),
        )
        .await;

        let hub = Hub::new(&config(&server, "")).unwrap();
        let rules = hub.warning_rules(305).await.unwrap();

        assert_eq!(
            rules,
            [
                Rule {
                    id: "ALLOC.DF".into(),
                    title: None,
                    relationship: None,
                    url: Some(Url::parse(&format!("{}/doc/ALLOC.DF.html", server.uri())).unwrap()),
                },
                Rule {
                    id: "CWE:415".into(),
                    title: Some("Double Free".into()),
                    relationship: Some("intersects".into()),
                    url: Some(Url::parse("https://links.example.com/cwe/415/").unwrap()),
                },
            ]
        );
    }

    #[tokio::test]
    async fn finds_the_project_of_an_analysis() {
        let server = MockServer::start().await;

        mount_json(
            &server,
            "/analysis/8.json",
            json!({"id": 8, "project": {"id": 1, "name": "mixxx", "path": "/mixxx"}}),
        )
        .await;

        mount_json(&server, "/analysis/9.json", json!({"id": 9})).await;

        let hub = Hub::new(&config(&server, "")).unwrap();
        assert_eq!(
            hub.analysis_project("8").await.unwrap(),
            Project {
                id: 1,
                name: "mixxx".into()
            }
        );
        assert!(hub.analysis_project("9").await.is_err());
    }

    #[test]
    fn finds_warning_instance_ids() {
        let id = |url: &str| warning_instance_id(&Url::parse(url).unwrap());
        assert_eq!(id("http://hub:7340/warninginstance/305.html"), Some(305));
        assert_eq!(id("http://hub:7340/analysis/305.html"), None);
        assert_eq!(id("http://hub:7340/warninginstance/x.html"), None);
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
