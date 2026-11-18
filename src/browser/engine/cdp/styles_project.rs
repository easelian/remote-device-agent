//! Project only direct declarations. Protocol selectors, values with custom syntax,
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

struct Projection<'a> {
    property: StyleProperty,
    sheets: Vec<&'a str>,
    candidates: Vec<StyleCandidate>,
    inspected: usize,
    truncated: bool,
}

impl<'a> Projection<'a> {
    fn sheet(&mut self, style: &'a Value) -> Result<Option<u8>> {
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
        self.sheets.push(id);
        Ok(Some(self.sheets.len() as u8))
    }

    fn style(&mut self, style: &'a Value, origin: StyleOrigin) -> Result<()> {
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
}

pub(super) fn project(raw: &Value, property: StyleProperty) -> Result<StyleSnapshot> {
    if !raw.is_object() {
        return Err(sem::types::decode());
    }
    let mut view = Projection {
        property,
        sheets: Vec::new(),
        candidates: Vec::new(),
        inspected: 0,
        truncated: false,
    };
    if let Some(style) = raw.get("inlineStyle") {
        view.style(style, StyleOrigin::Inline)?;
    }
    let matches = array(raw, "matchedCSSRules")?;
    view.truncated |= matches.len() > MAX_RULES;
    for entry in matches.iter().rev().take(MAX_RULES) {
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
        view.style(&rule["style"], origin)?;
    }
    Ok(StyleSnapshot::new(
        property,
        view.candidates,
        view.truncated,
    ))
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
