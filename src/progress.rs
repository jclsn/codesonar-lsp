//! Work done progress, which clients show e.g. in their status line.

use std::sync::atomic::{AtomicU64, Ordering};

use tower_lsp::Client;
use tower_lsp::lsp_types::notification::Progress as ProgressNotification;
use tower_lsp::lsp_types::request::WorkDoneProgressCreate;
use tower_lsp::lsp_types::{
    NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
    WorkDoneProgressCreateParams, WorkDoneProgressEnd, WorkDoneProgressReport,
};

use crate::hub::Downloaded;

pub const TITLE: &str = "CodeSonar";

/// A running operation shown by the client.
pub struct Progress {
    client: Client,
    token: NumberOrString,
}

impl Progress {
    /// Shows the start of an operation. `None` if the client refuses it.
    pub async fn begin(client: &Client, message: &str) -> Option<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let token = NumberOrString::String(format!("codesonar/{id}"));
        client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
            .ok()?;

        let progress = Self {
            client: client.clone(),
            token,
        };
        progress
            .send(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: TITLE.to_string(),
                message: Some(message.to_string()),
                ..WorkDoneProgressBegin::default()
            }))
            .await;
        Some(progress)
    }

    pub async fn report(&self, message: String, percentage: Option<u32>) {
        self.send(WorkDoneProgress::Report(WorkDoneProgressReport {
            message: Some(message),
            percentage,
            ..WorkDoneProgressReport::default()
        }))
        .await;
    }

    pub async fn end(self) {
        self.send(WorkDoneProgress::End(WorkDoneProgressEnd::default()))
            .await;
    }

    async fn send(&self, value: WorkDoneProgress) {
        self.client
            .send_notification::<ProgressNotification>(ProgressParams {
                token: self.token.clone(),
                value: ProgressParamsValue::WorkDone(value),
            })
            .await;
    }
}

/// Describes a download, e.g. `Downloading 4.2 MB`, and its percentage if
/// the size is known.
pub fn download(downloaded: Downloaded) -> (String, Option<u32>) {
    let received = megabytes(downloaded.received);

    match downloaded.total.filter(|total| *total > 0) {
        Some(total) => (
            format!("Downloading {received} of {}", megabytes(total)),
            Some((downloaded.received.min(total) * 100 / total) as u32),
        ),
        None => (format!("Downloading {received}"), None),
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}
