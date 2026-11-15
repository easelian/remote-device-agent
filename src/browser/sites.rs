//! Which sites a browser session may open: the device owner's lists, and the
//! built-in refusal of cloud metadata and link-local addresses.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Rule {
    #[default]
    Allow,
    Deny,
}

/// `browser.sites` in the local policy. Absent fields take these defaults:
/// public sites and private networks are allowed, the lists are empty.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sites {
    /// What happens to a public site that no list names.
    pub public: Rule,
    /// Public sites that may be opened when `public` is `deny`.
    pub allow: Patterns,
    /// Sites that are always refused.
    pub deny: Patterns,
    /// What happens to private networks and this device itself.
    pub private_networks: Rule,
    /// Hosts that are exempt from `private_networks` and from the built-in
    /// refusal of metadata and link-local addresses.
    pub private_allow: Patterns,
}

#[derive(Clone, Debug)]
enum Host {
    /// Exactly this host name.
    Name(String),
    /// Any host name below this one, not the name itself.
    Under(String),
    Address(IpAddr),
    /// An address range: the network address and its prefix length.
    Range(IpAddr, u8),
}

/// One entry of a site list: a host, with or without a port.
#[derive(Clone, Debug)]
pub struct Pattern {
    text: String,
    host: Host,
    port: Option<u16>,
}

impl Pattern {
    fn parse(text: &str) -> Result<Self, String> {
        let invalid = || {
            format!("browser.sites: \"{text}\" is not a host name, an address or an address range")
        };
        if text.is_empty() || text.len() > 255 || text.chars().any(char::is_whitespace) {
            return Err(invalid());
        }
        let pattern = |host, port| {
            Ok(Self {
                text: text.to_owned(),
                host,
                port,
            })
        };
        if let Some((address, bits)) = text.split_once('/') {
            let address: IpAddr = address.parse().map_err(|_| invalid())?;
            let bits: u8 = bits.parse().map_err(|_| invalid())?;
            if bits > if address.is_ipv4() { 32 } else { 128 } {
                return Err(invalid());
            }
            return pattern(Host::Range(address, bits), None);
        }
        let port = |digits: &str| match digits.parse::<u16>() {
            Ok(port) if port != 0 => Ok(Some(port)),
            _ => Err(invalid()),
        };
        if let Some(rest) = text.strip_prefix('[') {
            let (address, rest) = rest.split_once(']').ok_or_else(invalid)?;
            let address: Ipv6Addr = address.parse().map_err(|_| invalid())?;
            let port = match rest.strip_prefix(':') {
                Some(digits) => port(digits)?,
                None if rest.is_empty() => None,
                None => return Err(invalid()),
            };
            return pattern(Host::Address(address.into()), port);
        }
        if let Ok(address) = text.parse::<IpAddr>() {
            return pattern(Host::Address(address), None);
        }
        let (name, port) = match text.rsplit_once(':') {
            Some((name, digits)) => (name, port(digits)?),
            None => (text, None),
        };
        let (name, under) = match name.strip_prefix("*.") {
            Some(parent) => (parent, true),
            None => (name, false),
        };
        if name.contains('*') {
            return Err(invalid());
        }
        // The same reading a URL gets: lower case, punycode, and the numeric
        // spellings of an IPv4 address.
        let host = match url::Host::parse(name).map_err(|_| invalid())? {
            url::Host::Domain(name) if !name.trim_end_matches('.').is_empty() => {
                let name = name.trim_end_matches('.').to_owned();
                if under {
                    Host::Under(name)
                } else {
                    Host::Name(name)
                }
            }
            url::Host::Ipv4(address) if !under => Host::Address(address.into()),
            _ => return Err(invalid()),
        };
        pattern(host, port)
    }

