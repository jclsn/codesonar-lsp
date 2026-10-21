//! Client for the CodeSonar hub web API.

use anyhow::{Context, bail};
use reqwest::{Certificate, Client, Url};

use crate::config::HubConfig;
use crate::sarif::Log;

pub struct Hub {
    client: Client,
    base: Url,
    config: HubConfig,
}

impl Hub {
    pub fn new(config: &HubConfig) -> anyhow::Result<Self> {
        let mut base = Url::parse(&config.url).context("invalid hub url")?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }

        let mut builder = Client::builder()
            .cookie_store(true)
            .danger_accept_invalid_certs(config.accept_invalid_certs);
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
        if let Some(username) = &self.config.username {
            self.sign_in(username, &self.config.password().unwrap_or_default())
                .await?;
        }

        let url = self.url(&self.config.sarif_path())?;
        let response = self.client.get(url.clone()).send().await?;
        if !response.status().is_success() {
            bail!("hub returned {} for {url}", response.status());
        }
        let text = response.text().await?;
        Log::from_json(&text).with_context(|| format!("invalid SARIF from {url}"))
    }

    async fn sign_in(&self, username: &str, password: &str) -> anyhow::Result<()> {
        let url = self.url("/sign_in.html?response_try_plaintext=1")?;
        let response = self
            .client
            .post(url)
            .form(&[
                ("sif_username", username),
                ("sif_password", password),
                ("sif_sign_in", "yes"),
                ("sif_log_out_competitor", "yes"),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("hub sign in failed with {}", response.status());
        }
        Ok(())
    }

    fn url(&self, path: &str) -> anyhow::Result<Url> {
        Ok(self.base.join(path.trim_start_matches('/'))?)
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
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
    async fn signs_in_and_keeps_session() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cs/sign_in.html"))
            .and(query_param("response_try_plaintext", "1"))
            .and(body_string_contains("sif_username=alice"))
            .and(body_string_contains("sif_password=secret"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("set-cookie", "sessionid=abc; Path=/"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/cs/analysis/7-allwarnings.sarif"))
            .and(header("cookie", "sessionid=abc"))
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
            .and(path("/sign_in.html"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let config = HubConfig {
            username: Some("alice".into()),
            password: Some("wrong".into()),
            ..config(&server, "")
        };
        let error = Hub::new(&config).unwrap().fetch_sarif().await.unwrap_err();
        assert!(error.to_string().contains("sign in failed"), "{error}");
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
        assert!(error.to_string().contains("404"), "{error}");

        let config = HubConfig {
            analysis_id: "8".into(),
            ..config(&server, "")
        };
        let error = Hub::new(&config).unwrap().fetch_sarif().await.unwrap_err();
        assert!(error.to_string().contains("invalid SARIF"), "{error}");
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
