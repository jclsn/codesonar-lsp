//! Explanations of warnings, shown on hover.

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Url};

use crate::store::Store;
use crate::warning::Warning;

pub fn hover(store: &Store, uri: &Url, position: Position) -> Option<Hover> {
    let warnings = store.at(uri, position);

    if warnings.is_empty() {
        return None;
    }

    let value = warnings
        .iter()
        .map(|warning| explanation(warning))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: (warnings.len() == 1).then(|| warnings[0].range),
    })
}

/// Markdown explanation of a single warning.
pub fn explanation(warning: &Warning) -> String {
    let mut title = format!(
        "**CodeSonar: {}**",
        warning.rule_name.as_deref().unwrap_or("Warning")
    );

    if let Some(id) = &warning.rule_id {
        title.push_str(&format!(" (`{id}`)"));
    }

    let mut parts = vec![title, warning.message.clone()];

    if let Some(description) = &warning.description {
        parts.push(description.clone());
    }

    for related in &warning.related {
        let line = related.location.range.start.line + 1;
        let file = related
            .location
            .uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or_default();
        parts.push(format!("- {file}:{line}: {}", related.message));
    }

    let mut links = Vec::new();

    if let Some(uri) = &warning.help_uri {
        links.push(format!("[Documentation]({uri})"));
    }

    if let Some(uri) = &warning.hub_uri {
        links.push(format!("[Open in CodeSonar Hub]({uri})"));
    }

    if !links.is_empty() {
        parts.push(links.join(" · "));
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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

    fn markdown(hover: Hover) -> String {
        match hover.contents {
            HoverContents::Markup(content) => {
                assert_eq!(content.kind, MarkupKind::Markdown);
                content.value
            }

            other => panic!("unexpected contents: {other:?}"),
        }
    }

    #[test]
    fn explains_warning_with_details() {
        let store = store();
        let uri = Url::from_file_path("/ws/src/main.c").unwrap();
        let hover = hover(&store, &uri, Position::new(4, 5)).unwrap();
        assert_eq!(hover.range, Some(store.get(&uri)[0].range));

        let text = markdown(hover);
        assert!(text.starts_with("**CodeSonar: Null Pointer Dereference** (`LANG.MEM.NPD`)"));
        assert!(text.contains("Null pointer dereference of 'p'"));
        assert!(text.contains("Check pointers **before** use."));
        assert!(text.contains("- main.c:3: 'p' is set to NULL"));
        assert!(text.contains(
            "[Documentation](https://hub.example.com:7340/doc/LANG.MEM.NPD.html) · \
             [Open in CodeSonar Hub](https://hub.example.com:7340/warninginstance/12.html)"
        ));
    }

    #[test]
    fn explains_warning_without_details() {
        let store = store();
        let uri = Url::from_file_path("/ws/src/util file.c").unwrap();
        let text = markdown(hover(&store, &uri, Position::new(9, 0)).unwrap());
        assert_eq!(
            text,
            "**CodeSonar: Format String** (`MISC.FMT`)\n\nFormat string is not a literal"
        );
    }

    #[test]
    fn returns_nothing_outside_warnings() {
        let store = store();
        let uri = Url::from_file_path("/ws/src/main.c").unwrap();
        assert!(hover(&store, &uri, Position::new(0, 0)).is_none());
        let other = Url::from_file_path("/ws/other.c").unwrap();
        assert!(hover(&store, &other, Position::new(4, 5)).is_none());
    }

    #[test]
    fn joins_multiple_warnings() {
        let mut warnings: Vec<_> = store()
            .get(&Url::from_file_path("/ws/src/main.c").unwrap())
            .to_vec();
        warnings.push(warnings[0].clone());
        let store = Store::new(warnings);
        let uri = Url::from_file_path("/ws/src/main.c").unwrap();
        let hover = hover(&store, &uri, Position::new(4, 5)).unwrap();
        assert_eq!(hover.range, None);
        assert_eq!(markdown(hover).matches("\n\n---\n\n").count(), 1);
    }
}
