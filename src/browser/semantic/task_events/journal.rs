//! What a page reported while the AI worked on it, as text an AI reads:
//! console lines and requests. A journal lasts for one grant of control over
//! one tab and is bounded. Header values that carry credentials are not kept,
//! and neither is any request or response body.
use super::Level;
use serde_json::Value;
use std::collections::VecDeque;
use std::fmt::Write;

const LINES: usize = 200;
const REQUESTS: usize = 300;
const TEXT: usize = 2000;
const ADDRESS: usize = 1000;
const HEADERS: usize = 24;
const HEADER: usize = 200;
/// Header names whose values identify the user to a site.
const SECRET: [&str; 10] = [
    "cookie",
    "auth",
    "token",
    "secret",
    "key",
    "session",
    "csrf",
    "xsrf",
    "password",
    "signature",
];

fn cut(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn place(url: &str, line: Option<u64>, column: Option<u64>) -> String {
    if url.is_empty() {
        return String::new();
    }
    let mut text = cut(url, 200);
    if let Some(line) = line {
        let _ = write!(text, ":{}", line + 1);
        if let Some(column) = column {
            let _ = write!(text, ":{}", column + 1);
        }
    }
    text
}

fn caller(stack: &Value) -> String {
    let frame = &stack["callFrames"][0];
    place(
        frame["url"].as_str().unwrap_or(""),
        frame["lineNumber"].as_u64(),
        frame["columnNumber"].as_u64(),
    )
}

/// One thing the page wrote to its console, or an error nothing caught.
pub(crate) struct Said {
    level: Level,
    text: String,
    place: String,
}

impl Said {
    /// Reads a CDP `Runtime.consoleAPICalled` or `Runtime.exceptionThrown`.
    pub(crate) fn parse(method: &str, params: &Value) -> Option<Self> {
        match method {
            "Runtime.consoleAPICalled" => {
                let level = match params["type"].as_str()? {
                    "log" => Level::Log,
                    "debug" => Level::Debug,
                    "info" => Level::Info,
                    "warning" => Level::Warning,
                    "error" | "assert" => Level::Error,
                    _ => return None,
                };
                let mut text = String::new();
                for part in params["args"].as_array().into_iter().flatten() {
                    if text.len() > TEXT {
                        break;
                    }
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    match (&part["value"], part["description"].as_str()) {
                        (Value::String(value), _) => text.push_str(value),
                        (Value::Null, Some(described)) => text.push_str(described),
                        (Value::Null, None) => text.push_str(
                            part["subtype"]
                                .as_str()
                                .or(part["type"].as_str())
                                .unwrap_or("?"),
                        ),
                        (value, _) => {
                            let _ = write!(text, "{value}");
                        }
                    }
                }
                Some(Self {
                    level,
                    text: cut(&text, TEXT),
                    place: caller(&params["stackTrace"]),
                })
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                if !details.is_object() {
                    return None;
                }
                let thrown = &details["exception"];
                let what = match (thrown["description"].as_str(), &thrown["value"]) {
                    (Some(described), _) => Some(described.to_owned()),
                    (None, Value::Null) => None,
                    (None, Value::String(value)) => Some(value.clone()),
                    (None, value) => Some(value.to_string()),
                };
                let lead = details["text"].as_str().unwrap_or("Uncaught");
                let text = match what {
                    Some(what) if !lead.contains(what.lines().next().unwrap_or("")) => {
                        format!("{lead} {what}")
                    }
                    _ => lead.to_owned(),
                };
                let mut place = place(
                    details["url"].as_str().unwrap_or(""),
                    details["lineNumber"].as_u64(),
                    details["columnNumber"].as_u64(),
                );
                if place.is_empty() {
                    place = caller(&details["stackTrace"]);
                }
                Some(Self {
                    level: Level::Error,
                    text: cut(&text, TEXT),
                    place,
                })
            }
            _ => None,
        }
    }
}

fn headers(value: &Value) -> Vec<(String, String)> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .take(HEADERS)
        .map(|(name, value)| {
            let name = name.to_ascii_lowercase();
            let value = if SECRET.iter().any(|word| name.contains(word)) {
                "(not kept)".to_owned()
            } else {
                cut(value.as_str().unwrap_or(""), HEADER)
            };
            (name, value)
        })
        .collect()
}

