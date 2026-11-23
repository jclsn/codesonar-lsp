//! Resolving warnings on the hub, where the state and the finding of a
//! warning record that it was fixed or is a false positive. The hub keeps
//! them for the warning across analyses, and the SARIF export reports
//! them, so resolved warnings stay hidden.

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hub;

/// How a warning was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Resolution {
    /// The code was fixed: the state `Fixed`.
    Fixed,
    /// The warning is wrong: the finding `False Positive`.
    FalsePositive,
}

impl Resolution {
    pub const ALL: [Resolution; 2] = [Resolution::Fixed, Resolution::FalsePositive];

    /// The warning property on the hub, as the SARIF export names it, and
    /// its value.
    pub fn property(self) -> (&'static str, &'static str) {
        match self {
            Resolution::Fixed => ("state", "Fixed"),
            Resolution::FalsePositive => ("finding", "False Positive"),
        }
    }

    /// The form field that sets the property on the warning page.
    pub fn form_field(self) -> &'static str {
        match self {
            Resolution::Fixed => "sel_state",
            Resolution::FalsePositive => "sel_finding",
        }
    }

    /// The action of its code action.
    pub fn title(self) -> &'static str {
        match self {
            Resolution::Fixed => "Mark as fixed",
            Resolution::FalsePositive => "Mark as false positive",
        }
    }

    /// Completes "Mark the warning as …".
    pub fn describe(self) -> &'static str {
        match self {
            Resolution::Fixed => "fixed",
            Resolution::FalsePositive => "a false positive",
        }
    }

    /// The resolution that the properties of a warning record, e.g. the
    /// `properties` of a SARIF result.
    pub fn of(properties: &Value) -> Option<Self> {
        Self::ALL.into_iter().find(|resolution| {
            let (property, value) = resolution.property();
            properties.get(property).and_then(Value::as_str) == Some(value)
        })
    }
}

/// Records a resolution in a SARIF export, for the results of the warning
/// instance. Returns the changed export, or `None` if it has no such
/// result.
pub fn mark(sarif: &str, instance_id: u64, resolution: Resolution) -> Option<String> {
    let mut log: Value = serde_json::from_str(sarif).ok()?;
    let (property, value) = resolution.property();
    let mut marked = false;

    for run in log.get_mut("runs")?.as_array_mut()? {
        let Some(results) = run.get_mut("results").and_then(Value::as_array_mut) else {
            continue;
        };

        for result in results {
            if instance_of(result) != Some(instance_id) {
                continue;
            }

            if !result.get("properties").is_some_and(Value::is_object) {
                result["properties"] = json!({});
            }

            result["properties"][property] = Value::from(value);
            marked = true;
        }
    }

    marked.then(|| log.to_string())
}

/// The warning instance of a SARIF result: CodeSonar names it in the
/// properties and in the link to the hub.
fn instance_of(result: &Value) -> Option<u64> {
    result
        .pointer("/properties/id/instanceId")
        .and_then(Value::as_u64)
        .or_else(|| {
            let uri = result.get("hostedViewerUri")?.as_str()?;
            hub::warning_instance_id(&Url::parse(uri).ok()?)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_resolution_of_properties() {
        assert_eq!(
            Resolution::of(&json!({"state": "Fixed", "finding": "None"})),
            Some(Resolution::Fixed)
        );

        assert_eq!(
            Resolution::of(&json!({"state": "None", "finding": "False Positive"})),
            Some(Resolution::FalsePositive)
        );

        assert_eq!(Resolution::of(&json!({"state": "Assigned"})), None);
        assert_eq!(Resolution::of(&Value::Null), None);
    }

    #[test]
    fn marks_the_warning_in_an_export() {
        let result = |id: u64| json!({"ruleId": "x", "properties": {"id": {"instanceId": id}, "state": "None"}});
        let sarif = json!({"runs": [{"results": [result(1), result(2)]}]}).to_string();

        let marked: Value =
            serde_json::from_str(&mark(&sarif, 2, Resolution::Fixed).unwrap()).unwrap();

        assert_eq!(
            marked["runs"][0]["results"][0]["properties"]["state"],
            "None"
        );
        assert_eq!(
            marked["runs"][0]["results"][1]["properties"]["state"],
            "Fixed"
        );

        let marked: Value =
            serde_json::from_str(&mark(&sarif, 1, Resolution::FalsePositive).unwrap()).unwrap();

        assert_eq!(
            marked["runs"][0]["results"][0]["properties"]["finding"],
            "False Positive"
        );

        // Not in the export.
        assert_eq!(mark(&sarif, 3, Resolution::Fixed), None);

        // Without properties, the link to the hub names the instance.
        let linked = json!({"runs": [{"results": [
            {"hostedViewerUri": "https://hub:7340/warninginstance/7.html"},
        ]}]})
        .to_string();

        let marked: Value =
            serde_json::from_str(&mark(&linked, 7, Resolution::Fixed).unwrap()).unwrap();

        assert_eq!(
            marked["runs"][0]["results"][0]["properties"]["state"],
            "Fixed"
        );

        // Not a SARIF export.
        assert_eq!(mark("{", 1, Resolution::Fixed), None);
    }
}
