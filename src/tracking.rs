//! Matches warnings to the current code.
//!
//! An anchor is the analyzed code of a warning: its line and the closest
//! non-blank lines above and below. A warning follows its line when code
//! around it is edited and counts as changed once the line itself is gone.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Position, Range, TextEdit};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub line: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The line is where it was analyzed.
    Unchanged,
    /// The line moved, e.g. because lines were added above.
    Moved,
    /// The line was edited or deleted: the warning is usually fixed.
    Changed,
    /// The analyzed code is unknown.
    #[default]
    Unverified,
}

/// Where a warning is in the current code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub status: Status,
    pub range: Range,
    /// Lines the warning moved by.
    pub delta: i64,
    /// Characters the indentation of the line changed by.
    pub shift: i64,
}

/// The lines of a file, indexed by their trimmed text.
pub struct Lines<'a> {
    lines: Vec<&'a str>,
    index: HashMap<&'a str, Vec<usize>>,
}

impl<'a> Lines<'a> {
    pub fn new(text: &'a str) -> Self {
        let lines: Vec<&str> = text.lines().collect();
        let mut index: HashMap<&str, Vec<usize>> = HashMap::new();

        for (i, line) in lines.iter().enumerate() {
            let key = line.trim();

            if !key.is_empty() {
                index.entry(key).or_default().push(i);
            }
        }

        Self { lines, index }
    }

    fn neighbor(&self, index: usize, up: bool) -> &'a str {
        let mut i = index;

        loop {
            if up {
                if i == 0 {
                    return "";
                }

                i -= 1;
            } else {
                i += 1;

                if i >= self.lines.len() {
                    return "";
                }
            }

            let line = self.lines[i].trim();

            if !line.is_empty() {
                return line;
            }
        }
    }

    /// The anchor of a 0-based line, or `None` for blank or missing lines.
    pub fn anchor(&self, line: u32) -> Option<Anchor> {
        let index = line as usize;
        let text = self.lines.get(index)?;

        if text.trim().is_empty() {
            return None;
        }

        Some(Anchor {
            line: text.to_string(),
            before: self.neighbor(index, true).to_string(),
            after: self.neighbor(index, false).to_string(),
        })
    }

    /// Finds the analyzed line of a warning. Among the lines with the same
    /// text, the one with the most matching neighbors wins, then the one
    /// closest to `last`, the last known line.
    pub fn place(&self, analyzed: Range, anchor: Option<&Anchor>, last: u32) -> Placement {
        let unmoved = |status| Placement {
            status,
            range: analyzed,
            delta: 0,
            shift: 0,
        };

        let Some(anchor) = anchor else {
            return unmoved(Status::Unverified);
        };

        let mut best: Option<(usize, u32, u64)> = None;

        for &i in self.index.get(anchor.line.trim()).into_iter().flatten() {
            let score = u32::from(self.neighbor(i, true) == anchor.before)
                + u32::from(self.neighbor(i, false) == anchor.after);
            let distance = (i as i64 - i64::from(last)).unsigned_abs();
            let better = match best {
                None => true,
                Some((_, s, d)) => score > s || (score == s && distance < d),
            };

            if better {
                best = Some((i, score, distance));
            }
        }

        // A line without matching context elsewhere in the file is a
        // different occurrence of the same code, not the analyzed one.
        let Some((index, _, _)) = best.filter(|&(_, score, distance)| score > 0 || distance == 0)
        else {
            return unmoved(Status::Changed);
        };

        let delta = index as i64 - i64::from(analyzed.start.line);
        let shift = indent(self.lines[index]) - indent(&anchor.line);
        let status = if delta == 0 && shift == 0 {
            Status::Unchanged
        } else {
            Status::Moved
        };
        let single_line = analyzed.end.line == analyzed.start.line;
        let end = Position::new(
            add(analyzed.end.line, delta),
            if single_line {
                add(analyzed.end.character, shift)
            } else {
                analyzed.end.character
            },
        );

        Placement {
            status,
            range: Range::new(
                Position::new(
                    add(analyzed.start.line, delta),
                    add(analyzed.start.character, shift),
                ),
                end,
            ),
            delta,
            shift,
        }
    }
}

impl Placement {
    /// The edits of a fix moved like the warning, or `None` if the code
    /// changed and the fix may no longer apply.
    pub fn edits(&self, edits: &[TextEdit]) -> Option<Vec<TextEdit>> {
        // Columns of the fix are only valid while the indentation is the same.
        if self.status == Status::Changed || self.shift != 0 {
            return None;
        }

        let moved = edits
            .iter()
            .map(|edit| TextEdit {
                range: Range::new(
                    Position::new(
                        add(edit.range.start.line, self.delta),
                        edit.range.start.character,
                    ),
                    Position::new(
                        add(edit.range.end.line, self.delta),
                        edit.range.end.character,
                    ),
                ),
                new_text: edit.new_text.clone(),
            })
            .collect();
        Some(moved)
    }
}