/// One step in the life of a request, or the page becoming another document.
pub(crate) enum Net {
    Start {
        id: Box<str>,
        method: String,
        url: String,
        kind: String,
        at: f64,
        from: String,
        headers: Vec<(String, String)>,
        redirected: Option<u16>,
    },
    Response {
        id: Box<str>,
        status: u16,
        reason: String,
        mime: String,
        headers: Vec<(String, String)>,
        cached: bool,
    },
    Finished {
        id: Box<str>,
        at: f64,
        bytes: u64,
    },
    Failed {
        id: Box<str>,
        at: f64,
        error: String,
        canceled: bool,
    },
    Navigated {
        loader: Box<str>,
        url: String,
    },
}

impl Net {
    /// Reads the CDP events of the Network domain and `Page.frameNavigated`.
    pub(crate) fn parse(method: &str, params: &Value) -> Option<Self> {
        let id = || -> Option<Box<str>> {
            params["requestId"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .map(Into::into)
        };
        let at = params["timestamp"].as_f64().unwrap_or(0.0);
        Some(match method {
            "Network.requestWillBeSent" => {
                let request = &params["request"];
                let url = request["url"].as_str()?;
                if url.starts_with("data:") || url.starts_with("blob:") {
                    return None;
                }
                let initiator = &params["initiator"];
                let mut from = caller(&initiator["stack"]);
                if from.is_empty() {
                    from = place(
                        initiator["url"].as_str().unwrap_or(""),
                        initiator["lineNumber"].as_u64(),
                        None,
                    );
                }
                Self::Start {
                    id: id()?,
                    method: cut(request["method"].as_str().unwrap_or("?"), 16),
                    url: cut(url, ADDRESS),
                    kind: params["type"]
                        .as_str()
                        .filter(|kind| kind.len() <= 32)
                        .unwrap_or("other")
                        .to_ascii_lowercase(),
                    at,
                    from,
                    headers: headers(&request["headers"]),
                    redirected: params["redirectResponse"]["status"]
                        .as_u64()
                        .and_then(|status| u16::try_from(status).ok()),
                }
            }
            "Network.responseReceived" => {
                let response = &params["response"];
                Self::Response {
                    id: id()?,
                    status: u16::try_from(response["status"].as_u64()?).ok()?,
                    reason: cut(response["statusText"].as_str().unwrap_or(""), 64),
                    mime: cut(response["mimeType"].as_str().unwrap_or(""), 64),
                    headers: headers(&response["headers"]),
                    cached: response["fromDiskCache"] == true
                        || response["fromPrefetchCache"] == true,
                }
            }
            "Network.loadingFinished" => Self::Finished {
                id: id()?,
                at,
                bytes: params["encodedDataLength"].as_f64().unwrap_or(0.0).max(0.0) as u64,
            },
            "Network.loadingFailed" => {
                let mut error = cut(params["errorText"].as_str().unwrap_or("failed"), 100);
                for reason in [
                    &params["blockedReason"],
                    &params["corsErrorStatus"]["corsError"],
                ] {
                    if let Some(reason) = reason.as_str().filter(|reason| reason.len() <= 64) {
                        let _ = write!(error, " ({reason})");
                    }
                }
                Self::Failed {
                    id: id()?,
                    at,
                    error,
                    canceled: params["canceled"] == true,
                }
            }
            "Page.frameNavigated" if params["frame"].get("parentId").is_none() => Self::Navigated {
                loader: params["frame"]["loaderId"].as_str().unwrap_or("").into(),
                url: cut(params["frame"]["url"].as_str().unwrap_or(""), ADDRESS),
            },
            _ => return None,
        })
    }
}

struct Line {
    seq: u64,
    said: Said,
    count: u32,
    told: bool,
}

impl Line {
    fn word(&self) -> &'static str {
        match self.said.level {
            Level::Log => "log",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warning => "warn",
            Level::Error => "error",
        }
    }
    fn error(&self) -> bool {
        matches!(self.said.level, Level::Error)
    }
    fn brief(&self) -> String {
        let mut text = format!(
            "c{} {} {}",
            self.seq,
            self.word(),
            cut(self.said.text.lines().next().unwrap_or(""), 240)
        );
        if !self.said.place.is_empty() {
            let _ = write!(text, " ({})", self.said.place);
        }
        if self.count > 1 {
            let _ = write!(text, " ×{}", self.count);
        }
        text
    }
}

struct Request {
    seq: u64,
    id: Box<str>,
    method: String,
    url: String,
    kind: String,
    started: f64,
    ended: Option<f64>,
    status: Option<u16>,
    reason: String,
    mime: String,
    error: Option<String>,
    canceled: bool,
    bytes: Option<u64>,
    cached: bool,
    from: String,
    asked: Vec<(String, String)>,
    answered: Vec<(String, String)>,
    told: bool,
}

