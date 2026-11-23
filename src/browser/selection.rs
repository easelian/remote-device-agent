//! Which backend a session runs on, and what the device says it can offer.
use super::{Failure, Result, Settings};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Backend {
    Cdp,
    Bidi,
}
impl Backend {
    #[cfg(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    ))]
    pub fn profile_scope(self) -> super::profiles::Scope {
        match self {
            Self::Cdp => super::profiles::Scope::CdpV2,
            Self::Bidi => super::profiles::Scope::BidiV2,
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Self::Cdp => "cdp",
            Self::Bidi => "bidi",
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    pub version: u32,
    pub observation_profile: String,
    pub backend: Backend,
}
pub(super) fn parse(params: &Value) -> Result<Option<Selection>> {
    let Some(value) = params.get("selection") else {
        return Ok(None);
    };
    let selection: Selection = serde_json::from_value(value.clone())
        .map_err(|_| Failure::new("browser_selection_invalid"))?;
    if selection.version != 2 || selection.observation_profile != "semantic_dom_v1" {
        return Err(Failure::new("browser_selection_invalid"));
    }
    Ok(Some(selection))
}
fn installed(path: &std::path::Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
pub(super) fn extension(reason: Option<&str>) -> Value {
    json!({"version":1,"available":reason.is_none(),"reason":reason})
}
pub(super) fn with_input_extensions(mut caps: Value, reason: Option<&str>) -> Value {
    for name in [
        "after_action_view",
        "compact_observation",
        "visual_click",
        "visual_gestures",
        "submit_outcome",
        "submit_feedback",
        "submit_fields",
        "field_indicators",
        "pixel_indicators",
        "icon_diagnostics",
        "submit_validation_field",
        "submit_validation",
    ] {
        caps["extensions"][name] = extension(reason);
    }
    // Version 2 returns the changed regions in the compact layout and keeps
    // the references of untouched parts; version 1 returned a bounded overview.
    caps["extensions"]["after_action_view"]["version"] = 2.into();
    caps
}

pub(super) fn capability(settings: &Settings, common_reason: Option<&str>, cdp: bool) -> Value {
    let compiled = cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    ));
    let reason = if !compiled {
        Some("platform_or_build_unsupported")
    } else if !settings.capability_v2 {
        Some("disabled_by_local_policy")
    } else if settings.sites.strict() {
        Some("browser_policy_unsupported")
    } else {
        common_reason
    };
    let backends: Vec<_> = [
        (Backend::Cdp, "chromium", cdp),
        (
            Backend::Bidi,
            "firefox",
            settings
                .firefox_executable
                .as_ref()
                .is_some_and(|p| installed(p)),
        ),
    ]
    .into_iter()
    .map(|(backend, family, exists)| {
        let why = reason.or((!exists).then_some("browser_not_installed"));
        let caps = json!({"id":backend.id(),"browser_family":family,"available":why.is_none(),
                "reason":why,"stage":"candidate","observation_profiles":["semantic_dom_v1"],
                "extensions":{"exception_locations":extension(why), "actionability":extension(why),
                    "observation_delta":extension(why), "scoped_scroll":extension(why), "ref_hover":extension(why), "dom_evidence":extension(why), "form_validation":extension(why), "virtual_list":extension(why), "record_wait":extension(why),
                    "native_ax":extension(why.or((backend==Backend::Bidi).then_some("browser_feature_unsupported"))),
                    "ref_drag":extension(why.or((backend==Backend::Bidi).then_some("browser_feature_unsupported"))),
                    "matched_styles":extension(why.or((backend==Backend::Bidi).then_some("browser_feature_unsupported"))),
                    "ancestor_styles":extension(why.or((backend==Backend::Bidi).then_some("browser_feature_unsupported"))),
                    "task_events":{"version":1,"available":why.is_none(),"categories":["console","network"] }, "action_capture":{"version":1,"available":why.is_none() }, "network_progress":{"version":1,"available":why.is_none(),"reason":why}}});
        with_input_extensions(caps, why)
    })
    .collect();
    let available = backends.iter().any(|b| b["available"] == true);
    json!({"version":2,"available":available,"reason":if available{None}else{reason.or(Some("browser_not_installed"))},
        "named_profiles":settings.persistent_profiles,"uploads":settings.uploads,"downloads":settings.downloads,"observation_profiles":["semantic_dom_v1"],"backends":backends})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capability_gates_policy_platform_and_each_browser_independently() {
        // The policy can turn the session runtime off.
        let mut settings = Settings {
            capability_v2: false,
            ..Default::default()
        };
        let off = capability(&settings, None, true);
        assert_eq!(off["available"], false);
        assert_eq!(
            off["reason"],
            if cfg!(all(
                target_os = "linux",
                target_arch = "x86_64",
                feature = "experimental-browser-bidi"
            )) {
                "disabled_by_local_policy"
            } else {
                "platform_or_build_unsupported"
            }
        );
        settings = Settings::default();
        let compiled = cfg!(all(
            target_os = "linux",
            target_arch = "x86_64",
            feature = "experimental-browser-bidi"
        ));
        let only_cdp = capability(&settings, None, true);
        assert_eq!(only_cdp["backends"][0]["available"], compiled);
        assert_eq!(only_cdp["backends"][1]["available"], false);
        settings.firefox_executable = Some(std::env::current_exe().unwrap());
        let only_bidi = capability(&settings, None, false);
        assert_eq!(only_bidi["backends"][0]["available"], false);
        assert_eq!(only_bidi["backends"][1]["available"], compiled);
        for candidate in [&only_cdp, &only_bidi] {
            for backend in candidate["backends"].as_array().unwrap() {
                let ax = &backend["extensions"]["native_ax"];
                assert_eq!(ax["version"], 1);
                assert_eq!(backend["extensions"]["matched_styles"], *ax);
                assert_eq!(
                    ax["available"],
                    backend["available"] == true && backend["id"] == "cdp"
                );
                assert_eq!(
                    ax["reason"],
                    if backend["available"] == true && backend["id"] == "bidi" {
                        json!("browser_feature_unsupported")
                    } else {
                        backend["reason"].clone()
                    }
                );
                for name in ["actionability", "observation_delta", "dom_evidence"] {
                    let extension = &backend["extensions"][name];
                    assert_eq!(extension["version"], 1);
                    assert_eq!(extension["available"], backend["available"]);
                    assert_eq!(extension["reason"], backend["reason"]);
                }
            }
        }
        for reason in [
            "disabled_by_local_policy",
            "private_storage_unavailable",
            "shell_enabled",
        ] {
            let value = capability(&settings, Some(reason), true);
            assert_eq!(value["available"], false);
            assert!(
                value["backends"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|b| b["available"] == false)
            );
        }
        settings.sites.private_networks = crate::browser::sites::Rule::Deny;
        assert_eq!(capability(&settings, None, true)["available"], false);
    }
    #[test]
    fn only_a_complete_known_choice_selects_a_backend() {
        assert!(parse(&json!({})).unwrap().is_none());
        for backend in ["cdp", "bidi"] {
            assert_eq!(parse(&json!({"selection":{"version":2,"observation_profile":"semantic_dom_v1","backend":backend}})).unwrap().unwrap().backend.id(), backend);
        }
        for selection in [
            Value::Null,
            json!({}),
            json!({"version":1,"observation_profile":"semantic_dom_v1","backend":"cdp"}),
            json!({"version":2,"observation_profile":"cdp_ax_v1","backend":"cdp"}),
            json!({"version":2,"observation_profile":"semantic_dom_v1","backend":"auto"}),
            json!({"version":2,"observation_profile":"semantic_dom_v1","backend":"bidi","fallback":true}),
        ] {
            assert_eq!(
                parse(&json!({"selection":selection})).unwrap_err().code,
                "browser_selection_invalid"
            );
        }
    }
}