    fn matches(&self, host: &url::Host<&str>, port: Option<u16>) -> bool {
        if self.port.is_some() && self.port != port {
            return false;
        }
        let address = match host {
            url::Host::Ipv4(address) => Some(IpAddr::V4(*address)),
            url::Host::Ipv6(address) => Some(IpAddr::V6(*address)),
            url::Host::Domain(_) => None,
        };
        let name = match host {
            url::Host::Domain(name) => Some(name.trim_end_matches('.')),
            _ => None,
        };
        match &self.host {
            Host::Name(expected) => name == Some(expected.as_str()),
            Host::Under(parent) => name.is_some_and(|name| {
                name.strip_suffix(parent.as_str())
                    .is_some_and(|rest| rest.ends_with('.'))
            }),
            Host::Address(expected) => address == Some(*expected),
            Host::Range(network, bits) => {
                address.is_some_and(|address| within(address, *network, *bits))
            }
        }
    }
}

fn within(address: IpAddr, network: IpAddr, bits: u8) -> bool {
    fn prefix(value: u128, width: u8, bits: u8) -> u128 {
        if bits == 0 {
            0
        } else {
            value >> (width - bits)
        }
    }
    match (address, network) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            prefix(u32::from(a).into(), 32, bits) == prefix(u32::from(n).into(), 32, bits)
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            prefix(u128::from(a), 128, bits) == prefix(u128::from(n), 128, bits)
        }
        _ => false,
    }
}

/// A site list as written in the policy.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct Patterns(Vec<Pattern>);

impl TryFrom<Vec<String>> for Patterns {
    type Error = String;
    fn try_from(entries: Vec<String>) -> Result<Self, String> {
        if entries.len() > 256 {
            return Err("browser.sites: a list holds at most 256 entries".into());
        }
        entries
            .iter()
            .map(|entry| Pattern::parse(entry))
            .collect::<Result<_, _>>()
            .map(Self)
    }
}
impl From<Patterns> for Vec<String> {
    fn from(patterns: Patterns) -> Self {
        patterns.0.into_iter().map(|pattern| pattern.text).collect()
    }
}
impl Patterns {
    fn matches(&self, host: &url::Host<&str>, port: Option<u16>) -> bool {
        self.0.iter().any(|pattern| pattern.matches(host, port))
    }
    fn shown(&self) -> Vec<&str> {
        self.0
            .iter()
            .take(SHOWN)
            .map(|pattern| pattern.text.as_str())
            .collect()
    }
}

/// How many entries of a list the capability report shows.
const SHOWN: usize = 32;

/// Where an address leads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Public,
    /// A private network, or this device itself.
    Private,
    /// A cloud metadata service or a link-local address: refused unless the
    /// owner lists the host.
    Metadata,
}

/// Metadata services that answer under a name of their own.
const METADATA_NAMES: &[&str] = &["metadata.google.internal"];

pub fn reach(address: IpAddr) -> Reach {
    match address {
        IpAddr::V4(address) => reach_v4(address),
        IpAddr::V6(address) => reach_v6(address),
    }
}

fn reach_v4(address: Ipv4Addr) -> Reach {
    let [a, b, c, _] = address.octets();
    // Link-local holds the metadata service of most clouds. One cloud serves
    // its own from an address outside it.
    if (a, b) == (169, 254) || address == Ipv4Addr::new(100, 100, 100, 200) {
        return Reach::Metadata;
    }
    let private = a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..128).contains(&b))
        || (a == 172 && (16..32).contains(&b))
        || (a, b) == (192, 168)
        || (a, b, c) == (192, 0, 0)
        || (a, b, c) == (192, 0, 2)
        || (a == 198 && (18..20).contains(&b))
        || (a, b, c) == (198, 51, 100)
        || (a, b, c) == (203, 0, 113)
        || a >= 224;
    if private {
        Reach::Private
    } else {
        Reach::Public
    }
}

