//! One site-policy evaluator for native request interception and Fake schedules.
use crate::browser::{
    Failure, Result,
    sites::{Lookup, Sites, Verdict},
};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RequestKey {
    context: String,
    request: String,
    hop: u32,
}
#[derive(Clone, Default)]
pub(crate) struct Decisions(Arc<Mutex<DecisionState>>);
#[derive(Default)]
struct DecisionState {
    seen: HashSet<RequestKey>,
    metrics: Metrics,
}
#[derive(Clone, Default, serde::Serialize)]
pub(crate) struct Metrics {
    pub admitted: usize,
    pub active: usize,
    pub allowed: usize,
    pub denied: usize,
    pub redirects: usize,
    pub contexts: usize,
    pub unattributed: usize,
}
/// A non-cloneable request/hop lease. The session monitor owns it until the
/// native decision settles; duplicates remain fenced after completion.
pub(crate) struct DecisionToken {
    owner: Decisions,
    key: RequestKey,
    url: String,
    navigation: bool,
    known_context: bool,
    deadline: Instant,
}
impl Decisions {
    pub fn context_created(&self) {
        self.0.lock().unwrap().metrics.contexts += 1;
    }
    pub fn metrics(&self) -> Metrics {
        self.0.lock().unwrap().metrics.clone()
    }
    pub fn claim(
        &self,
        context: &str,
        request: &str,
        hop: u32,
        url: &str,
        navigation: bool,
        known_context: bool,
    ) -> Result<DecisionToken> {
        if context.len() > 512 || request.is_empty() || request.len() > 512 || url.len() > 8192 {
            return Err(Failure::new("browser_policy_token_invalid"));
        }
        let key = RequestKey {
            context: context.into(),
            request: request.into(),
            hop,
        };
        let mut state = self.0.lock().unwrap();
        if state.metrics.active >= 32 || state.seen.len() >= 4096 {
            return Err(Failure::new("browser_policy_capacity"));
        }
        if !state.seen.insert(key.clone()) {
            return Err(Failure::new("browser_policy_duplicate"));
        }
        state.metrics.active += 1;
        state.metrics.admitted += 1;
        state.metrics.redirects += usize::from(hop > 0);
        state.metrics.unattributed += usize::from(navigation && !known_context);
        Ok(DecisionToken {
            owner: self.clone(),
            key,
            url: url.into(),
            navigation,
            known_context,
            deadline: Policy::deadline(),
        })
    }
}
impl DecisionToken {
    pub fn request(&self) -> &str {
        &self.key.request
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub async fn allows(&self, policy: &Policy) -> bool {
        let lookup_deadline = self.deadline - Duration::from_millis(100);
        if Instant::now() >= lookup_deadline {
            return false;
        }
        !self.navigation || self.known_context && policy.allows(&self.url, lookup_deadline).await
    }
    pub fn settled(self, allowed: bool) {
        let mut state = self.owner.0.lock().unwrap();
        if allowed {
            state.metrics.allowed += 1;
        } else {
            state.metrics.denied += 1;
        }
    }
}
impl Drop for DecisionToken {
    fn drop(&mut self) {
        self.owner.0.lock().unwrap().metrics.active -= 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub navigation_intercept: bool,
    pub resolved_destination_check: bool,
    pub observed_connection_address: bool,
}
#[derive(Clone)]
pub(crate) struct Policy {
    sites: Sites,
    lookup: Arc<Lookup>,
}
impl Policy {
    pub fn new(sites: Sites) -> Self {
        Self::with_lookup(sites, Arc::new(Lookup::default()))
    }
    /// A policy that asks where host names lead through a lookup it shares.
    pub fn with_lookup(sites: Sites, lookup: Arc<Lookup>) -> Self {
        Self { sites, lookup }
    }
    pub fn admit(&self, capabilities: Capabilities) -> Result<()> {
        if !capabilities.navigation_intercept
            || !capabilities.resolved_destination_check
            || self.sites.strict() && !capabilities.observed_connection_address
        {
            return Err(Failure::new("browser_policy_unsupported"));
        }
        Ok(())
    }
    pub async fn allows(&self, text: &str, deadline: Instant) -> bool {
        if text.len() > 8192 {
            return false;
        }
        let Ok(url) = url::Url::parse(text) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return false;
        }
        match self.sites.judge(&url) {
            Verdict::Allow => true,
            Verdict::Deny => false,
            Verdict::Resolve => {
                let Some(host) = url.host_str() else {
                    return false;
                };
                match tokio::time::timeout_at(deadline.into(), self.lookup.addresses(host)).await {
                    Ok(Some(addresses)) => {
                        !addresses.is_empty() && self.sites.judge_addresses(&addresses)
                    }
                    _ => false,
                }
            }
        }
    }
    pub fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }
    #[cfg(test)]
    pub fn resolver(&self, resolver: crate::browser::sites::Resolver) {
        self.lookup.resolve_with(resolver);
    }
}
