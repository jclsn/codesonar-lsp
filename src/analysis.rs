//! The loaded analysis, with its warnings matched to the current code.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tower_lsp::lsp_types::Url;

use crate::config::Changed;
use crate::tracking::{Anchor, Lines, Placement, Status};
use crate::triage::Resolution;
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
    /// Resolved on the hub, so not shown.
    pub resolved: usize,
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

    /// The shown warnings of a file in the current code. Resolved ones are
    /// left out.
    pub fn warnings(&self, uri: &Url, changed: Changed) -> Vec<Warning> {
        self.files
            .get(uri)
            .into_iter()
            .flatten()
            .filter(|t| t.warning.resolution.is_none())
            .filter(|t| changed == Changed::Mark || t.placement.status != Status::Changed)
            .map(Tracked::current)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    /// Records that the warning of a file was resolved on the hub.
    pub fn resolve(&mut self, uri: &Url, fingerprint: &str, resolution: Resolution) {
        for tracked in self.files.get_mut(uri).into_iter().flatten() {
            if tracked.warning.fingerprint == fingerprint {
                tracked.warning.resolution = Some(resolution);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();

        for tracked in self.files.values().flatten() {
            if tracked.warning.resolution.is_some() {
                counts.resolved += 1;
                continue;
            }

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

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range, TextEdit};

    use super::*;

    const ANALYZED: &str = "int main(void)\n{\n    char *p = 0;\n    *p = 1;\n    return 0;\n}\n";

    fn uri() -> Url {
        Url::from_file_path("/ws/main.c").unwrap()
    }

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(line, end))
    }

    fn warning(fingerprint: &str, line: u32) -> Warning {
        let fix = QuickFix {
            title: "Check 'p'".into(),
            changes: [(
                uri(),
                vec![TextEdit {
                    range: range(line, 4, 4),
                    new_text: "if (p) ".into(),
                }],
            )]
            .into(),
        };
        Warning {
            uri: uri(),
            range: range(line, 4, 6),
            severity: DiagnosticSeverity::ERROR,
            message: "Null pointer dereference".into(),
            rule_id: None,
            rule_name: None,
            description: None,
            help_uri: None,
            hub_uri: None,
            related: Vec::new(),
            fixes: vec![fix],
            events: Vec::new(),
            fingerprint: fingerprint.into(),
            status: Status::Unverified,
            resolution: None,
        }
    }

    fn analysis() -> Analysis {
        Analysis::new(
            "key".into(),
            "test".into(),
            None,
            None,
            vec![warning("a", 3), warning("b", 2)],
        )
    }

    #[test]
    fn learns_analyzed_code() {
        let mut analysis = analysis();
        assert_eq!(analysis.missing_anchors(), [uri()]);

        let learned = analysis.learn(&uri(), ANALYZED);
        assert_eq!(learned.len(), 2);
        assert_eq!(learned["a"].line, "    *p = 1;");
        assert!(analysis.missing_anchors().is_empty());
        assert!(
            analysis.learn(&uri(), ANALYZED).is_empty(),
            "learned only once"
        );

        let mut other = Analysis::new(
            "key".into(),
            "test".into(),
            None,
            None,
            vec![warning("a", 3)],
        );

        other.set_anchors(&learned);

        assert!(other.missing_anchors().is_empty());
    }

    #[test]
    fn hides_or_marks_changed_warnings() {
        let mut analysis = analysis();

        analysis.learn(&uri(), ANALYZED);
        analysis.relocate(&uri(), &ANALYZED.replace("*p = 1;", "if (p) *p = 1;"));

        let shown = analysis.warnings(&uri(), Changed::Hide);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].fingerprint, "b");

        let marked = analysis.warnings(&uri(), Changed::Mark);
        assert_eq!(marked.len(), 2);
        assert_eq!(marked[0].status, Status::Changed);
        assert!(marked[0].fixes.is_empty(), "fix of changed code is dropped");

        assert_eq!(
            analysis.counts(),
            Counts {
                unchanged: 1,
                changed: 1,
                ..Counts::default()
            }
        );
    }

    #[test]
    fn moves_warnings_and_fixes() {
        let mut analysis = analysis();
        analysis.learn(&uri(), ANALYZED);
        analysis.relocate(&uri(), &format!("// comment\n{ANALYZED}"));

        let shown = analysis.warnings(&uri(), Changed::Hide);
        assert_eq!(shown[0].status, Status::Moved);
        assert_eq!(shown[0].range, range(4, 4, 6));
        assert_eq!(shown[0].fixes[0].changes[&uri()][0].range, range(4, 4, 4));
        assert_eq!(analysis.counts().moved, 2);
        assert_eq!(analysis.len(), 2);
    }

    #[test]
    fn keeps_unknown_code_unverified() {
        let mut analysis = analysis();
        analysis.relocate(&uri(), "");
        let shown = analysis.warnings(&uri(), Changed::Hide);

        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].status, Status::Unverified);
        assert_eq!(shown[0].range, range(3, 4, 6));
    }

    #[test]
    fn parses_timestamps() {
        let time =
            |text| parse_time(text).map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs_f64());
        assert_eq!(time("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(time("2026-07-24T19:11:35Z"), Some(1_784_920_295.0));
        assert_eq!(time("2026-07-24T19:11:35.5Z"), Some(1_784_920_295.5));
        assert_eq!(time("2026-07-24T21:11:35+02:00"), Some(1_784_920_295.0));
        assert_eq!(time("2000-02-29T00:00:00Z"), Some(951_782_400.0));
        assert_eq!(time("yesterday"), None);
    }
}
