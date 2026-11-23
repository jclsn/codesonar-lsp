//! Quick fixes and other code actions for warnings.

use serde_json::{Value, json};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Command, Range, Url, WorkspaceEdit,
};

use crate::hub;
use crate::store::Store;
use crate::triage::Resolution;

/// Reloads the warnings.
pub const REFRESH_COMMAND: &str = "codesonar.refresh";
/// Opens a warning on the hub. Takes the warning URL as argument.
pub const OPEN_IN_HUB_COMMAND: &str = "codesonar.openInHub";
/// Explains warnings, see [`crate::commands::ExplainArgs`].
pub const EXPLAIN_COMMAND: &str = "codesonar.explain";
/// Resolves warnings on the hub, see [`crate::commands::ResolveArgs`].
pub const RESOLVE_COMMAND: &str = "codesonar.resolve";

/// The code actions for the warnings in a range. `on_hub` tells whether
/// the warnings come from the hub, so they can be resolved there.
pub fn code_actions(
    store: &Store,
    uri: &Url,
    range: Range,
    on_hub: bool,
) -> Vec<CodeActionOrCommand> {
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

        let title = format!(
            "Show details for {}",
            warning.rule_id.as_deref().unwrap_or("warning")
        );
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: title.clone(),
            kind: Some(CodeActionKind::EMPTY),
            diagnostics: Some(vec![diagnostic.clone()]),
            command: Some(Command {
                title,
                command: EXPLAIN_COMMAND.to_string(),
                arguments: Some(vec![
                    json!({"uri": warning.uri, "fingerprint": warning.fingerprint}),
                ]),
            }),
            ..CodeAction::default()
        }));

        if let Some(hub_uri) = &warning.hub_uri {
            let title = format!(
                "Open {} in CodeSonar Hub",
                warning.rule_id.as_deref().unwrap_or("warning")
            );
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: title.clone(),
                kind: Some(CodeActionKind::EMPTY),
                diagnostics: Some(vec![diagnostic.clone()]),
                command: Some(Command {
                    title,
                    command: OPEN_IN_HUB_COMMAND.to_string(),
                    arguments: Some(vec![Value::String(hub_uri.to_string())]),
                }),
                ..CodeAction::default()
            }));

            if on_hub && hub::warning_instance_id(hub_uri).is_some() {
                for resolution in Resolution::ALL {
                    let title = resolution.title().to_string();
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: title.clone(),
                        kind: Some(CodeActionKind::EMPTY),
                        diagnostics: Some(vec![diagnostic.clone()]),
                        command: Some(Command {
                            title,
                            command: RESOLVE_COMMAND.to_string(),
                            arguments: Some(vec![json!({
                                "uri": warning.uri,
                                "fingerprint": warning.fingerprint,
                                "resolution": resolution,
                            })]),
                        }),
                        ..CodeAction::default()
                    }));
                }
            }
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
        actions_of(path, line, true)
    }

    fn actions_of(path: &str, line: u32, on_hub: bool) -> Vec<CodeAction> {
        let uri = Url::from_file_path(path).unwrap();
        let position = Position::new(line, 4);
        code_actions(&store(), &uri, Range::new(position, position), on_hub)
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
        assert_eq!(actions.len(), 5);

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
        let open = &actions[2];

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
    fn offers_only_explanation_without_fixes_or_hub() {
        let actions = actions("/ws/src/util file.c", 9);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].title, "Show details for MISC.FMT");
        assert!(self::actions("/ws/src/main.c", 0).is_empty());
    }

    #[test]
    fn offers_explanation() {
        let actions = actions("/ws/src/main.c", 4);
        let explain = &actions[1];

        assert_eq!(explain.title, "Show details for LANG.MEM.NPD");
        assert_eq!(explain.kind, Some(CodeActionKind::EMPTY));

        let command = explain.command.as_ref().unwrap();
        assert_eq!(command.command, EXPLAIN_COMMAND);

        let argument = &command.arguments.as_ref().unwrap()[0];
        assert_eq!(argument["uri"], "file:///ws/src/main.c");
        assert!(argument["fingerprint"].is_string());
    }

    #[test]
    fn offers_resolving_on_the_hub() {
        let actions = actions("/ws/src/main.c", 4);

        let titles: Vec<&str> = actions[3..].iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Mark as fixed", "Mark as false positive",]);

        let command = actions[4].command.as_ref().unwrap();
        assert_eq!(command.command, RESOLVE_COMMAND);

        let argument = &command.arguments.as_ref().unwrap()[0];
        assert_eq!(argument["uri"], "file:///ws/src/main.c");
        assert_eq!(argument["resolution"], "falsePositive");
        assert!(argument["fingerprint"].is_string());

        assert_eq!(
            actions_of("/ws/src/main.c", 4, false).len(),
            3,
            "not for SARIF files"
        );
    }
}
