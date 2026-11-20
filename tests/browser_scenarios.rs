//! Scenario pool: pages that change themselves in the ways real sites do,
//! driven through the production RPC path against a real Chromium.
#![cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
use axum::{
    Json, Router,
    extract::State,
    response::Html,
    routing::{get, post},
};
use rdh_agent::{config::DeviceConfig, policy::Policy, random, rpc::Runtime, transport};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

// The GNU native fixture embeds the production library rather than the CLI.
// Enter its real private supervisor before libtest parses arguments. Rust's
// GNU argv initializer uses priority 00099; this fixture runs immediately after.
// This entry is confined to this test executable and adds no production hook.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[used]
#[unsafe(link_section = ".init_array.00100")]
static SUPERVISOR_ENTRY: extern "C" fn() = {
    extern "C" fn enter() {
        if let Some(result) = rdh_agent::browser::run_internal_supervisor() {
            if let Err(error) = result {
                eprintln!("Browser fixture supervisor: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
    }
    enter
};

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Preserved failing browser fixture: {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Peer {
    runtime: Arc<Runtime>,
    sender: transport::Sender,
    receiver: tokio::sync::mpsc::Receiver<transport::Frame>,
    ids: Arc<Mutex<HashSet<String>>>,
    counter: u64,
    budget: u64,
}
impl Peer {
    async fn raw(
        &mut self,
        operation: &str,
        params: Value,
        owner: Option<&str>,
        receipt: Option<&str>,
    ) -> Value {
        self.raw_as(
            operation,
            params,
            owner.map(|owner| json!({"version":1,"owner":owner,"user":owner,"channel":"ai"})),
            receipt,
        )
        .await
    }
    async fn raw_as(
        &mut self,
        operation: &str,
        params: Value,
        principal: Option<Value>,
        receipt: Option<&str>,
    ) -> Value {
        let mut message =
            json!({"operation":operation,"params":params,"execution_budget_ms":self.budget});
        if let Some(principal) = principal {
            message["browser_principal"] = principal;
        }
        if let Some(receipt) = receipt {
            message["operation_receipt"] =
                json!({"operation_id":receipt,"instance_id":self.runtime.receipts.instance_id});
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut retries = 0;
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "RPC response deadline"
            );
            self.counter += 1;
            let id = format!("browser-test-{}", self.counter);
            message["id"] = id.clone().into();
            self.runtime
                .receive(&message, &self.sender, &self.ids)
                .unwrap();
            loop {
                let frame = tokio::time::timeout_at(deadline, self.receiver.recv())
                    .await
                    .expect("RPC response deadline")
                    .unwrap();
                let value: Value = serde_json::from_str(frame.message.to_text().unwrap()).unwrap();
                if value["id"] == id {
                    // A reply can arrive before the prior RPC releases its slot.
                    // Only retry an explicit pre-admission refusal, preserving
                    // the operation receipt; accepted or unknown work is never replayed.
                    if retries < 40
                        && value["error_code"] == "operation_busy"
                        && value["execution_state"] == "not_sent"
                        && value.get("execution_ms").is_none()
                    {
                        retries += 1;
                        break;
                    }
                    return value;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    async fn human(&mut self, params: Value) -> Value {
        self.raw_as(
            "browser_human",
            params,
            Some(
                json!({"version":1,"owner":"c".repeat(64),"user":"a".repeat(64),"channel":"human"}),
            ),
            None,
        )
        .await
    }
    async fn call(&mut self, operation: &str, params: Value) -> Value {
        let receipt = rdh_agent::browser::MUTATIONS
            .contains(&operation)
            .then(|| random::hex_token::<16>().unwrap());
        self.raw(operation, params, Some(&"a".repeat(64)), receipt.as_deref())
            .await
    }
    async fn observe(&mut self, id: &str, epoch: u64) -> Value {
        self.observe_named(id, epoch, "Save customer").await
    }
    async fn observe_named(&mut self, id: &str, epoch: u64, name: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..40 {
            let reply = self
                .call(
                    "browser_observe",
                    json!({"session_id":id,"control_epoch":epoch}),
                )
                .await;
            if reply["result"]["nodes"].as_array().is_some_and(|v| {
                v.iter()
                    .any(|n| n["name"].as_str().is_some_and(|s| s.trim() == name.trim()))
            }) {
                return reply["result"].clone();
            }
            assert!(
                !matches!(
                    reply["error_code"].as_str(),
                    Some("browser_lost" | "browser_io_error")
                ),
                "{reply}"
            );
            assert!(
                reply.get("error").is_none()
                    || matches!(
                        reply["error_code"].as_str(),
                        Some("browser_input_pending" | "browser_stale_reference")
                    ),
                "{reply}"
            );
            last = reply;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("fixture not observable: {last}");
    }
}
fn policy_peer(root: &std::path::Path, browser: Value) -> Peer {
    device_peer(root, json!({"version":1,"browser":browser}), false).unwrap()
}
/// A peer for a device with the given local policy, with or without a shell.
/// Fails where the device could not start with that policy.
fn device_peer(root: &std::path::Path, policy: Value, shell: bool) -> Result<Peer, String> {
    let config = DeviceConfig::parse(json!({"gateway_url":"http://127.0.0.1:8123","device_id":"profile-test","token":"x".repeat(48),"roots":rdh_agent::config::local_roots(&[format!("work={}", root.display())]).unwrap(),"allow_write":false,"allow_shell":shell})).unwrap();
    let policy = Policy::parse(&policy, &config).map_err(|error| error.to_string())?;
    let runtime =
        Runtime::with_config_path(config, policy, None, None, Some(&root.join("device.json")))
            .map_err(|error| error.to_string())?;
    let (sender, receiver, _) = transport::channel();
    Ok(Peer {
        runtime,
        sender,
        receiver,
        ids: Arc::new(Mutex::new(HashSet::new())),
        counter: 0,
        budget: 15000,
    })
}

// ---- Scenario pool -------------------------------------------------------
//
// Each scenario is one page under tests/fixtures/browser-scenarios/pages that
// changes itself in a particular way. The fixture server counts what the page
// writes, so a scenario is judged by what the site received and by what the
// page shows afterwards, never by a tool's own report. The same pages are
// driven with agent-browser by scripts/browser-scenarios-agent-browser.sh.

struct Site {
    child: std::process::Child,
    port: u16,
}
impl Site {
    fn start() -> Self {
        use std::io::BufRead;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/browser-scenarios/server.py");
        let mut child = std::process::Command::new("python3")
            .arg(script)
            .arg(port.to_string())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3 runs the scenario site");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("ready"), "{line}");
        Self { child, port }
    }
    fn url(&self, page: &str) -> String {
        format!("http://127.0.0.1:{}/{page}.html", self.port)
    }
    /// What the site received for one scenario, in order.
    fn writes(&self, scenario: &str) -> Vec<String> {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.write_all(b"GET /audit HTTP/1.0\r\n\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let body = reply.split("\r\n\r\n").nth(1).unwrap();
        let all: Value = serde_json::from_str(body).unwrap();
        all[scenario]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}
impl Drop for Site {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One browser session on one scenario page, counting what it costs.
struct Run<'a> {
    peer: &'a mut Peer,
    id: String,
    calls: usize,
    bytes: usize,
}
impl<'a> Run<'a> {
    async fn open(peer: &'a mut Peer, url: &str) -> Run<'a> {
        let selection =
            json!({"version":2,"observation_profile":"semantic_dom_v1","backend":"cdp"});
        let opened = peer
            .call("browser_open", json!({"url":url,"selection":selection}))
            .await;
        let id = opened["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{opened}"))
            .to_owned();
        Run {
            peer,
            id,
            calls: 1,
            bytes: 0,
        }
    }
    async fn call(&mut self, operation: &str, mut params: Value) -> Value {
        params["session_id"] = self.id.clone().into();
        params["control_epoch"] = 1.into();
        let reply = self.peer.call(operation, params).await;
        self.calls += 1;
        self.bytes += reply
            .get("result")
            .map_or(0, |result| result.to_string().len());
        if let Some(path) = std::env::var_os("RDH_SCENARIO_DUMP") {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(file, "{}", json!({"operation":operation,"reply":reply})).unwrap();
        }
        reply
    }
    async fn observe(&mut self, mut params: Value) -> Value {
        params["format"] = "compact".into();
        let reply = self.call("browser_observe", params).await;
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
    /// An action that also asks for the view of what changed.
    async fn act(&mut self, action: Value) -> Value {
        self.call(
            "browser_act",
            json!({"action":action,"after_action_view":true}),
        )
        .await
    }
    async fn click(&mut self, reference: &Value) -> Value {
        self.act(json!({"kind":"click","ref":reference})).await
    }
    async fn close(self) -> (usize, usize) {
        let id = self.id.clone();
        self.peer
            .call(
                "browser_control",
                json!({"action":"close","session_id":id,"control_epoch":1}),
            )
            .await;
        (self.calls, self.bytes)
    }
}

/// The nodes of a compact observation or of a sampled view. `within` of a node
/// is its record line and the names of the nodes under that line.
fn listed(view: &Value) -> Vec<Value> {
    let mut nodes: Vec<Value> = Vec::new();
    // Where the nodes under the current record line start, and that line.
    let mut record = (0, String::new());
    let close = |nodes: &mut Vec<Value>, record: &(usize, String)| {
        let block = nodes[record.0..]
            .iter()
            .fold(record.1.clone(), |block, node| {
                format!("{block}\n{}", node["name"].as_str().unwrap())
            });
        nodes[record.0..]
            .iter_mut()
            .for_each(|node| node["within"] = block.clone().into());
    };
    for line in view["text"].as_str().unwrap_or_default().lines() {
        if line.starts_with('<') || line.starts_with("---") || !line.starts_with("  ") {
            close(&mut nodes, &record);
            record = (
                nodes.len(),
                line.strip_prefix('<')
                    .map(|_| line.to_string())
                    .unwrap_or_default(),
            );
            if line.starts_with('<') || line.starts_with("---") {
                continue;
            }
        }
        // The name follows the first `: ` outside a quoted value.
        let (mut quoted, mut escaped, mut split) = (false, false, None);
        for (at, c) in line.char_indices() {
            match c {
                _ if escaped => escaped = false,
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                ':' if !quoted && line[at..].starts_with(": ") => {
                    split = Some(at);
                    break;
                }
                _ => {}
            }
        }
        let (head, name) = split.map_or((line, ""), |at| (&line[..at], &line[at + 2..]));
        let mut node = json!({"role":head.split_whitespace().next(),"name":name});
        if let Some(reference) = head
            .split_whitespace()
            .nth(1)
            .and_then(|word| word.strip_prefix('[')?.strip_suffix(']'))
        {
            node["ref"] = reference.into();
        }
        for (key, value) in head
            .split_whitespace()
            .filter_map(|word| word.split_once('='))
        {
            node[key] = value.trim_matches('"').into();
        }
        nodes.push(node);
    }
    close(&mut nodes, &record);
    nodes
}
/// The reference of the one control with this name whose record holds `within`.
fn control(view: &Value, name: &str, within: &str) -> Value {
    find_control(view, name, within).unwrap_or_else(|| panic!("{name} within {within:?}: {view}"))
}
/// As [`control`], for a view that may not hold the control.
fn find_control(view: &Value, name: &str, within: &str) -> Option<Value> {
    let found: Vec<_> = listed(view)
        .into_iter()
        .filter(|node| node["name"] == name && node.get("ref").is_some())
        .filter(|node| node["within"].as_str().unwrap().contains(within))
        .collect();
    (found.len() == 1).then(|| found[0]["ref"].clone())
}
fn view_of(reply: &Value) -> &Value {
    &reply["result"]["after_action_view"]
}
fn shows(view: &Value, text: &str) -> bool {
    view.to_string().contains(text)
}
fn outcome(reply: &Value) -> Value {
    json!([
        reply["result"]["input_acknowledged"],
        view_of(reply)["state"],
        view_of(reply)["reason"],
        reply["error_code"]
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, python3 and a sandbox-capable native host"]
async fn scenario_pool() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-scenarios-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"executable":executable}),
    );
    let site = Site::start();
    // RDH_SCENARIO_ONLY=sort,pager runs a part of the second batch while working on it.
    let only = std::env::var("RDH_SCENARIO_ONLY").ok();
    let wanted = |name: &str| {
        only.as_deref()
            .is_none_or(|list| list.split(',').any(|item| item == name))
    };
    let mut report = serde_json::Map::new();
    let mut failures = Vec::new();
    let mut record = |name: &str,
                      cost: (usize, usize),
                      writes: Vec<String>,
                      checks: Vec<(&str, bool)>,
                      detail: Value| {
        for (check, held) in &checks {
            if !held {
                failures.push(format!("{name}: {check}"));
            }
        }
        report.insert(name.into(), json!({"calls":cost.0,"bytes":cost.1,"writes":writes,
            "checks":checks.iter().map(|(check, held)| json!([check, held])).collect::<Vec<_>>(),"detail":detail}));
    };

    // A row element is given another order by scrolling.
    {
        let mut run = Run::open(&mut peer, &site.url("recycle")).await;
        let seen = run.observe(json!({"filter":"interactive"})).await;
        let old = control(&seen, "Approve", "Order 0003");
        let list = listed(&seen)
            .into_iter()
            .find(|node| node["kind"] == "scrollable")
            .map(|node| node["ref"].clone());
        let scrolled = run
            .act(json!({"kind":"scroll","ref":list,"delta_y":400}))
            .await;
        let wrong = run.click(&old).await;
        let after = run.observe(json!({"filter":"interactive"})).await;
        let fresh = control(&after, "Approve", "Order 0012");
        let right = run.click(&fresh).await;
        let cost = run.close().await;
        let writes = site.writes("recycle");
        record(
            "recycle",
            cost,
            writes.clone(),
            vec![
                (
                    "a reference to a recycled row is refused",
                    wrong["error_code"] == "browser_stale_reference",
                ),
                ("only the requested order is written", writes == ["12"]),
            ],
            json!({"scroll":outcome(&scrolled),"old_ref":outcome(&wrong),"fresh_ref":outcome(&right)}),
        );
    }
    // Every row element is replaced after a change.
    {
        let mut run = Run::open(&mut peer, &site.url("rerender")).await;
        let seen = run.observe(json!({})).await;
        let (second, fifth) = (
            control(&seen, "Remove", "Item 2"),
            control(&seen, "Remove", "Item 5"),
        );
        let removed = run.click(&second).await;
        let fresh = control(view_of(&removed), "Remove", "Item 4");
        let again = run.click(&fresh).await;
        let stale = run.click(&fifth).await;
        let cost = run.close().await;
        let writes = site.writes("rerender");
        record(
            "rerender",
            cost,
            writes.clone(),
            vec![
                (
                    "the view gives a usable reference to a replaced row",
                    again["result"]["input_acknowledged"] == true,
                ),
                (
                    "a reference to a replaced row is refused",
                    stale["error_code"] == "browser_stale_reference",
                ),
                (
                    "the two requested items are written once each",
                    writes == ["2", "4"],
                ),
            ],
            json!({"first":outcome(&removed),"second":outcome(&again),"old_ref":outcome(&stale),"view_bytes":view_of(&removed).to_string().len()}),
        );
    }
    // A style rule or a property changes the page; no element or attribute does.
    {
        let mut run = Run::open(&mut peer, &site.url("css-state")).await;
        let seen = run.observe(json!({})).await;
        let hidden_before = !shows(&seen, "Reset everything");
        let boxed = control(&seen, "Show advanced", "");
        let ticked = run
            .act(json!({"kind":"check","ref":boxed,"checked":true}))
            .await;
        let plan = control(&seen, "Plan", "");
        let chosen = run
            .act(json!({"kind":"select","ref":plan,"value":"pro"}))
            .await;
        let after = run.observe(json!({})).await;
        let cost = run.close().await;
        record(
            "css-state",
            cost,
            site.writes("css-state"),
            vec![
                ("the panel is hidden at first", hidden_before),
                (
                    "the view shows the panel a style rule revealed",
                    shows(view_of(&ticked), "Reset everything"),
                ),
                (
                    "the view shows the value a script set",
                    shows(view_of(&chosen), "20"),
                ),
                (
                    "an observation shows both",
                    shows(&after, "Reset everything") && shows(&after, "value=20"),
                ),
            ],
            json!({"tick":outcome(&ticked),"select":outcome(&chosen)}),
        );
    }
    // A confirmation is rendered at the end of the body, away from its row.
    {
        let mut run = Run::open(&mut peer, &site.url("modal")).await;
        let seen = run.observe(json!({"filter":"interactive"})).await;
        let (second, third) = (
            control(&seen, "Delete", "Order B42"),
            control(&seen, "Delete", "Order B43"),
        );
        let asked = run.click(&second).await;
        let named = shows(view_of(&asked), "Delete order B42?");
        let confirm = control(view_of(&asked), "Confirm", "");
        let confirmed = run.click(&confirm).await;
        let other = run.click(&third).await;
        let other_named = shows(view_of(&other), "Delete order B43?");
        let cancel = control(view_of(&other), "Cancel", "");
        let cancelled = run.click(&cancel).await;
        let cost = run.close().await;
        let writes = site.writes("modal");
        record(
            "modal",
            cost,
            writes.clone(),
            vec![
                ("the view names the order the dialog is about", named),
                (
                    "the view reports the removal and the notice",
                    view_of(&confirmed)["removed"] == true
                        && shows(view_of(&confirmed), "Deleted B42"),
                ),
                ("a reference to an untouched row still acts", other_named),
                ("only the confirmed order is deleted", writes == ["B42"]),
            ],
            json!({"ask":outcome(&asked),"confirm":outcome(&confirmed),"other":outcome(&other),"cancel":outcome(&cancelled),"dialog_view_bytes":view_of(&asked).to_string().len()}),
        );
    }
    // The page shows success, then takes it back.
    {
        let mut run = Run::open(&mut peer, &site.url("optimistic")).await;
        let seen = run.observe(json!({})).await;
        let approve = control(&seen, "Approve", "Order A17");
        let acted = run.click(&approve).await;
        let said_approved = shows(view_of(&acted), "Approved");
        tokio::time::sleep(Duration::from_millis(700)).await;
        let later = run.observe(json!({"find":"Order A17"})).await;
        let cost = run.close().await;
        record(
            "optimistic",
            cost,
            site.writes("optimistic"),
            vec![
                (
                    "the receipt does not claim the business result",
                    acted["result"]["business_result_verified"] == false,
                ),
                (
                    "a later read of the record shows the refusal",
                    shows(&later, "Rejected: limit exceeded") && !shows(&later, "Approved"),
                ),
            ],
            json!({"act":outcome(&acted),"view_showed_the_retracted_success":said_approved}),
        );
    }
    // A route change replaces the main content inside one document.
    {
        let mut run = Run::open(&mut peer, &site.url("spa")).await;
        let seen = run.observe(json!({})).await;
        let (home, orders) = (control(&seen, "Home", ""), control(&seen, "Orders", ""));
        let routed = run.click(&orders).await;
        let listed_orders = shows(view_of(&routed), "Order S03");
        let approved = if listed_orders {
            let third = control(view_of(&routed), "Approve", "Order S03");
            run.click(&third).await
        } else {
            Value::Null
        };
        let back = run.click(&home).await;
        let cost = run.close().await;
        let writes = site.writes("spa");
        record(
            "spa",
            cost,
            writes.clone(),
            vec![
                ("the view lists the new route's content", listed_orders),
                (
                    "the order chosen from the view is written",
                    writes == ["S03"],
                ),
                (
                    "navigation kept from before the route change still acts",
                    back["result"]["input_acknowledged"] == true,
                ),
            ],
            json!({"route":outcome(&routed),"approve":outcome(&approved),"home":outcome(&back),"route_view_bytes":view_of(&routed).to_string().len()}),
        );
    }
    // A clock and a progress bar change twenty times a second.
    {
        let mut run = Run::open(&mut peer, &site.url("ticker")).await;
        let seen = run.observe(json!({})).await;
        let second = control(&seen, "Approve", "Order T02");
        let acted = run.click(&second).await;
        let third = if view_of(&acted)["state"] == "sampled" {
            run.click(&control(&seen, "Approve", "Order T03")).await
        } else {
            Value::Null
        };
        let cost = run.close().await;
        let writes = site.writes("ticker");
        record(
            "ticker",
            cost,
            writes.clone(),
            vec![
                (
                    "the click is acknowledged on a page that keeps changing",
                    acted["result"]["input_acknowledged"] == true,
                ),
                (
                    "the view shows the changed record despite the noise",
                    shows(view_of(&acted), "Approved"),
                ),
                (
                    "the requested order is written",
                    writes.first().is_some_and(|w| w == "Order T02"),
                ),
            ],
            json!({"act":outcome(&acted),"next":outcome(&third),"view_bytes":view_of(&acted).to_string().len()}),
        );
    }
    // A choice rebuilds another field and reveals a note.
    {
        let mut run = Run::open(&mut peer, &site.url("dependent")).await;
        let seen = run.observe(json!({})).await;
        let country = control(&seen, "Country", "");
        let chosen = run
            .act(json!({"kind":"select","ref":country,"value":"jp"}))
            .await;
        let after = run.observe(json!({})).await;
        let city = control(&after, "City", "");
        let picked = run
            .act(json!({"kind":"select","ref":city,"value":"Osaka"}))
            .await;
        let save = control(&after, "Save address", "");
        let saved = run.click(&save).await;
        let cost = run.close().await;
        let writes = site.writes("dependent");
        record(
            "dependent",
            cost,
            writes.clone(),
            vec![
                (
                    "the view shows the rebuilt field and the revealed note",
                    shows(view_of(&chosen), "Tokyo")
                        && shows(view_of(&chosen), "Customs form required."),
                ),
                (
                    "the address is saved once with the chosen city",
                    writes == ["jp/Osaka"],
                ),
            ],
            json!({"country":outcome(&chosen),"city":outcome(&picked),"save":outcome(&saved)}),
        );
    }

    // Sorting moves the row elements; no row changes what it says.
    if wanted("sort") {
        let mut run = Run::open(&mut peer, &site.url("sort")).await;
        let seen = run.observe(json!({"filter":"interactive"})).await;
        let third = control(&seen, "Approve", "Invoice Q3");
        let sorted = run.click(&control(&seen, "Sort by amount", "")).await;
        let acted = run.click(&third).await;
        let cost = run.close().await;
        let writes = site.writes("sort");
        record(
            "sort",
            cost,
            writes.clone(),
            vec![(
                "a reference to a row that only moved still acts on that row",
                writes == ["Q3"],
            )],
            json!({"sort":outcome(&sorted),"old_ref":outcome(&acted),"sort_view_bytes":view_of(&sorted).to_string().len()}),
        );
    }
    // The next page writes other orders into the same row elements.
    if wanted("pager") {
        let mut run = Run::open(&mut peer, &site.url("pager")).await;
        let seen = run.observe(json!({"filter":"interactive"})).await;
        let old = control(&seen, "Approve", "Order P02");
        let paged = run.click(&control(&seen, "Next page", "")).await;
        let fresh = find_control(view_of(&paged), "Approve", "Order P07");
        let right = run.click(fresh.as_ref().unwrap_or(&Value::Null)).await;
        let wrong = run.click(&old).await;
        let cost = run.close().await;
        let writes = site.writes("pager");
        record(
            "pager",
            cost,
            writes.clone(),
            vec![
                ("the view lists the rows of the new page", fresh.is_some()),
                (
                    "a reference to a row that now shows another order is refused",
                    wrong["error_code"] == "browser_stale_reference",
                ),
                (
                    "only the order chosen from the new page is written",
                    writes == ["P07"],
                ),
            ],
            json!({"page":outcome(&paged),"fresh":outcome(&right),"old_ref":outcome(&wrong)}),
        );
    }
    // New entries keep pushing every row down.
    if wanted("feed") {
        let mut run = Run::open(&mut peer, &site.url("feed")).await;
        let seen = run.observe(json!({"filter":"interactive"})).await;
        let target = find_control(&seen, "Approve", "Order F1");
        let acted = run.click(target.as_ref().unwrap_or(&Value::Null)).await;
        let cost = run.close().await;
        let writes = site.writes("feed");
        record(
            "feed",
            cost,
            writes.clone(),
            vec![
                (
                    "a row that keeps moving can be read with a reference",
                    target.is_some(),
                ),
                ("the click lands on the requested row", writes == ["F1"]),
            ],
            json!({"act":outcome(&acted),"view_bytes":view_of(&acted).to_string().len()}),
        );
    }
    // Controls inside open shadow trees.
    if wanted("shadow") {
        let mut run = Run::open(&mut peer, &site.url("shadow")).await;
        let seen = run.observe(json!({})).await;
        let (second, third) = (
            find_control(&seen, "Approve", "Order K2"),
            find_control(&seen, "Approve", "Order K3"),
        );
        let acted = run.click(second.as_ref().unwrap_or(&Value::Null)).await;
        let other = run.click(third.as_ref().unwrap_or(&Value::Null)).await;
        let cost = run.close().await;
        let writes = site.writes("shadow");
        record(
            "shadow",
            cost,
            writes.clone(),
            vec![
                (
                    "the view shows a change made inside a shadow tree",
                    shows(view_of(&acted), "Approved") && shows(view_of(&acted), "Order K2"),
                ),
                ("both cards are written in order", writes == ["K2", "K3"]),
            ],
            json!({"first":outcome(&acted),"second":outcome(&other)}),
        );
    }
    // The orders live in a frame from another site.
    if wanted("xframe") {
        let mut run = Run::open(&mut peer, &site.url("xframe")).await;
        let mut seen = run.observe(json!({})).await;
        for _ in 0..20 {
            if shows(&seen, "Order X2") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            seen = run.observe(json!({})).await;
        }
        let target = find_control(&seen, "Approve", "Order X2");
        let acted = run.click(target.as_ref().unwrap_or(&Value::Null)).await;
        let cost = run.close().await;
        let writes = site.writes("xframe");
        record(
            "xframe",
            cost,
            writes.clone(),
            vec![
                (
                    "the frame's rows are read with references",
                    target.is_some(),
                ),
                (
                    "the view shows the change inside the frame",
                    shows(view_of(&acted), "Approved"),
                ),
                ("the requested order is written", writes == ["X2"]),
            ],
            json!({"act":outcome(&acted)}),
        );
    }
    // A modal dialog makes the page behind it inert.
    if wanted("toplayer") {
        let mut run = Run::open(&mut peer, &site.url("toplayer")).await;
        let seen = run.observe(json!({})).await;
        let danger = control(&seen, "Delete account", "");
        let opened = run.click(&control(&seen, "Open settings", "")).await;
        let save = find_control(view_of(&opened), "Save settings", "");
        let saved = run.click(save.as_ref().unwrap_or(&Value::Null)).await;
        let behind = run.click(&danger).await;
        let cost = run.close().await;
        let writes = site.writes("toplayer");
        record(
            "toplayer",
            cost,
            writes.clone(),
            vec![
                ("the view lists the dialog's controls", save.is_some()),
                (
                    "a control behind the dialog is not clicked",
                    behind.get("error").is_some() && !writes.contains(&"delete".to_owned()),
                ),
                (
                    "the dialog's own control is written once",
                    writes == ["save"],
                ),
            ],
            json!({"open":outcome(&opened),"save":outcome(&saved),"behind":outcome(&behind)}),
        );
    }
    // A row is edited in place: one observation, then views only.
    if wanted("inline") {
        let mut run = Run::open(&mut peer, &site.url("inline")).await;
        let seen = run.observe(json!({"filter":"interactive"})).await;
        let editing = run.click(&control(&seen, "Edit", "R2")).await;
        let field = find_control(view_of(&editing), "Name for R2", "");
        let typed = run
            .act(json!({"kind":"fill","ref":field,"text":"Zed"}))
            .await;
        // Typing changed the row, so the reference to its Save control is taken from the newer view.
        let save = find_control(view_of(&typed), "Save", "");
        let saved = run.click(save.as_ref().unwrap_or(&Value::Null)).await;
        let cost = run.close().await;
        let writes = site.writes("inline");
        record(
            "inline",
            cost,
            writes.clone(),
            vec![
                ("the view gives the field to type in", field.is_some()),
                (
                    "the view after typing gives a current reference to Save",
                    save.is_some(),
                ),
                (
                    "the row is saved once with the typed name",
                    writes == ["R2=Zed"],
                ),
                (
                    "the view shows the saved name",
                    shows(view_of(&saved), "Zed"),
                ),
            ],
            json!({"edit":outcome(&editing),"type":outcome(&typed),"save":outcome(&saved)}),
        );
    }
    // A form in three steps, each replacing the one before.
    if wanted("wizard") {
        let mut run = Run::open(&mut peer, &site.url("wizard")).await;
        let seen = run.observe(json!({})).await;
        let typed = run
            .act(json!({"kind":"fill","ref":control(&seen, "Email", ""),"text":"a@b.test"}))
            .await;
        let second = run.click(&control(&seen, "Next", "")).await;
        let plan = find_control(view_of(&second), "Plan", "");
        let next = find_control(view_of(&second), "Next", "");
        let chosen = run
            .act(json!({"kind":"select","ref":plan,"value":"pro"}))
            .await;
        let third = run.click(next.as_ref().unwrap_or(&Value::Null)).await;
        let create = find_control(view_of(&third), "Create account", "");
        let summary = shows(view_of(&third), "a@b.test on the pro plan");
        let done = run.click(create.as_ref().unwrap_or(&Value::Null)).await;
        let cost = run.close().await;
        let writes = site.writes("wizard");
        record(
            "wizard",
            cost,
            writes.clone(),
            vec![
                (
                    "each view gives the next step's controls",
                    plan.is_some() && next.is_some() && create.is_some(),
                ),
                ("the last step's summary is in its view", summary),
                (
                    "the account is created once with what was entered",
                    writes == ["a@b.test/pro"],
                ),
                (
                    "the view shows the confirmation",
                    shows(view_of(&done), "Account created"),
                ),
            ],
            json!({"type":outcome(&typed),"step2":outcome(&second),"plan":outcome(&chosen),"step3":outcome(&third),"create":outcome(&done)}),
        );
    }
    // Suggestions appear a moment after typing stops.
    if wanted("autocomplete") {
        let mut run = Run::open(&mut peer, &site.url("autocomplete")).await;
        let seen = run.observe(json!({})).await;
        let typed = run
            .act(json!({"kind":"fill","ref":control(&seen, "City", ""),"text":"Pa"}))
            .await;
        let in_view = find_control(view_of(&typed), "Paris", "");
        let mut after = Value::Null;
        let mut option = in_view.clone();
        for _ in 0..10 {
            if option.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            after = run.observe(json!({})).await;
            option = find_control(&after, "Paris", "");
        }
        let picked = run.click(option.as_ref().unwrap_or(&Value::Null)).await;
        let cost = run.close().await;
        let writes = site.writes("autocomplete");
        record(
            "autocomplete",
            cost,
            writes.clone(),
            vec![
                ("a suggestion can be chosen by reference", option.is_some()),
                ("the chosen city is written once", writes == ["Paris"]),
                (
                    "the view shows the result of choosing",
                    shows(view_of(&picked), "Delivering to Paris"),
                ),
            ],
            json!({"type":outcome(&typed),"suggestion_already_in_view":in_view.is_some(),"pick":outcome(&picked),"observed_again":!after.is_null()}),
        );
    }
    // A card moves under another heading and says the same as before.
    if wanted("kanban") {
        let mut run = Run::open(&mut peer, &site.url("kanban")).await;
        let seen = run.observe(json!({})).await;
        let (second, third) = (
            control(&seen, "Move to Done", "Card K2"),
            control(&seen, "Move to Done", "Card K3"),
        );
        let moved = run.click(&second).await;
        let other = run.click(&third).await;
        let after = run.observe(json!({})).await;
        let cost = run.close().await;
        let writes = site.writes("kanban");
        record(
            "kanban",
            cost,
            writes.clone(),
            vec![
                (
                    "the view shows the moved card",
                    shows(view_of(&moved), "Card K2"),
                ),
                (
                    "the view says where the card went",
                    shows(view_of(&moved), "Done"),
                ),
                (
                    "a reference to another card still acts",
                    writes == ["K2", "K3"],
                ),
                (
                    "an observation shows both cards",
                    shows(&after, "Card K2") && shows(&after, "Card K3"),
                ),
            ],
            json!({"move":outcome(&moved),"other":outcome(&other),"view":view_of(&moved)["text"]}),
        );
    }
    // A header stays on top and covers what scrolls under it.
    if wanted("sticky") {
        let mut run = Run::open(&mut peer, &site.url("sticky")).await;
        let seen = run.observe(json!({"filter":"interactive"})).await;
        let far = run.click(&control(&seen, "Approve", "Order S45")).await;
        let near = run.click(&control(&seen, "Approve", "Order S44")).await;
        let back = run.click(&control(&seen, "Approve", "Order S02")).await;
        let cost = run.close().await;
        let writes = site.writes("sticky");
        record(
            "sticky",
            cost,
            writes.clone(),
            vec![(
                "rows far down, just above and back at the top are all clicked",
                writes == ["S45", "S44", "S02"],
            )],
            json!({"far":outcome(&far),"near":outcome(&near),"back":outcome(&back)}),
        );
    }
    // Reaching the end of the page loads more rows.
    if wanted("lazy") {
        let mut run = Run::open(&mut peer, &site.url("lazy")).await;
        let seen = run.observe(json!({"filter":"interactive"})).await;
        let before = shows(&seen, "Order L35");
        let scrolled = run.act(json!({"kind":"scroll","delta_y":1500})).await;
        let mut target = find_control(view_of(&scrolled), "Approve", "Order L35");
        let from_view = target.is_some();
        if target.is_none() {
            let after = run.observe(json!({"find":"Order L35"})).await;
            target = find_control(&after, "Approve", "Order L35");
        }
        let acted = run.click(target.as_ref().unwrap_or(&Value::Null)).await;
        let cost = run.close().await;
        let writes = site.writes("lazy");
        record(
            "lazy",
            cost,
            writes.clone(),
            vec![
                ("the row is not there before scrolling", !before),
                ("the view after scrolling lists the loaded rows", from_view),
                ("the loaded row is written", writes == ["L35"]),
            ],
            json!({"scroll":outcome(&scrolled),"act":outcome(&acted),"scroll_view_bytes":view_of(&scrolled).to_string().len()}),
        );
    }
    // One row among two thousand.
    if wanted("big") {
        let mut run = Run::open(&mut peer, &site.url("big")).await;
        let started = std::time::Instant::now();
        let mut found = run.observe(json!({"find":"Order G1500"})).await;
        let mut reads = 1;
        while find_control(&found, "Approve", "Order G1500").is_none() && reads < 12 {
            let Some(cursor) = found["next_cursor"].as_str().map(str::to_owned) else {
                break;
            };
            found = run.observe(json!({"cursor":cursor})).await;
            reads += 1;
        }
        let find_ms = started.elapsed().as_millis();
        let target = find_control(&found, "Approve", "Order G1500");
        let acted = run.click(target.as_ref().unwrap_or(&Value::Null)).await;
        let cost = run.close().await;
        let writes = site.writes("big");
        record(
            "big",
            cost,
            writes.clone(),
            vec![
                (
                    "the row is found by one search call",
                    reads == 1 && target.is_some(),
                ),
                (
                    "the view shows only the changed row",
                    shows(view_of(&acted), "Approved") && view_of(&acted).to_string().len() < 1000,
                ),
                ("the requested order is written", writes == ["G1500"]),
            ],
            json!({"search_calls":reads,"find_ms":find_ms,"act":outcome(&acted)}),
        );
    }

    let report = Value::Object(report);
    if let Ok(path) = std::env::var("RDH_SCENARIO_REPORT") {
        fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    println!("SCENARIOS {report}");
    assert!(failures.is_empty(), "{failures:#?}");
}