fn reach_v6(address: Ipv6Addr) -> Reach {
    let segments = address.segments();
    let embedded = |high: u16, low: u16| {
        reach_v4(Ipv4Addr::new(
            (high >> 8) as u8,
            high as u8,
            (low >> 8) as u8,
            low as u8,
        ))
    };
    if address.is_unspecified() || address.is_loopback() {
        return Reach::Private;
    }
    // An IPv4 address carried inside an IPv6 one leads where the IPv4 address does.
    if segments[..5] == [0; 5] && matches!(segments[5], 0 | 0xffff) {
        return embedded(segments[6], segments[7]);
    }
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return embedded(segments[6], segments[7]);
    }
    if segments[0] == 0x2002 {
        return embedded(segments[1], segments[2]);
    }
    if segments[0] & 0xffc0 == 0xfe80
        || address == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
    {
        return Reach::Metadata;
    }
    if segments[0] & 0xfe00 == 0xfc00
        || segments[0] & 0xff00 == 0xff00
        || (segments[0], segments[1]) == (0x2001, 0xdb8)
    {
        return Reach::Private;
    }
    Reach::Public
}

/// What the lists and the address itself say about a URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
    /// A host name that nothing refuses so far: the addresses it leads to
    /// decide.
    Resolve,
}

impl Sites {
    /// Judges a URL by what is written in it: the lists, a literal address, a
    /// name that is known to be local.
    pub fn judge(&self, url: &url::Url) -> Verdict {
        let Some(host) = url.host() else {
            return Verdict::Deny;
        };
        let port = url.port_or_known_default();
        if self.deny.matches(&host, port) {
            return Verdict::Deny;
        }
        let exempt = self.private_allow.matches(&host, port);
        let known = match &host {
            url::Host::Ipv4(address) => Some(reach_v4(*address)),
            url::Host::Ipv6(address) => Some(reach_v6(*address)),
            url::Host::Domain(name) => {
                let name = name.trim_end_matches('.');
                if METADATA_NAMES.contains(&name) {
                    Some(Reach::Metadata)
                } else if name == "localhost" || name.ends_with(".localhost") {
                    Some(Reach::Private)
                } else {
                    None
                }
            }
        };
        let public = |pass| {
            if self.public == Rule::Deny && !self.allow.matches(&host, port) {
                Verdict::Deny
            } else {
                pass
            }
        };
        match known {
            Some(Reach::Metadata) if exempt => Verdict::Allow,
            Some(Reach::Metadata) => Verdict::Deny,
            Some(Reach::Private) if exempt || self.private_networks == Rule::Allow => {
                Verdict::Allow
            }
            Some(Reach::Private) => Verdict::Deny,
            Some(Reach::Public) => public(Verdict::Allow),
            // The owner named this host as a local one that may be opened.
            None if exempt => Verdict::Allow,
            None => public(Verdict::Resolve),
        }
    }

    /// Judges the addresses that a host name resolved to. The name itself has
    /// passed `judge`.
    pub fn judge_addresses(&self, addresses: &[IpAddr]) -> bool {
        addresses.iter().all(|address| match reach(*address) {
            Reach::Public => true,
            Reach::Private => self.private_networks == Rule::Allow,
            Reach::Metadata => false,
        })
    }

    /// Whether the owner refuses private networks. Only then is the address a
    /// connection really went to compared with the rules.
    pub fn strict(&self) -> bool {
        self.private_networks == Rule::Deny
    }

    /// Judges the address space the browser reports for a connection. The
    /// browser's names for the private spaces changed between versions, so
    /// only `Public` is relied on.
    pub fn judge_connection(&self, space: &str) -> bool {
        !self.strict() || space == "Public"
    }

    /// What the capability report says about the policy. The refused sites are
    /// not listed.
    pub fn summary(&self) -> Value {
        json!({
            "public": self.public,
            "private_networks": self.private_networks,
            "allow": self.allow.shown(),
            "private_allow": self.private_allow.shown(),
            "truncated": self.allow.0.len() > SHOWN || self.private_allow.0.len() > SHOWN,
            "metadata_addresses": "denied_unless_in_private_allow",
        })
    }
}

