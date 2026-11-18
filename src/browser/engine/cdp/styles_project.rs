//! Project direct declarations of each explicitly bound node. Protocol selectors, values with custom syntax,
//! and global sheet IDs stay inside the native adapter.
use super::*;
use sem::styles::{StyleCandidate, StyleOrigin, StyleProperty, StyleSnapshot};
use serde_json::Value;

const MAX_RULES: usize = 32;
const MAX_PROPERTIES: usize = 128;
const MAX_CANDIDATES: usize = 16;

impl StyleProperty {
    fn name(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Visibility => "visibility",
            Self::PointerEvents => "pointer-events",
        }
    }

    fn literal(self, authored: &str) -> Option<String> {
        if authored.len() > 128 {
            return None;
        }
        let value = authored.trim().to_ascii_lowercase();
        let literal = value
            .strip_suffix("!important")
            .unwrap_or(&value)
            .trim_end();
        let allowed = matches!(
            literal,
            "inherit" | "initial" | "unset" | "revert" | "revert-layer"
        ) || match self {
            Self::Display => matches!(
                literal,
                "none"
                    | "contents"
                    | "block"
                    | "inline"
                    | "inline-block"
                    | "flex"
                    | "inline-flex"
                    | "grid"
                    | "inline-grid"
                    | "table"
                    | "list-item"
                    | "flow-root"
            ),
            Self::Visibility => matches!(literal, "visible" | "hidden" | "collapse"),
            Self::PointerEvents => matches!(
                literal,
                "auto"
                    | "none"
                    | "all"
                    | "visible"
                    | "painted"
                    | "fill"
                    | "stroke"
                    | "bounding-box"
                    | "visiblepainted"
                    | "visiblefill"
                    | "visiblestroke"
            ),
        };
        allowed.then(|| literal.to_owned())
    }
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    value.get(key).map_or(Ok(&[]), |v| {
        v.as_array()
            .map(Vec::as_slice)
            .ok_or_else(sem::types::decode)
    })
}

fn flag(value: &Value, key: &str, default: bool) -> Result<bool> {
    value
        .get(key)
        .map_or(Ok(default), |v| v.as_bool().ok_or_else(sem::types::decode))
}

fn location(prop: &Value) -> Result<(Option<u32>, Option<u32>)> {
    let Some(range) = prop.get("range") else {
        return Ok((None, None));
    };
    let line = range["startLine"].as_u64().ok_or_else(sem::types::decode)?;
    let column = range["startColumn"]
        .as_u64()
        .ok_or_else(sem::types::decode)?;
    if line >= 1_000_000 || column >= 1_000_000 {
        return Ok((None, None));
    }
    Ok((Some(line as u32 + 1), Some(column as u32 + 1)))
}

pub(super) struct Projection {
    property: StyleProperty,
    sheets: Vec<String>,
    candidates: Vec<StyleCandidate>,
    inspected: usize,
    rules: usize,
    truncated: bool,
}

impl Projection {
    fn sheet(&mut self, style: &Value) -> Result<Option<u8>> {
        let Some(value) = style.get("styleSheetId") else {
            return Ok(None);
        };
        let id = value.as_str().ok_or_else(sem::types::decode)?;
        if id.is_empty() || id.len() > 256 {
            return Ok(None);
        }
        if let Some(index) = self.sheets.iter().position(|s| *s == id) {
            return Ok(Some(index as u8 + 1));
        }
        if self.sheets.len() == MAX_RULES {
            return Ok(None);
        }
        self.sheets.push(id.to_owned());
        Ok(Some(self.sheets.len() as u8))
    }

    fn style(&mut self, style: &Value, origin: StyleOrigin, depth: Option<u8>) -> Result<()> {
        let props = style["cssProperties"]
            .as_array()
            .ok_or_else(sem::types::decode)?;
        let count = props.len().min(MAX_PROPERTIES - self.inspected);
        self.inspected += count;
        self.truncated |= count < props.len();
        let mut block = Vec::new();
        for prop in &props[..count] {
            let name = prop["name"].as_str().ok_or_else(sem::types::decode)?;
            if !name.eq_ignore_ascii_case(self.property.name())
                || flag(prop, "disabled", false)?
                || !flag(prop, "parsedOk", true)?
            {
                continue;
            }
            let raw = prop["value"].as_str().ok_or_else(sem::types::decode)?;
            let (line, column) = location(prop)?;
            block.push(StyleCandidate {
                ancestor_depth: depth,
                origin,
                value: self.property.literal(raw),
                important: flag(prop, "important", false)?,
                sheet: self.sheet(style)?,
                line,
                column,
            });
        }
        // Chromium can report both the authored declaration with a source range
        // and a synthesized copy without one. Preserve distinct authored rules.
        for (i, item) in block.iter().enumerate() {
            if item.line.is_none()
                && block.iter().enumerate().any(|(j, other)| {
                    i != j
                        && other.line.is_some()
                        && item.value == other.value
                        && item.important == other.important
                })
            {
                continue;
            }
            if self.candidates.len() == MAX_CANDIDATES {
                self.truncated = true;
                break;
            }
            self.candidates.push(item.clone());
        }
        Ok(())
    }

    pub(super) fn new(property: StyleProperty) -> Self {
        Self {
            property,
            sheets: Vec::new(),
            candidates: Vec::new(),
            inspected: 0,
            rules: 0,
            truncated: false,
        }
    }

