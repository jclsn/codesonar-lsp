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