/// The addresses of a host name. `None` means the lookup gave no answer.
pub type Answer = Option<Vec<IpAddr>>;

/// Finds the addresses of a host name.
pub type Resolver =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Answer> + Send>> + Send + Sync>;

/// Makes a resolver out of an asynchronous lookup.
pub fn resolver<F, R>(lookup: F) -> Resolver
where
    F: Fn(String) -> R + Send + Sync + 'static,
    R: Future<Output = Answer> + Send + 'static,
{
    Arc::new(move |host| Box::pin(lookup(host)) as Pin<Box<dyn Future<Output = Answer> + Send>>)
}

/// The operating system's resolver, given a short time to answer.
pub fn system_resolver() -> Resolver {
    resolver(|host| async move {
        let lookup = tokio::net::lookup_host((host.as_str(), 0));
        match tokio::time::timeout(Duration::from_millis(1500), lookup).await {
            Ok(Ok(found)) => Some(found.map(|address| address.ip()).collect()),
            _ => None,
        }
    })
}

/// Host name lookups for the address rules. An answer is kept for a short
/// while, so that a page with many frames asks once.
pub struct Lookup {
    resolver: Mutex<Resolver>,
    known: Mutex<HashMap<String, (Instant, Answer)>>,
}

impl Default for Lookup {
    fn default() -> Self {
        Self {
            resolver: Mutex::new(system_resolver()),
            known: Mutex::new(HashMap::new()),
        }
    }
}

impl Lookup {
    const KEPT: Duration = Duration::from_secs(30);

    pub fn resolve_with(&self, resolver: Resolver) {
        *self.resolver.lock().unwrap() = resolver;
        self.known.lock().unwrap().clear();
    }

    /// The answer for a host name, when it was asked about a short while ago.
    pub fn remembered(&self, host: &str) -> Option<Answer> {
        match self.known.lock().unwrap().get(host) {
            Some((at, answer)) if at.elapsed() < Self::KEPT => Some(answer.clone()),
            _ => None,
        }
    }

