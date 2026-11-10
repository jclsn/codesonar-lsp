//! Explanations of warnings, shown on hover and by the explain command.

use serde::Serialize;
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range, Url};

use crate::store::Store;
use crate::tracking::Status;
use crate::warning::Warning;

/// The parts of an explanation, for clients that render it themselves.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Explanation {
    pub title: String,
    pub rule_id: Option<String>,
    pub message: String,
    pub description: Option<String>,
    pub status: Status,
    pub related: Vec<Related>,
    pub help_uri: Option<Url>,
    pub hub_uri: Option<Url>,
    /// The whole explanation as Markdown, as shown on hover.
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Related {
    pub uri: Url,
    /// 1-based line.
    pub line: u32,
    pub message: String,
}

impl Explanation {
    pub fn new(warning: &Warning) -> Self {
        Self {
            // CodeSonar exports often name the warning class by its id.
            title: warning
                .rule_name
                .as_deref()
                .or(warning.rule_id.as_deref())
                .unwrap_or("Warning")
                .to_string(),
            rule_id: warning.rule_id.clone(),
            message: warning.message.clone(),
            description: warning.description.clone(),
            status: warning.status,
            related: warning
                .related
                .iter()
                .map(|related| Related {
                    uri: related.location.uri.clone(),
                    line: related.location.range.start.line + 1,
                    message: related.message.clone(),
                })
                .collect(),
            help_uri: warning.help_uri.clone(),
            hub_uri: warning.hub_uri.clone(),
            markdown: explanation(warning),
        }
    }
}

/// Explanations of the warnings of a file, picked by fingerprint or else
/// by range.
pub fn explain(
    store: &Store,
    uri: &Url,
    fingerprint: Option<&str>,
    range: Option<Range>,
) -> Vec<Explanation> {
    let warnings: Vec<&Warning> = match (fingerprint, range) {
        (Some(fingerprint), _) => store
            .get(uri)
            .iter()
            .filter(|warning| warning.fingerprint == fingerprint)
            .collect(),
        (None, Some(range)) => store.overlapping(uri, range),
        (None, None) => store.get(uri).iter().collect(),
    };
    warnings.into_iter().map(Explanation::new).collect()
}

fn title(warning: &Warning) -> &str {
    warning.rule_name.as_deref().unwrap_or("Warning")
}

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
    let mut title = format!("**CodeSonar: {}**", title(warning));

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

    #[test]
    fn explains_warning_by_fingerprint() {
        let store = store();
        let uri = Url::from_file_path("/ws/src/main.c").unwrap();
        let warning = &store.get(&uri)[0];

        let explanations = explain(&store, &uri, Some(&warning.fingerprint), None);

        assert_eq!(explanations.len(), 1);

        let explanation = &explanations[0];
        assert_eq!(explanation.title, "Null Pointer Dereference");
        assert_eq!(explanation.rule_id.as_deref(), Some("LANG.MEM.NPD"));
        assert_eq!(explanation.message, "Null pointer dereference of 'p'");
        assert_eq!(
            explanation.description.as_deref(),
            Some("Check pointers **before** use.")
        );
        assert_eq!(explanation.status, Status::Unverified);

        assert_eq!(explanation.related.len(), 1);
        assert_eq!(explanation.related[0].line, 3);
        assert_eq!(explanation.related[0].message, "'p' is set to NULL");
        assert_eq!(
            explanation.hub_uri.as_ref().map(Url::as_str),
            Some("https://hub.example.com:7340/warninginstance/12.html")
        );
        assert_eq!(explanation.markdown, super::explanation(warning));
    }

    #[test]
    fn titles_warning_by_rule_id_without_name() {
        let mut warning = store().get(&Url::from_file_path("/ws/src/main.c").unwrap())[0].clone();
        warning.rule_name = None;

        assert_eq!(Explanation::new(&warning).title, "LANG.MEM.NPD");

        warning.rule_id = None;

        assert_eq!(Explanation::new(&warning).title, "Warning");
    }

    #[test]
    fn explains_warnings_in_range() {
        let store = store();
        let uri = Url::from_file_path("/ws/src/main.c").unwrap();

        assert_eq!(
            explain(
                &store,
                &uri,
                None,
                Some(Range::new(Position::new(4, 0), Position::new(5, 0)))
            )
            .len(),
            1
        );

        assert!(
            explain(
                &store,
                &uri,
                None,
                Some(Range::new(Position::new(0, 0), Position::new(1, 0)))
            )
            .is_empty()
        );

        assert!(explain(&store, &uri, Some("unknown"), None).is_empty());
    }
}
