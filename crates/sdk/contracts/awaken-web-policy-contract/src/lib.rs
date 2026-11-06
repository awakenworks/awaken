//! Closed URL-source selection shared by authored tool policy and Web execution.
//!
//! # Boundary
//!
//! Context: `agent-runtime`.
//!
//! Published SDK contract (ADR-0086): every context may read it, so its readers are not listed
//! here; `cargo tree -e normal -i awaken-web-policy-contract` prints them.

#![warn(unreachable_pub)]

mod configuration;
pub use configuration::*;

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};

/// Source identity comes from frozen execution descriptors, not a tool's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WebUrlSourceKind {
    UserInput,
    ClientTool,
    ServerTool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebUrlToolReference {
    ToolReference { name: String },
}

impl WebUrlToolReference {
    fn name(&self) -> &str {
        match self {
            Self::ToolReference { name } => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebUrlSourceFilter {
    All {},
    None {},
    Only { tools: Vec<WebUrlToolReference> },
    Except { tools: Vec<WebUrlToolReference> },
}

impl<'de> Deserialize<'de> for WebUrlSourceFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Normalize the two wire forms into the one policy enum. A second
        // tagged enum would duplicate its variants and closed-field rules.
        struct FilterVisitor;
        impl<'de> serde::de::Visitor<'de> for FilterVisitor {
            type Value = (String, Option<Vec<WebUrlToolReference>>);

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a URL source shorthand or closed object")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok((value.into(), None))
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut kind = None;
                let mut tools = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" if kind.is_none() => kind = Some(map.next_value::<String>()?),
                        "tools" if tools.is_none() => {
                            tools = Some(map.next_value::<Vec<WebUrlToolReference>>()?)
                        }
                        _ => {
                            return Err(serde::de::Error::custom(
                                "duplicate or unknown URL source field",
                            ));
                        }
                    }
                }
                Ok((
                    kind.ok_or_else(|| serde::de::Error::missing_field("type"))?,
                    tools,
                ))
            }
        }
        let (kind, tools) = deserializer.deserialize_any(FilterVisitor)?;
        let filter = match (kind.as_str(), tools) {
            ("all", None) => Self::All {},
            ("none", None) => Self::None {},
            ("only" | "except", Some(tools)) => {
                if kind == "only" {
                    Self::Only { tools }
                } else {
                    Self::Except { tools }
                }
            }
            _ => {
                return Err(serde::de::Error::custom(
                    "invalid URL source type or fields",
                ));
            }
        };
        filter.validate_shape().map_err(serde::de::Error::custom)?;
        Ok(filter)
    }
}

impl WebUrlSourceFilter {
    fn validate_shape(&self) -> Result<(), String> {
        if let Self::Only { tools } | Self::Except { tools } = self {
            let names: BTreeSet<_> = tools.iter().map(WebUrlToolReference::name).collect();
            if tools.is_empty()
                || tools.len() > 128
                || names.len() != tools.len()
                || names.iter().any(|name| name.is_empty())
            {
                return Err("URL source tool lists require 1..128 distinct nonempty names".into());
            }
        }
        Ok(())
    }
    fn allows(&self, name: &str) -> bool {
        match self {
            Self::All {} => true,
            Self::None {} => false,
            Self::Only { tools } => tools.iter().any(|tool| tool.name() == name),
            Self::Except { tools } => !tools.iter().any(|tool| tool.name() == name),
        }
    }