    pub async fn addresses(&self, host: &str) -> Answer {
        if let Some(answer) = self.remembered(host) {
            return answer;
        }
        let resolver = self.resolver.lock().unwrap().clone();
        let answer = resolver(host.to_owned()).await;
        let mut known = self.known.lock().unwrap();
        if known.len() >= 256 {
            known.clear();
        }
        known.insert(host.to_owned(), (Instant::now(), answer.clone()));
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sites(value: Value) -> Sites {
        serde_json::from_value(value).unwrap()
    }
    fn judge(sites: &Sites, url: &str) -> Verdict {
        sites.judge(&url::Url::parse(url).unwrap())
    }
    fn address(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn addresses_are_sorted_by_where_they_lead() {
        for public in [
            "93.184.216.34",
            "8.8.8.8",
            "100.63.0.1",
            "172.32.0.1",
            "2606:4700::1",
        ] {
            assert_eq!(reach(address(public)), Reach::Public, "{public}");
        }
        for private in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "64:ff9b::c0a8:101",
        ] {
            assert_eq!(reach(address(private)), Reach::Private, "{private}");
        }
        for metadata in [
            "169.254.169.254",
            "169.254.0.23",
            "100.100.100.200",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
        ] {
            assert_eq!(reach(address(metadata)), Reach::Metadata, "{metadata}");
        }
    }

    #[test]
    fn by_default_only_metadata_and_link_local_addresses_are_refused() {
        let sites = Sites::default();
        for refused in [
            "http://169.254.169.254/latest/meta-data/",
            // The same address written as one number, and in hexadecimal.
            "http://2852039166/",
            "http://0xa9.0xfe.0xa9.0xfe/",
            "http://100.100.100.200/",
            "http://[fe80::1]/",
            "http://[fd00:ec2::254]/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://METADATA.google.internal./",
        ] {
            assert_eq!(judge(&sites, refused), Verdict::Deny, "{refused}");
        }
        for allowed in [
            "http://127.0.0.1:3000/",
            "http://localhost/",
            "http://app.localhost:8080/",
            "http://192.168.1.10/",
            "https://8.8.8.8/",
            "http://[::1]:9000/",
        ] {
            assert_eq!(judge(&sites, allowed), Verdict::Allow, "{allowed}");
        }
        // A name says nothing about where it leads.
        assert_eq!(judge(&sites, "https://example.com/"), Verdict::Resolve);
        assert_eq!(judge(&sites, "http://wiki.corp/"), Verdict::Resolve);
        assert!(sites.judge_addresses(&[address("93.184.216.34")]));
        assert!(sites.judge_addresses(&[address("10.1.2.3"), address("::1")]));
        assert!(!sites.judge_addresses(&[address("93.184.216.34"), address("169.254.169.254")]));
        assert!(!sites.strict());
        assert!(sites.judge_connection("Local"));
    }

    #[test]
    fn an_owner_can_refuse_private_networks_and_list_exceptions() {
        let sites = sites(json!({
            "private_networks": "deny",
            "private_allow": ["localhost:3000", "wiki.corp", "192.168.10.0/24", "169.254.7.7"],
        }));
        for refused in [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://localhost:3001/",
            "http://192.168.11.1/",
            "http://10.0.0.1/",
            "http://169.254.169.254/",
        ] {
            assert_eq!(judge(&sites, refused), Verdict::Deny, "{refused}");
        }
        for allowed in [
            "http://localhost:3000/",
            "http://wiki.corp/",
            "https://wiki.corp:8443/",
            "http://192.168.10.77/",
            // A listed link-local device is exempt from the built-in refusal too.
            "http://169.254.7.7/",
        ] {
            assert_eq!(judge(&sites, allowed), Verdict::Allow, "{allowed}");
        }
        assert_eq!(judge(&sites, "https://example.com/"), Verdict::Resolve);
        // A name that leads into a private network is refused once that is known.
        assert!(!sites.judge_addresses(&[address("10.1.2.3")]));
        assert!(sites.judge_addresses(&[address("93.184.216.34")]));
        assert!(sites.strict());
        // Both spellings a browser uses for a connection that is not public.
        for space in ["Loopback", "Local", "Private", "Unknown"] {
            assert!(!sites.judge_connection(space), "{space}");
        }
        assert!(sites.judge_connection("Public"));
    }

    #[test]
    fn an_owner_can_name_the_only_public_sites_and_sites_never_to_open() {
        let sites = sites(json!({
            "public": "deny",
            "allow": ["example.org", "*.example.org", "203.0.114.5"],
            "deny": ["pay.example.org", "*.bank.example"],
        }));
        assert_eq!(judge(&sites, "https://example.org/"), Verdict::Resolve);
        assert_eq!(
            judge(&sites, "https://docs.example.org/a"),
            Verdict::Resolve
        );
        assert_eq!(judge(&sites, "https://203.0.114.5/"), Verdict::Allow);
        for refused in [
            "https://example.com/",
            "https://notexample.org/",
            "https://203.0.114.6/",
            // A refused site stays refused even where a wider entry allows it.
            "https://pay.example.org/",
            "https://www.bank.example/",
        ] {
            assert_eq!(judge(&sites, refused), Verdict::Deny, "{refused}");
        }
        // The list of public sites does not open local ones, and does not close them.
        assert_eq!(judge(&sites, "http://127.0.0.1/"), Verdict::Allow);
        // The parent of a `*.` entry is a different site.
        let below = self::sites(json!({"deny": ["*.bank.example"]}));
        assert_eq!(judge(&below, "https://bank.example/"), Verdict::Resolve);
        assert_eq!(judge(&below, "https://a.b.bank.example/"), Verdict::Deny);
    }

    #[test]
    fn entries_are_read_the_way_urls_are() {
        let sites = sites(
            json!({"deny": ["EXAMPLE.com", "bücher.example", "127.1", "[fd00::1]:8080", "fd00::2"]}),
        );
        assert_eq!(judge(&sites, "https://example.com/"), Verdict::Deny);
        assert_eq!(judge(&sites, "https://example.com./"), Verdict::Deny);
        assert_eq!(
            judge(&sites, "https://xn--bcher-kva.example/"),
            Verdict::Deny
        );
        assert_eq!(judge(&sites, "http://127.0.0.1/"), Verdict::Deny);
        assert_eq!(judge(&sites, "http://[fd00::1]:8080/"), Verdict::Deny);
        assert_eq!(judge(&sites, "http://[fd00::1]/"), Verdict::Allow);
        assert_eq!(judge(&sites, "http://[fd00::2]:9/"), Verdict::Deny);
        // A port in an entry is compared with the port a URL implies.
        let ports = self::sites(json!({"deny": ["example.com:443"]}));
        assert_eq!(judge(&ports, "https://example.com/"), Verdict::Deny);
        assert_eq!(judge(&ports, "http://example.com/"), Verdict::Resolve);
    }

    #[test]
    fn a_policy_that_cannot_be_read_is_rejected() {
        for bad in [
            json!({"allow": ["http://example.com"]}),
            json!({"allow": ["example.com/path"]}),
            json!({"allow": ["exa mple.com"]}),
            json!({"allow": ["*.*.example.com"]}),
            json!({"allow": ["example.*"]}),
            json!({"allow": ["example.com:0"]}),
            json!({"allow": ["example.com:port"]}),
            json!({"allow": ["10.0.0.0/33"]}),
            json!({"allow": ["[::1"]}),
            json!({"allow": [""]}),
            json!({"allow": "example.com"}),
            json!({"public": "maybe"}),
            json!({"private": "deny"}),
        ] {
            assert!(
                serde_json::from_value::<Sites>(bad.clone()).is_err(),
                "{bad}"
            );
        }
        let many: Vec<String> = (0..257).map(|i| format!("host{i}.example")).collect();
        assert!(serde_json::from_value::<Sites>(json!({"deny": many})).is_err());
        let error = serde_json::from_value::<Sites>(json!({"deny": ["a b"]})).unwrap_err();
        assert!(error.to_string().contains("\"a b\""), "{error}");
    }

    #[test]
    fn the_report_shows_the_lists_that_open_sites_and_not_the_refused_ones() {
        let entries: Vec<String> = (0..40).map(|i| format!("host{i}.example")).collect();
        let summary = sites(json!({
            "public": "deny", "allow": entries, "deny": ["secret.example"],
            "private_allow": ["localhost:3000"],
        }))
        .summary();
        assert_eq!(summary["public"], "deny");
        assert_eq!(summary["private_networks"], "allow");
        assert_eq!(summary["allow"].as_array().unwrap().len(), 32);
        assert_eq!(summary["truncated"], true);
        assert_eq!(summary["private_allow"], json!(["localhost:3000"]));
        assert!(!summary.to_string().contains("secret.example"));
        assert_eq!(Sites::default().summary()["truncated"], false);
    }

    #[tokio::test]
    async fn a_lookup_is_asked_once_and_remembered() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let asked = Arc::new(AtomicUsize::new(0));
        let lookup = Lookup::default();
        let counter = asked.clone();
        lookup.resolve_with(resolver(move |host: String| {
            counter.fetch_add(1, Ordering::Relaxed);
            async move { (host == "known.example").then(|| vec![address("10.0.0.7")]) }
        }));
        assert_eq!(
            lookup.addresses("known.example").await,
            Some(vec![address("10.0.0.7")])
        );
        assert_eq!(
            lookup.addresses("known.example").await,
            Some(vec![address("10.0.0.7")])
        );
        assert_eq!(lookup.addresses("other.example").await, None);
        assert_eq!(asked.load(Ordering::Relaxed), 2);
    }
}
