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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tower_lsp::lsp_types::Position;

    use super::*;
    use crate::config::{Config, PathMapping};
    use crate::sarif::Log;
    use crate::warning;

    fn store() -> Store {
        let log = Log::from_json(include_str!("../tests/fixtures/codesonar.sarif")).unwrap();
        let config = Config {
            path_mappings: vec![PathMapping {
                from: "/build".into(),
                to: "/ws".into(),
            }],
            ..Config::default()
        };
        Store::new(warning::from_log(&log, &config, Some(Path::new("/ws"))))
    }

    fn actions(path: &str, line: u32) -> Vec<CodeAction> {
        let uri = Url::from_file_path(path).unwrap();
        let position = Position::new(line, 4);
        code_actions(&store(), &uri, Range::new(position, position))
            .into_iter()
            .map(|action| match action {
                CodeActionOrCommand::CodeAction(action) => action,
                other => panic!("unexpected command: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn offers_quick_fix() {
        let actions = actions("/ws/src/main.c", 4);
        assert_eq!(actions.len(), 2);

        let fix = &actions[0];
        assert_eq!(fix.title, "Check 'p' for NULL");
        assert_eq!(fix.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(fix.is_preferred, Some(true));
        assert_eq!(
            fix.diagnostics.as_ref().unwrap()[0].message,
            "Null pointer dereference of 'p'"
        );
        let changes = fix.edit.as_ref().unwrap().changes.as_ref().unwrap();
        let edits = &changes[&Url::from_file_path("/ws/src/main.c").unwrap()];
        assert_eq!(edits[0].new_text, "if (p) ");
    }

    #[test]
    fn offers_open_in_hub() {
        let actions = actions("/ws/src/main.c", 4);
        let open = &actions[1];
        assert_eq!(open.title, "Open LANG.MEM.NPD in CodeSonar Hub");
        assert!(open.edit.is_none());
        let command = open.command.as_ref().unwrap();
        assert_eq!(command.command, OPEN_IN_HUB_COMMAND);
        assert_eq!(
            command.arguments,
            Some(vec![Value::String(
                "https://hub.example.com:7340/warninginstance/12.html".into()
            )])
        );
    }

    #[test]
    fn offers_nothing_without_fixes_or_hub() {
        assert!(actions("/ws/src/util file.c", 9).is_empty());
        assert!(actions("/ws/src/main.c", 0).is_empty());
    }
}
