//! Quick fixes and other code actions for warnings.

use serde_json::Value;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Command, Range, Url, WorkspaceEdit,
};

use crate::store::Store;

/// Reloads the warnings.
pub const REFRESH_COMMAND: &str = "codesonar.refresh";
/// Opens a warning on the hub. Takes the warning URL as argument.
pub const OPEN_IN_HUB_COMMAND: &str = "codesonar.openInHub";

pub fn code_actions(store: &Store, uri: &Url, range: Range) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();
    for warning in store.overlapping(uri, range) {
        let diagnostic = warning.to_diagnostic();
        for (index, fix) in warning.fixes.iter().enumerate() {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: fix.title.clone(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(fix.changes.clone()),
                    ..WorkspaceEdit::default()
                }),
                is_preferred: Some(index == 0 && warning.fixes.len() == 1),
                ..CodeAction::default()
            }));
        }
        if let Some(hub_uri) = &warning.hub_uri {
            let title = format!(
                "Open {} in CodeSonar Hub",
                warning.rule_id.as_deref().unwrap_or("warning")
            );
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: title.clone(),
                kind: Some(CodeActionKind::EMPTY),
                diagnostics: Some(vec![diagnostic]),
                command: Some(Command {
                    title,
                    command: OPEN_IN_HUB_COMMAND.to_string(),
                    arguments: Some(vec![Value::String(hub_uri.to_string())]),
                }),
                ..CodeAction::default()
            }));
        }
    }
    actions
}