    fn references(&self) -> &[WebUrlToolReference] {
        match self {
            Self::Only { tools } | Self::Except { tools } => tools,
            Self::All {} | Self::None {} => &[],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchUrlSources {
    #[serde(default)]
    pub client_tool_results: Option<WebUrlSourceFilter>,
    #[serde(default)]
    pub server_tool_results: Option<WebUrlSourceFilter>,
    #[serde(default)]
    pub user_input: Option<WebUrlSourceFilter>,
}

impl WebFetchUrlSources {
    /// The caller supplies the single publication's custom-tool roster.
    pub fn validate(&self, custom_tools: &BTreeSet<&str>) -> Result<(), String> {
        self.validate_intrinsic()?;
        if let Some(rule) = &self.client_tool_results
            && rule
                .references()
                .iter()
                .any(|tool| !custom_tools.contains(tool.name()))
        {
            return Err("url_sources.client_tool_results references an unavailable tool".into());
        }
        Ok(())
    }

    /// Intrinsic rules also apply when execution decodes frozen inherited input.
    pub fn validate_intrinsic(&self) -> Result<(), String> {
        for rule in [
            &self.client_tool_results,
            &self.server_tool_results,
            &self.user_input,
        ]
        .into_iter()
        .flatten()
        {
            rule.validate_shape()?;
        }
        if self.user_input.as_ref().is_some_and(|rule| {
            !matches!(
                rule,
                WebUrlSourceFilter::All {} | WebUrlSourceFilter::None {}
            )
        }) {
            return Err("url_sources.user_input permits only all or none".into());
        }
        if let Some(rule) = &self.server_tool_results
            && rule
                .references()
                .iter()
                .any(|tool| !matches!(tool.name(), "web_search" | "web_fetch"))
        {
            return Err("url_sources.server_tool_results references an unavailable tool".into());
        }
        if [
            &self.client_tool_results,
            &self.server_tool_results,
            &self.user_input,
        ]
        .iter()
        .all(|rule| matches!(rule, Some(WebUrlSourceFilter::None {})))
        {
            return Err("url_sources cannot disable every source".into());
        }
        Ok(())
    }

    pub fn permits(&self, source: WebUrlSourceKind, name: &str) -> bool {
        let rule = match source {
            WebUrlSourceKind::UserInput => &self.user_input,
            WebUrlSourceKind::ServerTool => &self.server_tool_results,
            WebUrlSourceKind::ClientTool => &self.client_tool_results,
        };
        rule.as_ref().is_none_or(|rule| rule.allows(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn url_source_admission_and_normalization_decision_table() {
        // Causes: C1 shorthand/object; C2 closed tag/fields; C3 list cardinality,
        // unique exact names; C4 correct publication/server roster; C5 user-only
        // all/none; C6 at least one enabled source. Effects: canonical objects
        // with nullable keys, or reject before publication/effects.
        // R1 valid shorthand/object -> same value; R2 malformed/list/roster or
        // all-none -> reject. U1/U2 exact case comparison never widens a filter.
        let custom = BTreeSet::from(["lookup"]);
        let sources: WebFetchUrlSources = serde_json::from_value(json!({
            "user_input":"none", "server_tool_results":{"type":"only", "tools":[{"type":"tool_reference", "name":"web_search"}]},
            "client_tool_results":"all"
        })).unwrap();
        sources.validate(&custom).unwrap();
        assert!(!sources.permits(WebUrlSourceKind::UserInput, ""));
        assert!(sources.permits(WebUrlSourceKind::ServerTool, "web_search"));
        assert!(!sources.permits(WebUrlSourceKind::ServerTool, "web_fetch"));
        assert!(sources.permits(WebUrlSourceKind::ClientTool, "lookup"));
        assert_eq!(
            serde_json::to_value(&sources).unwrap()["user_input"],
            json!({"type":"none"})
        );
        assert_eq!(
            serde_json::to_value(WebFetchUrlSources::default()).unwrap()["user_input"],
            json!(null)
        );
        for invalid in [
            json!({"type":"all", "extra":1}),
            json!({"type":"all", "tools":null}),
            json!({"type":"only", "tools":[]}),
            json!({"type":"only", "tools":[{"type":"tool_reference", "name":"lookup"}, {"type":"tool_reference", "name":"lookup"}]}),
            json!({"type":"only", "tools":[{"type":"tool_reference", "name":"lookup", "extra":1}]}),
        ] {
            assert!(serde_json::from_value::<WebUrlSourceFilter>(invalid).is_err());
        }
        // R2 duplicate JSON keys cannot be erased by an intermediate value map.
        assert!(
            serde_json::from_str::<WebUrlSourceFilter>(r#"{"type":"all","type":"none"}"#).is_err()
        );
        for invalid in [
            json!({"user_input":"none", "client_tool_results":"none", "server_tool_results":"none"}),
            json!({"user_input":{"type":"only", "tools":[{"type":"tool_reference", "name":"lookup"}]}}),
            json!({"client_tool_results":{"type":"only", "tools":[{"type":"tool_reference", "name":"Lookup"}]}}),
            json!({"server_tool_results":{"type":"except", "tools":[{"type":"tool_reference", "name":"lookup"}]}}),
        ] {
            let value: WebFetchUrlSources = serde_json::from_value(invalid).unwrap();
            assert!(value.validate(&custom).is_err());
        }
        // R2 applies to typed callers too: even an empty Only/Except is not
        // an all/none user selector, independent of serde's list validation.
        for rule in [
            WebUrlSourceFilter::Only { tools: vec![] },
            WebUrlSourceFilter::Except { tools: vec![] },
        ] {
            assert!(
                WebFetchUrlSources {
                    user_input: Some(rule),
                    ..Default::default()
                }
                .validate_intrinsic()
                .is_err()
            );
        }
        // R3 the maximum distinct list is admitted; the next entry is refused.
        for (count, valid) in [(128, true), (129, false)] {
            let tools: Vec<_> = (0..count)
                .map(|i| json!({"type":"tool_reference", "name":format!("t{i}")}))
                .collect();
            assert_eq!(
                serde_json::from_value::<WebUrlSourceFilter>(json!({"type":"only", "tools":tools}))
                    .is_ok(),
                valid
            );
        }
    }
}

#[cfg(test)]
mod typed_source_shapes {
    use super::*;
    #[test]
    fn typed_filters_cannot_bypass_closed_list_shapes() {
        for tools in [
            vec![],
            vec![WebUrlToolReference::ToolReference {
                name: String::new(),
            }],
            vec![
                WebUrlToolReference::ToolReference {
                    name: "web_search".into()
                };
                2
            ],
        ] {
            for rule in [
                WebUrlSourceFilter::Only {
                    tools: tools.clone(),
                },
                WebUrlSourceFilter::Except {
                    tools: tools.clone(),
                },
            ] {
                for server in [false, true] {
                    let policy = WebFetchUrlSources {
                        client_tool_results: (!server).then_some(rule.clone()),
                        server_tool_results: server.then_some(rule.clone()),
                        user_input: None,
                    };
                    assert!(policy.validate_intrinsic().is_err());
                    assert!(admit_fetch_url("https://example.com/", &[], &[policy], None).is_err());
                }
            }
        }
    }
}
