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
