//! Warnings indexed by file.

use std::collections::HashMap;

use tower_lsp::lsp_types::{Position, Range, Url};

use crate::warning::Warning;

#[derive(Debug, Default)]
pub struct Store {
    files: HashMap<Url, Vec<Warning>>,
}

impl Store {
    pub fn new(warnings: Vec<Warning>) -> Self {
        let mut files: HashMap<Url, Vec<Warning>> = HashMap::new();

        for warning in warnings {
            files.entry(warning.uri.clone()).or_default().push(warning);
        }

        Self { files }
    }

    pub fn files(&self) -> impl Iterator<Item = (&Url, &[Warning])> {
        self.files
            .iter()
            .map(|(uri, warnings)| (uri, warnings.as_slice()))
    }

    pub fn get(&self, uri: &Url) -> &[Warning] {
        self.files.get(uri).map_or(&[], Vec::as_slice)
    }

    pub fn len(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Warnings whose range contains `position`.
    pub fn at(&self, uri: &Url, position: Position) -> Vec<&Warning> {
        self.overlapping(uri, Range::new(position, position))
    }

    /// Warnings whose range intersects `range`. Range ends are exclusive,
    /// except for empty warning ranges.
    pub fn overlapping(&self, uri: &Url, range: Range) -> Vec<&Warning> {
        self.get(uri)
            .iter()
            .filter(|warning| {
                let w = warning.range;
                w.start <= range.end && (range.start < w.end || range.start == w.start)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::DiagnosticSeverity;

    use super::*;

    fn warning(path: &str, start: (u32, u32), end: (u32, u32)) -> Warning {
        Warning {
            uri: Url::from_file_path(path).unwrap(),
            range: Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1)),
            severity: DiagnosticSeverity::WARNING,
            message: format!("{path} {start:?}"),
            rule_id: None,
            rule_name: None,
            description: None,
            help_uri: None,
            hub_uri: None,
            related: Vec::new(),
            fixes: Vec::new(),
        }
    }

    fn store() -> Store {
        Store::new(vec![
            warning("/a.c", (1, 0), (2, 0)),
            warning("/a.c", (5, 3), (5, 8)),
            warning("/b.c", (0, 0), (0, 0)),
        ])
    }

    fn uri(path: &str) -> Url {
        Url::from_file_path(path).unwrap()
    }

    #[test]
    fn groups_by_file() {
        let store = store();
        assert_eq!(store.len(), 3);
        assert!(!store.is_empty());
        assert_eq!(store.files().count(), 2);
        assert_eq!(store.get(&uri("/a.c")).len(), 2);
        assert!(store.get(&uri("/c.c")).is_empty());
        assert!(Store::default().is_empty());
    }

    #[test]
    fn finds_warnings_at_position() {
        let store = store();
        let a = uri("/a.c");
        assert_eq!(store.at(&a, Position::new(1, 0)).len(), 1);
        assert_eq!(store.at(&a, Position::new(1, 40)).len(), 1);
        assert!(
            store.at(&a, Position::new(2, 0)).is_empty(),
            "end is exclusive"
        );
        assert_eq!(store.at(&a, Position::new(5, 5)).len(), 1);
        assert!(store.at(&a, Position::new(5, 9)).is_empty());
        assert_eq!(store.at(&uri("/b.c"), Position::new(0, 0)).len(), 1);
    }

    #[test]
    fn finds_overlapping_warnings() {
        let store = store();
        let a = uri("/a.c");

        let range = |a, b, c, d| Range::new(Position::new(a, b), Position::new(c, d));

        assert_eq!(store.overlapping(&a, range(0, 0, 9, 0)).len(), 2);
        assert_eq!(store.overlapping(&a, range(5, 0, 5, 3)).len(), 1);
        assert!(store.overlapping(&a, range(3, 0, 4, 0)).is_empty());
    }
}
