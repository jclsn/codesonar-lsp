//! The checks (warning classes) of a CodeSonar installation, which its
//! `template.conf` lists with the rules that turn them on or off.

use serde::Serialize;

/// The languages of the checks that are offered.
pub const LANGUAGES: [&str; 1] = ["C and C++"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub class: String,
    pub language: String,
    /// Whether CodeSonar reports its warnings without a rule.
    pub default: bool,
}

/// The checks that `template.conf` lists for the languages, in the order
/// of the file: first the ones enabled by default, then the others.
pub fn checks(template: &str, languages: &[&str]) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut language = String::new();
    let mut default = None;

    for line in template.lines() {
        if line.contains("The following checks are enabled by default") {
            default = Some(true);
        } else if line.contains("The following checks are disabled by default") {
            default = Some(false);
        } else if let Some(name) = line
            .strip_prefix("# (")
            .and_then(|rest| rest.trim_end().strip_suffix(" warning classes)"))
        {
            language = name.to_string();
        } else if let Some(default) = default
            && languages.contains(&language.as_str())
            && let Some(class) = listed_class(line, default)
        {
            checks.push(Check {
                class,
                language: language.clone(),
                default,
            });
        } else if default.is_some() && !line.starts_with('#') && !line.trim().is_empty() {
            // The lists end with the first setting.
            break;
        }
    }

    checks
}

/// The class of a listed rule, e.g. `#    WARNING_FILTER += discard
/// class="Buffer Overrun"` for a check that is enabled by default.
fn listed_class(line: &str, default: bool) -> Option<String> {
    let action = if default { "discard" } else { "allow" };
    let class = line
        .strip_prefix('#')?
        .trim()
        .strip_prefix("WARNING_FILTER += ")?
        .strip_prefix(action)?
        .trim()
        .strip_prefix("class=\"")?
        .trim_end()
        .strip_suffix('"')?;
    Some(class.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = include_str!("../tests/fixtures/template_checks.conf");

    #[test]
    fn lists_the_checks_of_the_languages() {
        let check = |class: &str, default| Check {
            class: class.into(),
            language: "C and C++".into(),
            default,
        };
        assert_eq!(
            checks(TEMPLATE, &LANGUAGES),
            [
                check("Buffer Overrun", true),
                check("Null Pointer Dereference", true),
                check("Function Too Long", false),
            ]
        );
        assert_eq!(checks(TEMPLATE, &["Java"]).len(), 2);
    }
}