impl Request {
    fn failed(&self) -> bool {
        (self.error.is_some() && !self.canceled) || self.status.is_some_and(|status| status >= 400)
    }
    /// A failure worth saying unasked. Every site lacks some icon.
    fn problem(&self) -> bool {
        self.failed() && !(self.kind == "other" && self.url.ends_with("/favicon.ico"))
    }
    fn took(&self) -> Option<u64> {
        self.ended
            .map(|ended| ((ended - self.started).max(0.0) * 1000.0).round() as u64)
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1000 => format!("{bytes}B"),
        1000..1_000_000 => format!("{:.1}kB", bytes as f64 / 1e3),
        _ => format!("{:.1}MB", bytes as f64 / 1e6),
    }
}

fn origin(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

/// Keeps the newest of `lines` that fit in `budget` bytes.
fn newest(head: String, mut lines: Vec<String>, budget: usize) -> String {
    let mut used = head.len();
    let mut keep = lines.len();
    while keep > 0 && used + lines[keep - 1].len() < budget.max(head.len() + 200) {
        used += lines[keep - 1].len() + 1;
        keep -= 1;
    }
    let mut text = head;
    if keep > 0 {
        let _ = write!(
            text,
            "\n({keep} older not shown; narrow with \"find\" or \"filter\")"
        );
    }
    for line in lines.drain(keep..) {
        text.push('\n');
        text.push_str(&line);
    }
    text
}

#[derive(Default)]
pub(crate) struct Log {
    lines: VecDeque<Line>,
    requests: VecDeque<Request>,
    next: u64,
    /// The address of the document on show, and the first entry that is its.
    page: String,
    since: u64,
}

impl Log {
    pub(crate) fn on(page: &str) -> Self {
        Self {
            page: page.to_owned(),
            ..Self::default()
        }
    }

    pub(crate) fn said(&mut self, said: Said) {
        if let Some(last) = self.lines.back_mut()
            && last.seq >= self.since
            && last.said.level as u8 == said.level as u8
            && last.said.text == said.text
            && last.said.place == said.place
        {
            last.count += 1;
            return;
        }
        if self.lines.len() == LINES {
            self.lines.pop_front();
        }
        self.next += 1;
        self.lines.push_back(Line {
            seq: self.next,
            said,
            count: 1,
            told: false,
        });
    }

    pub(crate) fn net(&mut self, event: Net) {
        fn open(requests: &VecDeque<Request>, id: &str) -> Option<usize> {
            requests.iter().rposition(|request| &*request.id == id)
        }
        match event {
            Net::Start {
                id,
                method,
                url,
                kind,
                at,
                from,
                headers,
                redirected,
            } => {
                if let Some(status) = redirected
                    && let Some(index) = open(&self.requests, &id)
                {
                    let earlier = &mut self.requests[index];
                    earlier.status = Some(status);
                    earlier.ended = Some(at);
                }
                if self.requests.len() == REQUESTS {
                    self.requests.pop_front();
                }
                self.next += 1;
                self.requests.push_back(Request {
                    seq: self.next,
                    id,
                    method,
                    url,
                    kind,
                    started: at,
                    ended: None,
                    status: None,
                    reason: String::new(),
                    mime: String::new(),
                    error: None,
                    canceled: false,
                    bytes: None,
                    cached: false,
                    from,
                    asked: headers,
                    answered: Vec::new(),
                    told: false,
                });
            }
            Net::Response {
                id,
                status,
                reason,
                mime,
                headers,
                cached,
            } => {
                if let Some(index) = open(&self.requests, &id) {
                    let request = &mut self.requests[index];
                    request.status = Some(status);
                    request.reason = reason;
                    request.mime = mime;
                    request.answered = headers;
                    request.cached = cached;
                }
            }
            Net::Finished { id, at, bytes } => {
                if let Some(index) = open(&self.requests, &id) {
                    let request = &mut self.requests[index];
                    request.ended = Some(at);
                    request.bytes = Some(bytes);
                }
            }
            Net::Failed {
                id,
                at,
                error,
                canceled,
            } => {
                if let Some(index) = open(&self.requests, &id) {
                    let request = &mut self.requests[index];
                    request.ended = Some(at);
                    request.error = Some(error);
                    request.canceled = canceled;
                }
            }
            Net::Navigated { loader, url } => {
                // The request for the document, with the redirects before it,
                // belongs to the page it produced.
                self.since = self
                    .requests
                    .iter()
                    .filter(|request| request.id == loader)
                    .map(|request| request.seq)
                    .min()
                    .unwrap_or(self.next + 1);
                self.page = url;
            }
        }
    }

    fn address(&self, url: &str) -> String {
        let short = origin(&self.page)
            .and_then(|origin| url.strip_prefix(origin.as_str()))
            .filter(|rest| rest.starts_with('/'))
            .unwrap_or(url);
        cut(short, 160)
    }

    fn brief(&self, request: &Request) -> String {
        // A response that was refused, as a script answered with 404 is, has
        // both a status and an error; the status says more.
        let state = match (request.status, &request.error) {
            (Some(status), _) => status.to_string(),
            (None, Some(_)) => "✗".to_owned(),
            (None, None) => "…".to_owned(),
        };
        let mut text = format!(
            "n{} {state} {} {} {}",
            request.seq,
            request.method,
            self.address(&request.url),
            request.kind
        );
        if let Some(error) = &request.error {
            let _ = write!(text, " {error}");
            return text;
        }
        if let Some(took) = request.took() {
            let _ = write!(text, " {took}ms");
        }
        if request.cached {
            text.push_str(" cache");
        } else if let Some(bytes) = request.bytes {
            let _ = write!(text, " {}", size(bytes));
        }
        text
    }

    /// The errors and failed requests not said before, oldest first, and how
    /// many more there were than `limit`. Each is said once.
    pub(crate) fn problems(&mut self, limit: usize) -> (Vec<String>, usize) {
        let mut found: Vec<(u64, String)> = Vec::new();
        for line in self.lines.iter().filter(|line| !line.told && line.error()) {
            found.push((line.seq, line.brief()));
        }
        for request in &self.requests {
            if !request.told && request.problem() {
                found.push((request.seq, self.brief(request)));
            }
        }
        for line in &mut self.lines {
            line.told |= line.error();
        }
        for request in &mut self.requests {
            request.told |= request.problem();
        }
        found.sort_by_key(|(seq, _)| *seq);
        let more = found.len().saturating_sub(limit);
        (
            found
                .into_iter()
                .take(limit)
                .map(|(_, text)| text)
                .collect(),
            more,
        )
    }

    fn loaded(&self) -> String {
        if self.page.is_empty() {
            "since recording began".to_owned()
        } else {
            format!("since {} loaded", cut(&self.page, 200))
        }
    }

    pub(crate) fn console(&self, find: Option<&str>, errors: bool, budget: usize) -> String {
        let find = find.map(str::to_lowercase);
        let mine = || self.lines.iter().filter(|line| line.seq >= self.since);
        let lines: Vec<String> = mine()
            .filter(|line| !errors || line.error())
            .filter(|line| {
                find.as_ref().is_none_or(|find| {
                    line.said.text.to_lowercase().contains(find)
                        || line.said.place.to_lowercase().contains(find)
                })
            })
            .map(Line::brief)
            .collect();
        let head = format!(
            "console: {} of {} lines {} ({} errors)",
            lines.len(),
            mine().count(),
            self.loaded(),
            mine().filter(|line| line.error()).count()
        );
        newest(head, lines, budget)
    }

    pub(crate) fn network(&self, find: Option<&str>, failed: bool, budget: usize) -> String {
        let find = find.map(str::to_lowercase);
        let mine = || {
            self.requests
                .iter()
                .filter(|request| request.seq >= self.since)
        };
        let lines: Vec<String> = mine()
            .filter(|request| !failed || request.failed())
            .map(|request| self.brief(request))
            .filter(|line| {
                find.as_ref()
                    .is_none_or(|find| line.to_lowercase().contains(find))
            })
            .collect();
        let head = format!(
            "network: {} of {} requests {} ({} failed, {} pending)",
            lines.len(),
            mine().count(),
            self.loaded(),
            mine().filter(|request| request.failed()).count(),
            mine().filter(|request| request.ended.is_none()).count()
        );
        newest(head, lines, budget)
    }

    /// Everything kept about the entry an ID such as `c12` or `n7` names.
    pub(crate) fn detail(&self, id: &str) -> Option<String> {
        let seq: u64 = id.get(1..)?.parse().ok()?;
        if id.starts_with('c') {
            let line = self.lines.iter().find(|line| line.seq == seq)?;
            let mut text = format!("c{} {}", line.seq, line.word());
            if !line.said.place.is_empty() {
                let _ = write!(text, " at {}", line.said.place);
            }
            if line.count > 1 {
                let _ = write!(text, " ×{}", line.count);
            }
            let _ = write!(text, "\n{}", line.said.text);
            return Some(text);
        }
        if !id.starts_with('n') {
            return None;
        }
        let request = self.requests.iter().find(|request| request.seq == seq)?;
        let mut text = format!("n{} {} {}", request.seq, request.method, request.url);
        if let Some(status) = request.status {
            let _ = write!(text, " → {status} {}", request.reason);
        }
        match &request.error {
            Some(error) => {
                let _ = write!(text, " ✗ {error}");
            }
            None if request.status.is_none() => text.push_str(" … no response yet"),
            None => {}
        }
        let _ = write!(text, "\n{}", request.kind);
        if let Some(took) = request.took() {
            let _ = write!(text, ", {took} ms");
        }
        if let Some(bytes) = request.bytes {
            let _ = write!(text, ", {} received", size(bytes));
        }
        if request.cached {
            text.push_str(", from cache");
        }
        if !request.mime.is_empty() {
            let _ = write!(text, ", {}", request.mime);
        }
        if !request.from.is_empty() {
            let _ = write!(text, "\nstarted by {}", request.from);
        }
        for (title, headers) in [
            ("request headers", &request.asked),
            ("response headers", &request.answered),
        ] {
            if !headers.is_empty() {
                let _ = write!(text, "\n{title}:");
                for (name, value) in headers {
                    let _ = write!(text, "\n  {name}: {value}");
                }
            }
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn start(id: &str, url: &str, kind: &str) -> Net {
        Net::parse(
            "Network.requestWillBeSent",
            &json!({"requestId":id,"timestamp":10.0,"type":kind,
                "request":{"url":url,"method":"GET","headers":{"Accept":"*/*","Authorization":"Bearer SECRET","X-Api-Key":"SECRET"}},
                "initiator":{"type":"script","stack":{"callFrames":[{"url":"https://shop.test/app.js","lineNumber":87,"columnNumber":11}]}}}),
        )
        .unwrap()
    }

    fn response(id: &str, status: u64) -> Net {
        Net::parse(
            "Network.responseReceived",
            &json!({"requestId":id,"response":{"status":status,"statusText":"Nope","mimeType":"application/json",
                "headers":{"Content-Type":"application/json","Set-Cookie":"sid=SECRET"}}}),
        )
        .unwrap()
    }

    fn finished(id: &str) -> Net {
        Net::parse(
            "Network.loadingFinished",
            &json!({"requestId":id,"timestamp":10.132,"encodedDataLength":1234}),
        )
        .unwrap()
    }

    fn error(text: &str) -> Said {
        Said::parse(
            "Runtime.consoleAPICalled",
            &json!({"type":"error","args":[{"type":"string","value":text}]}),
        )
        .unwrap()
    }

    #[test]
    fn a_failure_is_said_once_and_credentials_are_never_kept() {
        let mut log = Log::on("https://shop.test/cart");
        log.net(start("1", "https://shop.test/api/save?x=1", "Fetch"));
        log.net(response("1", 500));
        log.net(finished("1"));
        log.net(start("2", "https://shop.test/favicon.ico", "Other"));
        log.net(response("2", 404));
        log.net(start("3", "https://cdn.test/a.js", "Script"));
        log.net(
            Net::parse(
                "Network.loadingFailed",
                &json!({"requestId":"3","timestamp":11.0,"errorText":"net::ERR_NAME_NOT_RESOLVED"}),
            )
            .unwrap(),
        );
        log.net(start("4", "https://shop.test/old", "Fetch"));
        log.net(
            Net::parse(
                "Network.loadingFailed",
                &json!({"requestId":"4","timestamp":11.0,"errorText":"net::ERR_ABORTED","canceled":true}),
            )
            .unwrap(),
        );
        log.said(error("boom"));
        log.said(error("boom"));
        let (problems, more) = log.problems(8);
        assert_eq!(
            problems,
            [
                "n1 500 GET /api/save?x=1 fetch 132ms 1.2kB",
                "n3 ✗ GET https://cdn.test/a.js script net::ERR_NAME_NOT_RESOLVED",
                "c5 error boom ×2",
            ]
        );
        assert_eq!(more, 0);
        assert_eq!(log.problems(8), (vec![], 0));
        let detail = log.detail("n1").unwrap();
        assert!(
            detail.starts_with("n1 GET https://shop.test/api/save?x=1 → 500 Nope\nfetch, 132 ms, 1.2kB received, application/json\nstarted by https://shop.test/app.js:88:12"),
            "{detail}"
        );
        assert!(
            detail.contains("authorization: (not kept)")
                && detail.contains("x-api-key: (not kept)")
        );
        assert!(detail.contains("set-cookie: (not kept)") && !detail.contains("SECRET"));
        let all = log.network(None, false, 4000);
        assert!(
            all.starts_with(
                "network: 4 of 4 requests since https://shop.test/cart loaded (3 failed, 1 pending)"
            ),
            "{all}"
        );
        assert!(all.contains("\nn2 404 GET /favicon.ico other"));
        let failed = log.network(Some("cdn"), true, 4000);
        assert_eq!(failed.lines().count(), 2, "{failed}");
        assert_eq!(log.detail("c5").unwrap(), "c5 error ×2\nboom");
        assert!(log.detail("c99").is_none() && log.detail("x").is_none());
    }

    #[test]
    fn a_list_starts_at_the_request_for_the_document_on_show() {
        let mut log = Log::on("https://shop.test/a");
        log.said(error("from the page before"));
        log.net(start("L1", "https://shop.test/login", "Document"));
        log.net(Net::Start {
            id: "L1".into(),
            method: "GET".into(),
            url: "https://shop.test/home".into(),
            kind: "document".into(),
            at: 10.5,
            from: String::new(),
            headers: Vec::new(),
            redirected: Some(302),
        });
        log.net(
            Net::parse(
                "Page.frameNavigated",
                &json!({"frame":{"id":"F","loaderId":"L1","url":"https://shop.test/home"}}),
            )
            .unwrap(),
        );
        assert!(
            Net::parse(
                "Page.frameNavigated",
                &json!({"frame":{"id":"C","parentId":"F","loaderId":"L9","url":"https://ads.test/"}})
            )
            .is_none()
        );
        log.said(error("from this page"));
        let console = log.console(None, false, 4000);
        assert_eq!(
            console,
            "console: 1 of 1 lines since https://shop.test/home loaded (1 errors)\nc4 error from this page"
        );
        let network = log.network(None, false, 4000);
        assert!(
            network.contains("\nn2 302 GET /login document")
                && network.contains("\nn3 … GET /home document"),
            "{network}"
        );
        // What the page before said is still told once.
        assert_eq!(
            log.problems(1),
            (vec!["c1 error from the page before".to_owned()], 1)
        );
    }

    #[test]
    fn the_newest_lines_are_kept_when_not_all_fit() {
        let mut log = Log::default();
        for index in 0..LINES + 50 {
            log.said(error(&format!("failure number {index} {}", "x".repeat(40))));
        }
        let text = log.console(None, true, 1000);
        assert!(text.len() < 1300, "{}", text.len());
        assert!(
            text.contains("older not shown")
                && text.ends_with(&format!("failure number {} {}", LINES + 49, "x".repeat(40)))
        );
        assert!(log.detail("c1").is_none());
    }

    #[test]
    fn an_uncaught_error_is_read_with_where_it_was_thrown() {
        let said = Said::parse(
            "Runtime.exceptionThrown",
            &json!({"exceptionDetails":{"text":"Uncaught","url":"https://shop.test/app.js","lineNumber":9,"columnNumber":4,
                "exception":{"type":"object","description":"TypeError: x is not a function\n    at save (https://shop.test/app.js:10:5)"}}}),
        )
        .unwrap();
        let mut log = Log::default();
        log.said(said);
        assert_eq!(
            log.problems(4).0,
            ["c1 error Uncaught TypeError: x is not a function (https://shop.test/app.js:10:5)"]
        );
        assert!(log.detail("c1").unwrap().contains("\n    at save"));
        let mixed = Said::parse(
            "Runtime.consoleAPICalled",
            &json!({"type":"warning","args":[{"type":"string","value":"left"},{"type":"number","value":3},
                {"type":"object","description":"Object"},{"type":"object","subtype":"null","value":null},{"type":"undefined"}]}),
        )
        .unwrap();
        assert_eq!(mixed.text, "left 3 Object null undefined");
        assert!(
            Said::parse(
                "Runtime.consoleAPICalled",
                &json!({"type":"table","args":[]})
            )
            .is_none()
        );
    }
}
