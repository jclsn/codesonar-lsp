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
