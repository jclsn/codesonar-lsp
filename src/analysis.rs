//! The loaded analysis, with its warnings matched to the current code.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tower_lsp::lsp_types::Url;

use crate::config::Changed;
use crate::tracking::{Anchor, Lines, Placement, Status};
use crate::warning::{QuickFix, Warning};

#[derive(Debug)]
struct Tracked {
    /// The warning as analyzed.
    warning: Warning,
    anchor: Option<Anchor>,
    placement: Placement,
}

impl Tracked {
    fn new(warning: Warning) -> Self {
        let placement = Placement {
            status: Status::Unverified,
            range: warning.range,
            delta: 0,
            shift: 0,
        };
        Self {
            warning,
            anchor: None,
            placement,
        }
    }

    /// The warning in the current code. Fixes whose code moved are moved
    /// along; fixes whose code changed are dropped.
    fn current(&self) -> Warning {
        let placement = &self.placement;
        let uri = &self.warning.uri;
        let fixes = self
            .warning
            .fixes
            .iter()
            .filter_map(|fix| {
                let mut changes = fix.changes.clone();

                if let Some(edits) = changes.get_mut(uri) {
                    *edits = placement.edits(edits)?;
                }

                Some(QuickFix {
                    title: fix.title.clone(),
                    changes,
                })
            })
            .collect();

        Warning {
            range: placement.range,
            status: placement.status,
            fixes,
            ..self.warning.clone()
        }
    }
}

/// How many warnings have each status.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub unchanged: usize,
    pub moved: usize,
    pub changed: usize,
    pub unverified: usize,
}

#[derive(Debug)]
pub struct Analysis {
    /// Identifies the analysis in the memory.
    pub key: String,
    pub name: String,
    /// Hub analysis id, or `None` for a local file.
    pub hub_id: Option<String>,
    /// When the analysis ran.
    pub time: Option<SystemTime>,
    files: HashMap<Url, Vec<Tracked>>,
}

impl Analysis {
    pub fn new(
        key: String,
        name: String,
        hub_id: Option<String>,
        time: Option<SystemTime>,
        warnings: Vec<Warning>,
    ) -> Self {
        let mut files: HashMap<Url, Vec<Tracked>> = HashMap::new();

        for warning in warnings {
            files
                .entry(warning.uri.clone())
                .or_default()
                .push(Tracked::new(warning));
        }

        Self {
            key,
            name,
            hub_id,
            time,
            files,
        }
    }

    pub fn contains(&self, uri: &Url) -> bool {
        self.files.contains_key(uri)
    }

    pub fn uris(&self) -> impl Iterator<Item = &Url> {
        self.files.keys()
    }

    /// Files with warnings whose analyzed code is unknown.
    pub fn missing_anchors(&self) -> Vec<Url> {
        self.files
            .iter()
            .filter(|(_, tracked)| tracked.iter().any(|t| t.anchor.is_none()))
            .map(|(uri, _)| uri.clone())
            .collect()
    }

    /// Gives warnings their remembered anchors, by fingerprint.
    pub fn set_anchors(&mut self, anchors: &HashMap<String, Anchor>) {
        for tracked in self.files.values_mut().flatten() {
            if let Some(anchor) = anchors.get(&tracked.warning.fingerprint) {
                tracked.anchor = Some(anchor.clone());
            }
        }
    }

    /// Gives the warnings of a file without anchors the anchors in its
    /// analyzed text and returns them by fingerprint.
    pub fn learn(&mut self, uri: &Url, analyzed: &str) -> HashMap<String, Anchor> {
        let lines = Lines::new(analyzed);
        let mut learned = HashMap::new();

        for tracked in self.files.get_mut(uri).into_iter().flatten() {
            if tracked.anchor.is_some() {
                continue;
            }

            if let Some(anchor) = lines.anchor(tracked.warning.range.start.line) {
                learned.insert(tracked.warning.fingerprint.clone(), anchor.clone());
                tracked.anchor = Some(anchor);
            }
        }

        learned
    }

    /// Matches the warnings of a file to its current text.
    pub fn relocate(&mut self, uri: &Url, text: &str) {
        let lines = Lines::new(text);

        for tracked in self.files.get_mut(uri).into_iter().flatten() {
            let last = tracked.placement.range.start.line;
            tracked.placement = lines.place(tracked.warning.range, tracked.anchor.as_ref(), last);
        }
    }

    /// The shown warnings of a file in the current code.
    pub fn warnings(&self, uri: &Url, changed: Changed) -> Vec<Warning> {
        self.files
            .get(uri)
            .into_iter()
            .flatten()
            .filter(|t| changed == Changed::Mark || t.placement.status != Status::Changed)
            .map(Tracked::current)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();

        for tracked in self.files.values().flatten() {
            match tracked.placement.status {
                Status::Unchanged => counts.unchanged += 1,
                Status::Moved => counts.moved += 1,
                Status::Changed => counts.changed += 1,
                Status::Unverified => counts.unverified += 1,
            }
        }

        counts
    }
}

/// Parses an RFC 3339 timestamp like `2026-07-24T19:11:35.654875Z`.
pub fn parse_time(text: &str) -> Option<SystemTime> {
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);

    let offset_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (time, offset) = rest.split_at(offset_at);
    let mut time = time.splitn(3, ':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: f64 = time.next().unwrap_or("0").parse().ok()?;

    let offset = match offset.chars().next() {
        Some(sign @ ('+' | '-')) => {
            let (h, m) = offset[1..].split_once(':').unwrap_or((&offset[1..], "0"));
            let minutes = h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?;
            if sign == '+' {
                minutes * 60
            } else {
                -minutes * 60
            }
        }
        _ => 0,
    };

    // Days since 1970-01-01, after Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let seconds = (days * 86_400 + hour * 3600 + minute * 60 - offset) as f64 + second;
    (seconds >= 0.0).then(|| UNIX_EPOCH + Duration::from_secs_f64(seconds))
}
