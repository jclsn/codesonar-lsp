//! Client for the CodeSonar hub web API.

use anyhow::{Context, bail};
use reqwest::header::ACCEPT;
use reqwest::{Certificate, Client, RequestBuilder, Response, Url};
use serde::Deserialize;

use crate::config::HubConfig;
use crate::sarif::Log;

pub struct Hub {
    client: Client,
    base: Url,
    config: HubConfig,
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
        })
    }

    /// Signs in if credentials are configured and downloads the SARIF export.
    pub async fn fetch_sarif(&self) -> anyhow::Result<Log> {
        let token = match &self.config.username {
            Some(username) => {
                let password = self.config.password().unwrap_or_default();
                Some(self.sign_in(username, &password).await?)
            }
            None => None,
        };

        let url = self.url(&self.config.sarif_path())?;
        let mut request = self.client.get(url.clone());
        if let Some(token) = &token {
            request = request.bearer_auth(token);
        }
        let text = send(request)
            .await
            .with_context(|| format!("cannot download {url}"))?
            .text()
            .await?;
        Log::from_json(&text).with_context(|| format!("invalid SARIF from {url}"))
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

/// Sends a request and turns error responses into errors with the hub's message.
async fn send(request: RequestBuilder) -> anyhow::Result<Response> {
    let response = request.send().await?;
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
            .fetch_sarif()
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
        let log = Hub::new(&config).unwrap().fetch_sarif().await.unwrap();
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
        let error = Hub::new(&config).unwrap().fetch_sarif().await.unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("hub sign in failed"), "{error}");
        assert!(error.contains("Invalid user name or password"), "{error}");
    }

    #[tokio::test]
    async fn reports_http_and_parse_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/analysis/7-allwarnings.sarif"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/analysis/8-allwarnings.sarif"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;

        let error = Hub::new(&config(&server, ""))
            .unwrap()
            .fetch_sarif()
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("404"), "{error}");

        let config = HubConfig {
            analysis_id: "8".into(),
            ..config(&server, "")
        };
        let error = Hub::new(&config).unwrap().fetch_sarif().await.unwrap_err();
        assert!(format!("{error:#}").contains("invalid SARIF"), "{error:#}");
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