fn indent(line: &str) -> i64 {
    (line.len() - line.trim_start().len()) as i64
}

fn add(value: u32, delta: i64) -> u32 {
    (i64::from(value) + delta).clamp(0, i64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANALYZED: &str = "int main(void)\n{\n    char *p = 0;\n\n    *p = 1;\n    return 0;\n}\n";

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(line, end))
    }

    fn place(text: &str, analyzed: Range) -> Placement {
        let anchor = Lines::new(ANALYZED).anchor(analyzed.start.line);
        Lines::new(text).place(analyzed, anchor.as_ref(), analyzed.start.line)
    }

    #[test]
    fn records_line_and_neighbors() {
        let lines = Lines::new(ANALYZED);
        let anchor = lines.anchor(4).unwrap();

        assert_eq!(anchor.line, "    *p = 1;");
        assert_eq!(anchor.before, "char *p = 0;", "blank lines are skipped");
        assert_eq!(anchor.after, "return 0;");
        assert_eq!(lines.anchor(0).unwrap().before, "");
        assert!(lines.anchor(3).is_none(), "blank line");
        assert!(lines.anchor(99).is_none());
    }

    #[test]
    fn keeps_unchanged_warnings() {
        let placement = place(ANALYZED, range(4, 4, 6));
        assert_eq!(placement.status, Status::Unchanged);
        assert_eq!(placement.range, range(4, 4, 6));
    }

    #[test]
    fn follows_moved_lines() {
        let text = ANALYZED.replace("{\n", "{\n    int a;\n    int b;\n");
        let placement = place(&text, range(4, 4, 6));

        assert_eq!(placement.status, Status::Moved);
        assert_eq!(placement.range, range(6, 4, 6));
        assert_eq!(placement.delta, 2);
    }

    #[test]
    fn follows_indentation() {
        let text = ANALYZED.replace("    *p = 1;", "        *p = 1;");
        let placement = place(&text, range(4, 4, 6));

        assert_eq!(placement.status, Status::Moved);
        assert_eq!(placement.range, range(4, 8, 10));
    }

    #[test]
    fn detects_changed_lines() {
        let text = ANALYZED.replace("*p = 1;", "if (p) *p = 1;");
        assert_eq!(place(&text, range(4, 4, 6)).status, Status::Changed);

        let text = ANALYZED.replace("    *p = 1;\n", "");
        assert_eq!(place(&text, range(4, 4, 6)).status, Status::Changed);
    }

    #[test]
    fn ignores_copies_without_context() {
        let text = format!(
            "{}void f(void)\n{{\n    *p = 1;\n}}\n",
            ANALYZED.replace("    *p = 1;\n", "")
        );
        assert_eq!(place(&text, range(4, 4, 6)).status, Status::Changed);
    }

    #[test]
    fn prefers_matching_context_then_nearest_line() {
        let text = "x;\n    *p = 1;\ny;\nchar *p = 0;\n    *p = 1;\nreturn 0;\n";
        let anchor = Lines::new(ANALYZED).anchor(4);
        let placement = Lines::new(text).place(range(4, 4, 6), anchor.as_ref(), 1);

        assert_eq!(
            placement.range.start.line, 4,
            "best context wins over distance"
        );

        let text = "    *p = 1;\nchar *p = 0;\n    *p = 1;\nreturn 0;\nchar *p = 0;\n    *p = 1;\nreturn 0;\n";
        let placement = Lines::new(text).place(range(4, 4, 6), anchor.as_ref(), 4);

        assert_eq!(placement.range.start.line, 5, "nearest of equal matches");
    }

    #[test]
    fn leaves_unknown_code_unverified() {
        let placement = Lines::new(ANALYZED).place(range(4, 4, 6), None, 4);
        assert_eq!(placement.status, Status::Unverified);
        assert_eq!(placement.range, range(4, 4, 6));
    }

    #[test]
    fn moves_fix_edits() {
        let edit = TextEdit {
            range: range(4, 4, 11),
            new_text: "if (p) *p = 1;".into(),
        };

        let text = ANALYZED.replace("{\n", "{\n    int a;\n");
        let edits = place(&text, range(4, 4, 6))
            .edits(std::slice::from_ref(&edit))
            .unwrap();

        assert_eq!(edits[0].range, range(5, 4, 11));

        let text = ANALYZED.replace("    *p = 1;", "  *p = 1;");
        assert!(
            place(&text, range(4, 4, 6))
                .edits(std::slice::from_ref(&edit))
                .is_none()
        );

        let text = ANALYZED.replace("*p = 1;", "*p = 2;");
        assert!(place(&text, range(4, 4, 6)).edits(&[edit]).is_none());
    }
}