    pub(super) fn add_node(&mut self, raw: &Value, depth: Option<u8>) -> Result<()> {
        if !raw.is_object() {
            return Err(sem::types::decode());
        }
        if let Some(style) = raw.get("inlineStyle") {
            self.style(style, StyleOrigin::Inline, depth)?;
        }
        let matches = array(raw, "matchedCSSRules")?;
        let count = matches.len().min(MAX_RULES - self.rules);
        self.rules += count;
        self.truncated |= count < matches.len();
        for entry in matches.iter().rev().take(count) {
            if entry["matchingSelectors"]
                .as_array()
                .ok_or_else(sem::types::decode)?
                .is_empty()
            {
                continue;
            }
            let rule = &entry["rule"];
            let origin = match rule["origin"].as_str() {
                Some("regular") => StyleOrigin::Author,
                Some("user-agent") => StyleOrigin::UserAgent,
                Some("injected") => StyleOrigin::Injected,
                Some("inspector") => StyleOrigin::Inspector,
                _ => return Err(sem::types::decode()),
            };
            self.style(&rule["style"], origin, depth)?;
        }
        Ok(())
    }

    pub(super) fn finish(self, ancestors_examined: Option<u8>, more: bool) -> StyleSnapshot {
        StyleSnapshot::with_scope(
            self.property,
            self.candidates,
            self.truncated || more,
            ancestors_examined,
        )
    }
}

pub(super) fn project(raw: &Value, property: StyleProperty) -> Result<StyleSnapshot> {
    let mut projection = Projection::new(property);
    projection.add_node(raw, None)?;
    Ok(projection.finish(None, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(sheet: &str, props: Value) -> Value {
        json!({"matchingSelectors":[0],"rule":{"origin":"regular","selectorList":{"text":"private-selector"},"style":{"styleSheetId":sheet,"cssProperties":props}}})
    }

    #[test]
    fn direct_rules_preserve_distinct_authored_locations_and_hide_native_text() {
        let authored = json!({"name":"pointer-events","value":"none!important","important":true,"range":{"startLine":0,"startColumn":9}});
        let copy = json!({"name":"pointer-events","value":"none !important","important":true});
        let private = json!({"name":"pointer-events","value":"var(--private-value)"});
        let skipped = json!({"name":"pointer-events","value":"auto","disabled":true});
        let raw = json!({"matchedCSSRules":[rule("private-sheet-one",json!([authored,copy,private,skipped])),rule("private-sheet-two",json!([authored]))],
            "inlineStyle":{"cssProperties":[{"name":"pointer-events","value":"auto"}]}});
        let result = project(&raw, StyleProperty::PointerEvents).unwrap();
        assert!(!result.truncated);
        assert_eq!(result.candidates.len(), 4);
        assert_eq!(result.candidates[0].origin, StyleOrigin::Inline);
        assert_ne!(result.candidates[1].sheet, result.candidates[2].sheet);
        assert_eq!(result.candidates[2].column, Some(10));
        assert_eq!(result.candidates[2].value.as_deref(), Some("none"));
        assert!(result.candidates[2].important && result.candidates[3].value.is_none());
        assert!(!serde_json::to_string(&result).unwrap().contains("private"));
    }

    #[test]
    fn ancestor_nodes_share_rule_property_candidate_and_sheet_budgets() {
        let item = json!({"name":"visibility","value":"hidden"});
        let mut projection = Projection::new(StyleProperty::Visibility);
        projection
            .add_node(
                &json!({"matchedCSSRules":[rule("same",json!([item]))]}),
                Some(1),
            )
            .unwrap();
        projection
            .add_node(
                &json!({"matchedCSSRules":vec![rule("same",json!([item]));32]}),
                Some(2),
            )
            .unwrap();
        let snapshot = projection.finish(Some(2), false);
        assert!(snapshot.truncated);
        assert_eq!(snapshot.candidates.len(), MAX_CANDIDATES);
        assert_eq!(snapshot.candidates[0].ancestor_depth, Some(1));
        assert!(
            snapshot.candidates[1..]
                .iter()
                .all(|c| c.ancestor_depth == Some(2) && c.sheet == Some(1))
        );
        let mut projection = Projection::new(StyleProperty::Visibility);
        projection.add_node(&json!({"inlineStyle":{"cssProperties":vec![json!({"name":"color","value":"red"});128]}}),Some(1)).unwrap();
        projection
            .add_node(&json!({"inlineStyle":{"cssProperties":[item]}}), Some(2))
            .unwrap();
        let exhausted = projection.finish(Some(2), false);
        assert!(exhausted.truncated && exhausted.candidates.is_empty());
    }

    #[test]
    fn bounds_and_native_shape_are_checked_independently() {
        let prop = json!({"name":"display","value":"block"});
        let candidate = rule("1", json!([prop]));
        for value in [
            json!({"matchedCSSRules":vec![candidate.clone(); 33]}),
            json!({"inlineStyle":{"cssProperties":vec![json!({"name":"color","value":"x"});129]}}),
            json!({"matchedCSSRules":vec![candidate;17]}),
        ] {
            let result = project(&value, StyleProperty::Display).unwrap();
            assert!(result.truncated && result.candidates.len() <= 16);
        }
        assert!(
            project(
                &json!({"matchedCSSRules":[{"rule":{}}]}),
                StyleProperty::Display
            )
            .is_err()
        );
        assert!(
            project(
                &json!({"inlineStyle":{"cssProperties":[{"name":"display","value":{}}]}}),
                StyleProperty::Display
            )
            .is_err()
        );
    }
}
