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
    async fn call(&mut self, operation: &str, params: Value) -> Value {
        let receipt = rdh_agent::browser::MUTATIONS
            .contains(&operation)
            .then(|| random::hex_token::<16>().unwrap());
        self.raw(operation, params, Some(&"a".repeat(64)), receipt.as_deref())
            .await
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

/// A page, the steps after opening it, what the site must have been sent and
/// what the reply must say.
type Task<'a> = (&'a str, Vec<Value>, Vec<&'a str>, Vec<&'a str>);

/// The scenario pages through the interface an AI works with: each task is a
/// list of steps in one call, aimed at what the controls are called.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, python3 and a sandbox-capable native host"]
async fn front_pool() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-front-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"executable":executable}),
    );
    let site = Site::start();
    let only = std::env::var("RDH_SCENARIO_ONLY").ok();
    let approve = |order: &str| json!({"click":{"name":"Approve","in":order}});
    let click = |name: &str| json!({"click":{"name":name}});
    let ready = json!({"wait":{"text":"Orders are ready"}});
    let tasks: Vec<Task> = vec![
        (
            "recycle",
            vec![
                json!({"scroll":400,"in":{"name":"Order list"}}),
                approve("Order 0012"),
            ],
            vec!["12"],
            vec![],
        ),
        (
            "rerender",
            vec![
                json!({"click":{"name":"Remove","in":"Item 2"}}),
                json!({"click":{"name":"Remove","in":"Item 4"}}),
            ],
            vec!["2", "4"],
            vec![],
        ),
        (
            "css-state",
            vec![
                json!({"check":{"name":"Show advanced"}}),
                json!({"select":{"name":"Plan"},"value":"pro"}),
            ],
            vec![],
            vec!["Reset everything", "20"],
        ),
        (
            "modal",
            vec![
                json!({"click":{"name":"Delete","in":"Order B42"}}),
                click("Confirm"),
            ],
            vec!["B42"],
            vec!["Deleted B42"],
        ),
        (
            "optimistic",
            vec![approve("Order A17"), json!({"wait":{"text":"Rejected"}})],
            vec!["A17"],
            vec!["Rejected: limit exceeded"],
        ),
        (
            "spa",
            vec![click("Orders"), approve("Order S03")],
            vec!["S03"],
            vec![],
        ),
        (
            "ticker",
            vec![approve("Order T02")],
            vec!["Order T02"],
            vec![],
        ),
        (
            "dependent",
            vec![
                json!({"select":{"name":"Country"},"value":"jp"}),
                json!({"select":{"name":"City"},"value":"Osaka"}),
                click("Save address"),
            ],
            vec!["jp/Osaka"],
            vec![],
        ),
        (
            "sort",
            vec![click("Sort by amount"), approve("Invoice Q3")],
            vec!["Q3"],
            vec![],
        ),
        (
            "pager",
            vec![click("Next page"), approve("Order P07")],
            vec!["P07"],
            vec![],
        ),
        ("feed", vec![approve("Order F1")], vec!["F1"], vec![]),
        (
            "worker",
            vec![approve("Order W1")],
            vec!["W1"],
            vec!["Order W1 approved"],
        ),
        (
            "reload",
            vec![ready.clone(), approve("Order L1")],
            vec!["L1"],
            vec![],
        ),
        (
            "churn",
            vec![ready.clone(), approve("Order C1")],
            vec!["C1"],
            vec![],
        ),
        (
            "shadow",
            vec![approve("Order K2"), approve("Order K3")],
            vec!["K2", "K3"],
            vec![],
        ),
        (
            "xframe",
            vec![json!({"wait":{"text":"Order X2"}}), approve("Order X2")],
            vec!["X2"],
            vec![],
        ),
        (
            "toplayer",
            vec![click("Open settings"), click("Save settings")],
            vec!["save"],
            vec![],
        ),
        (
            "inline",
            vec![
                json!({"click":{"name":"Edit","in":"R2"}}),
                json!({"fill":{"name":"Name for R2"},"text":"Zed"}),
                click("Save"),
            ],
            vec!["R2=Zed"],
            vec!["Zed"],
        ),
        (
            "wizard",
            vec![
                json!({"fill":{"name":"Email"},"text":"a@b.test"}),
                click("Next"),
                json!({"select":{"name":"Plan"},"value":"pro"}),
                click("Next"),
                click("Create account"),
            ],
            vec!["a@b.test/pro"],
            vec!["Account created"],
        ),
        (
            "autocomplete",
            vec![
                json!({"fill":{"name":"City"},"text":"Pa"}),
                json!({"wait":{"text":"Paris"}}),
                click("Paris"),
            ],
            vec!["Paris"],
            vec!["Delivering to Paris"],
        ),
        (
            "kanban",
            vec![
                json!({"click":{"name":"Move to Done","in":"Card K2"}}),
                json!({"click":{"name":"Move to Done","in":"Card K3"}}),
            ],
            vec!["K2", "K3"],
            vec![],
        ),
        (
            "sticky",
            vec![
                approve("Order S45"),
                approve("Order S44"),
                approve("Order S02"),
            ],
            vec!["S45", "S44", "S02"],
            vec![],
        ),
        (
            "lazy",
            vec![
                json!({"scroll":1500}),
                json!({"wait":{"text":"Order L35"}}),
                approve("Order L35"),
            ],
            vec!["L35"],
            vec![],
        ),
        ("big", vec![approve("Order G1500")], vec!["G1500"], vec![]),
    ];
    let mut report = serde_json::Map::new();
    let mut failures = Vec::new();
    for (name, after, writes, says) in tasks {
        if only
            .as_deref()
            .is_some_and(|list| !list.split(',').any(|item| item == name))
        {
            continue;
        }
        let mut steps = vec![json!({"open":site.url(name)})];
        steps.extend(after);
        let started = std::time::Instant::now();
        let reply = peer.call("browser_do", json!({"steps":steps})).await;
        let elapsed = started.elapsed().as_millis() as u64;
        let text = reply["result"]["text"].as_str().unwrap_or("").to_owned();
        let closed = peer
            .call(
                "browser_do",
                json!({"steps":[{"close":true}],"read":"none"}),
            )
            .await;
        if let Some(path) = std::env::var_os("RDH_SCENARIO_DUMP") {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(
                file,
                "=== {name} [{elapsed} ms]\n{text}\n{}",
                if text.is_empty() {
                    reply.to_string()
                } else {
                    String::new()
                }
            )
            .unwrap();
        }
        let written = site.writes(name);
        let mut checks = vec![
            (
                "every step ran",
                reply["result"]["failed"].is_null() && reply.get("error").is_none(),
            ),
            (
                "the site was sent what the task asked for",
                written == writes,
            ),
            ("the session closes", closed.get("error").is_none()),
        ];
        for expected in &says {
            checks.push(("the reply says what happened", text.contains(expected)));
        }
        for (check, held) in &checks {
            if !held {
                let lines: Vec<&str> = text
                    .lines()
                    .filter(|line| !line.starts_with(['<', ' ']))
                    .take(12)
                    .collect();
                failures.push(format!(
                    "{name}: {check} (writes {written:?}, {})\n{}",
                    reply["error_code"],
                    lines.join("\n")
                ));
            }
        }
        report.insert(
            name.into(),
            json!({"calls":2,"bytes":text.len(),"ms":elapsed,"writes":written,
                "ok":checks.iter().all(|(_, held)| *held)}),
        );
    }
    // A question the browser asks ends the call that raised it; the answer is
    // the caller's, and the steps after the answer run as usual.
    if only
        .as_deref()
        .is_none_or(|list| list.split(',').any(|item| item == "confirm"))
    {
        let delete = |order: &str| json!({"click":{"name":"Delete","in":order}});
        let mut said = Vec::new();
        for steps in [
            json!([{"open":site.url("confirm")}, delete("Order D1"), delete("Order D2")]),
            json!([delete("Order D2")]),
            json!([{"dialog":"accept"}, delete("Order D2")]),
            json!([{"dialog":"dismiss"}, {"fill":{"name":"Search"},"text":"abc"},
                {"press":"Enter","on":{"name":"Search"}}]),
            json!([{"close":true}]),
        ] {
            let reply = peer.call("browser_do", json!({"steps":steps})).await;
            said.push(reply["result"]["text"].as_str().unwrap_or("").to_owned());
        }
        let written = site.writes("confirm");
        let checks = [
            (
                "the step that raised the dialog ends there and says what it asks",
                said[0].contains("a confirm dialog opened: \"Delete order D1?\"")
                    && said[0].contains("not run: 3–3"),
            ),
            (
                "another step is refused at once while the dialog is open",
                said[1].contains("browser_dialog_open"),
            ),
            (
                "after the answer the next step runs and raises the next dialog",
                said[2].contains("dialog accept") && said[2].contains("Delete order D2?"),
            ),
            (
                "a dismissed dialog leaves the session usable",
                said[3].contains("Searched for abc"),
            ),
            ("only the confirmed order is deleted", written == ["D1"]),
        ];
        for (check, held) in &checks {
            if !held {
                failures.push(format!(
                    "confirm: {check} (writes {written:?})\n{}",
                    said.join("\n--\n")
                ));
            }
        }
        report.insert(
            "confirm".into(),
            json!({"calls":said.len(),"bytes":said.iter().map(String::len).sum::<usize>(),
                "writes":written,"ok":checks.iter().all(|(_, held)| *held)}),
        );
    }
    if let Ok(path) = std::env::var("RDH_SCENARIO_REPORT") {
        fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    for (name, entry) in &report {
        println!("{name}: {entry}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

/// What a person opens the developer tools for comes with the reply of the
/// step that caused it, and the rest can be asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, python3 and a sandbox-capable native host"]
async fn front_diagnostics() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-front-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"executable":executable}),
    );
    let site = Site::start();
    async fn said(peer: &mut Peer, operation: &str, params: Value) -> String {
        let reply = peer.call(operation, params).await;
        reply["result"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{reply}"))
            .to_owned()
    }
    let opened = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"open":site.url("diagnostics")},{"wait":{"text":"Late notice"}}]}),
    )
    .await;
    assert!(opened.contains("\n  problems:\n"), "{opened}");
    assert!(opened.contains(" 404 GET /missing.js script "), "{opened}");
    assert!(
        opened.contains(" error config missing Object ("),
        "{opened}"
    );
    assert!(
        !opened.contains("favicon") && !opened.contains("orders loaded"),
        "{opened}"
    );
    assert!(opened.contains("--- page\nDiagnostics"), "{opened}");

    let saved = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"click":{"name":"Save order"}},{"wait":{"text":"Save failed"}}]}),
    )
    .await;
    assert!(saved.contains(" 500 POST /fail fetch"), "{saved}");
    assert!(
        saved.contains(" error Uncaught (in promise) TypeError: order is not saved"),
        "{saved}"
    );
    // A problem is told once.
    assert!(
        !saved.contains("missing.js") && !saved.contains("config missing"),
        "{saved}"
    );

    let quiet = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"click":{"name":"Refresh"}}]}),
    )
    .await;
    assert!(
        quiet.contains("Refreshed") && !quiet.contains("problems"),
        "{quiet}"
    );

    let failed = said(
        &mut peer,
        "browser_read",
        json!({"what":"network","filter":"failed"}),
    )
    .await;
    assert!(failed.starts_with("network: "), "{failed}");
    assert!(
        failed.contains("/missing.js") && !failed.contains(" document"),
        "{failed}"
    );
    let entry = failed
        .lines()
        .find(|line| line.contains("POST /fail"))
        .and_then(|line| line.split(' ').next())
        .unwrap_or_else(|| panic!("{failed}"))
        .to_owned();
    let detail = said(
        &mut peer,
        "browser_read",
        json!({"what":"network","find":entry}),
    )
    .await;
    assert!(detail.contains("/fail → 500"), "{detail}");
    assert!(
        detail.contains("\nstarted by http://127.0.0.1:"),
        "{detail}"
    );
    assert!(
        // What the server answered a failed request with says why it failed.
        detail.ends_with("\nresponse body:\n{\"error\":\"limit exceeded\"}"),
        "{detail}"
    );
    assert!(
        detail.contains("authorization: (not kept)") && !detail.contains("scenario-secret"),
        "{detail}"
    );
    let every = said(&mut peer, "browser_read", json!({"what":"network"})).await;
    assert!(
        every.contains(" 200 GET /diagnostics.html document"),
        "{every}"
    );

    let console = said(&mut peer, "browser_read", json!({"what":"console"})).await;
    assert!(
        console.contains(" log orders loaded") && console.contains(" error config missing"),
        "{console}"
    );
    let errors = said(
        &mut peer,
        "browser_read",
        json!({"what":"console","filter":"errors","find":"saved"}),
    )
    .await;
    assert!(
        errors.contains("order is not saved")
            && !errors.contains("orders loaded")
            && !errors.contains("config missing"),
        "{errors}"
    );
    let entry = errors
        .lines()
        .nth(1)
        .and_then(|line| line.split(' ').next())
        .unwrap()
        .to_owned();
    let thrown = said(
        &mut peer,
        "browser_read",
        json!({"what":"console","find":entry}),
    )
    .await;
    assert!(thrown.contains("\n    at "), "{thrown}");

    let performance = said(&mut peer, "browser_read", json!({"what":"performance"})).await;
    assert!(
        performance.starts_with("performance: http://127.0.0.1:")
            && performance.contains("\ndocument: first byte after ")
            && performance.contains(", loaded after ")
            && performance.contains("\nslowest:\n  n")
            && performance.contains(" GET /diagnostics.html document "),
        "{performance}"
    );
    // What the page measured of itself: painting, shifting and blocking.
    assert!(
        performance.contains("\npaint: first content after ")
            && performance.contains(", largest content after ")
            && performance.contains("\nlayout shift: 0.")
            && performance.contains(" in 1, the largest 0.")
            && performance.contains(" (body)\n")
            && performance.contains("\nlong tasks: ")
            && performance.contains(", the longest 1"),
        "{performance}"
    );
    // How an element is drawn, and what is in the way of a click on it.
    let styles = said(
        &mut peer,
        "browser_read",
        json!({"what":"styles","find":"Save order"}),
    )
    .await;
    assert!(
        styles.starts_with(
            "styles: 1 element with the text \"Save order\"\nbutton \"Save order\" — "
        ),
        "{styles}"
    );
    assert!(
        styles.contains("viewport; on top at its centre\n  ")
            && styles.contains("display: inline-block")
            && styles.contains("background-color: rgb(")
            && !styles.contains("opacity")
            && !styles.contains("width:"),
        "{styles}"
    );
    let covered = said(
        &mut peer,
        "browser_read",
        json!({"what":"styles","find":"Hidden offer"}),
    )
    .await;
    assert!(
        covered.contains("covered at its centre by div \"Notice\"")
            && covered.contains("opacity: 0.5"),
        "{covered}"
    );
    let none = said(
        &mut peer,
        "browser_read",
        json!({"what":"styles","find":"No such text"}),
    )
    .await;
    assert!(
        none.starts_with("styles: no element has the text"),
        "{none}"
    );
    // A step that opens a tab is followed there, and the other tabs stay
    // within reach without moving to them.
    async fn current(peer: &mut Peer) -> (String, usize) {
        let tabs = said(peer, "browser_read", json!({"what":"tabs"})).await;
        let on = tabs
            .lines()
            .find(|line| line.starts_with('*'))
            .and_then(|line| line.split(' ').nth(1))
            .unwrap_or_else(|| panic!("{tabs}"))
            .to_owned();
        // A tab is written by the start of its ID, and taken by it.
        assert!(on.len() == 6, "{tabs}");
        (on, tabs.lines().count())
    }
    let (old, _) = current(&mut peer).await;
    let followed = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"click":{"name":"Open report"}}]}),
    )
    .await;
    assert!(
        followed.contains(&format!(
            "\n  a new tab opened and is worked on now; go back with {{\"tab\":\"{old}\"}}\n"
        )) && followed.contains("--- page\n")
            && followed.contains("/modal.html"),
        "{followed}"
    );
    let (new, count) = current(&mut peer).await;
    assert!(new != old && count == 2);
    let other = said(&mut peer, "browser_read", json!({"tab":old})).await;
    assert!(other.starts_with("Diagnostics — "), "{other}");
    let seen = peer.call("browser_see", json!({"tab":old})).await;
    assert!(seen.get("error").is_none(), "{seen}");
    assert_eq!(current(&mut peer).await.0, new);
    let unrecorded = said(
        &mut peer,
        "browser_read",
        json!({"what":"console","tab":old}),
    )
    .await;
    assert!(
        unrecorded.starts_with("console: only the tab that is worked on"),
        "{unrecorded}"
    );
    // Steps for a named tab are carried out there.
    let back = said(
        &mut peer,
        "browser_do",
        json!({"tab":old,"steps":[{"click":{"name":"Refresh"}}]}),
    )
    .await;
    assert!(back.contains("1 click button \"Refresh\""), "{back}");
    assert_eq!(current(&mut peer).await.0, old);
    let closed = said(
        &mut peer,
        "browser_do",
        json!({"tab":new,"steps":[{"tab":"close"}],"read":"none"}),
    )
    .await;
    assert!(closed.starts_with("1 tab close (now on "), "{closed}");
    assert_eq!(current(&mut peer).await, (old.clone(), 1));
    // The journal follows the tab to another document, and starts over there.
    let moved = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"open":site.url("modal")}],"read":"none"}),
    )
    .await;
    assert!(!moved.contains("problems"), "{moved}");
    let console = said(&mut peer, "browser_read", json!({"what":"console"})).await;
    assert!(
        console.starts_with("console: 0 of 0 lines since http://127.0.0.1:"),
        "{console}"
    );
    assert!(console.contains("/modal.html loaded"), "{console}");

    // A second session is asked for by name, and then each is named.
    let second = said(
        &mut peer,
        "browser_do",
        json!({"session":"new","steps":[{"open":site.url("diagnostics")}],"read":"none"}),
    )
    .await;
    assert!(second.contains("/missing.js"), "{second}");
    let sessions = said(&mut peer, "browser_read", json!({"what":"sessions"})).await;
    assert_eq!(sessions.lines().count(), 2, "{sessions}");
    // The tabs of every session are listed when none is named.
    let everywhere = said(&mut peer, "browser_read", json!({"what":"tabs"})).await;
    assert_eq!(
        everywhere
            .lines()
            .filter(|line| line.starts_with("session "))
            .count(),
        2,
        "{everywhere}"
    );
    assert_eq!(
        everywhere
            .lines()
            .filter(|line| line.starts_with('*'))
            .count(),
        2,
        "{everywhere}"
    );
    let unnamed = peer
        .call("browser_do", json!({"steps":[{"close":true}]}))
        .await;
    assert_eq!(
        unnamed["error_code"], "browser_session_ambiguous",
        "{unnamed}"
    );
    for line in sessions.lines() {
        let short = line.split(' ').next().unwrap();
        let closed = said(
            &mut peer,
            "browser_do",
            json!({"session":short,"steps":[{"close":true}]}),
        )
        .await;
        assert_eq!(closed, "1 close");
    }
}

/// A reference from an earlier reply still names its element after the page
/// has redrawn it, and names nothing once that element is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, python3 and a sandbox-capable native host"]
async fn front_references() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-front-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"executable":executable}),
    );
    let site = Site::start();
    async fn said(peer: &mut Peer, operation: &str, params: Value) -> String {
        let reply = peer.call(operation, params).await;
        reply["result"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{reply}"))
            .to_owned()
    }
    /// The reference of the control called `name` under the line with `row`.
    fn reference(page: &str, row: &str, name: &str) -> String {
        let mut lines = page.lines().skip_while(|line| !line.contains(row));
        lines
            .find_map(|line| {
                let (head, tail) = line.split_once("]: ")?;
                (tail.trim() == name).then(|| head.rsplit_once('[').unwrap().1.to_owned())
            })
            .unwrap_or_else(|| panic!("no {name} under {row} in\n{page}"))
    }

    // Removing an item redraws the whole list: every reference of the first
    // reading is dead to the browser, and each still finds its button.
    let page = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"open":site.url("rerender")}]}),
    )
    .await;
    let (two, four) = (
        reference(&page, "Item 2", "Remove"),
        reference(&page, "Item 4", "Remove"),
    );
    let removed = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"click":two},{"click":four}]}),
    )
    .await;
    assert!(!removed.contains('✗'), "{removed}");
    assert!(
        removed.contains(&format!("(was [{four}], now [")),
        "{removed}"
    );
    assert_eq!(site.writes("rerender"), ["2", "4"]);
    // The item is gone, and so is what its reference meant.
    let gone = said(&mut peer, "browser_do", json!({"steps":[{"click":two}]})).await;
    assert!(
        gone.starts_with("✗ 1 click: browser_stale_reference")
            && gone.contains("is no longer on the page"),
        "{gone}"
    );
    assert_eq!(site.writes("rerender").len(), 2);

    // The next page shows other orders in the same rows. A reference to an
    // order of the first page does not pass to the order that took its row.
    let page = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"open":site.url("pager")}]}),
    )
    .await;
    let (next, second) = (
        reference(&page, "", "Next page"),
        reference(&page, "Order P02", "Approve"),
    );
    let turned = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"click":next},{"click":second}]}),
    )
    .await;
    assert!(
        turned.contains("\n✗ 2 click: browser_stale_reference") && turned.contains("Order P02"),
        "{turned}"
    );
    assert!(site.writes("pager").is_empty());
    // Back on its page the order is there again, and the reference with it.
    let back = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"click":{"name":"Previous page"}},{"click":second}]}),
    )
    .await;
    if back.contains("browser_target_not_found") {
        // The page has no way back; nothing more to check here.
    } else {
        assert!(!back.contains('✗'), "{back}");
        assert_eq!(site.writes("pager"), ["P02"]);
    }
    let closed = said(&mut peer, "browser_do", json!({"steps":[{"close":true}]})).await;
    assert_eq!(closed, "1 close");
}

/// Carries out the calls of a file against a real browser and prints every
/// reply with its size and time: the way to try the interface on a site by
/// hand. RDH_EXPLORE names the file, a JSON array of [operation, params];
/// ["sleep", {"ms": 500}] lets time pass between two calls. RDH_EXPLORE_PROXY
/// names the proxy the browser is to ask through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, RDH_EXPLORE and a sandbox-capable native host"]
async fn front_explore() {
    // Without a file there is nothing to try; the other tests of this name run.
    let Ok(file) = std::env::var("RDH_EXPLORE") else {
        return;
    };
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let calls: Vec<(String, Value)> =
        serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-front-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let mut peer = policy_peer(
        &temp.0,
        match std::env::var("RDH_EXPLORE_PROXY") {
            // A device that reaches other sites only through a proxy.
            Ok(proxy) => json!({"enabled":true,"capability_v2":true,"executable":executable,
                "proxy":{"server":proxy}}),
            Err(_) => json!({"enabled":true,"capability_v2":true,"executable":executable}),
        },
    );
    for (operation, params) in calls {
        if operation == "sleep" {
            let pause = params["ms"].as_u64().unwrap_or(0);
            tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
            continue;
        }
        let started = std::time::Instant::now();
        let reply = peer.call(&operation, params.clone()).await;
        let text = match reply["result"]["text"].as_str() {
            Some(text) => text.to_owned(),
            None => reply.to_string(),
        };
        println!(
            "=== {operation} {params}\n--- {} bytes, {} ms\n{text}",
            text.len(),
            started.elapsed().as_millis()
        );
    }
}

/// A long page is read in parts that follow one another, a search reads only
/// what holds the words, and a frame from another site is read with the page
/// around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, python3 and a sandbox-capable native host"]
async fn front_reading() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-front-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"executable":executable}),
    );
    let site = Site::start();
    async fn said(peer: &mut Peer, operation: &str, params: Value) -> String {
        let reply = peer.call(operation, params.clone()).await;
        reply["result"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{params}: {reply}"))
            .to_owned()
    }
    fn orders(text: &str) -> Vec<&str> {
        text.match_indices("Order G")
            .filter_map(|(at, _)| text.get(at + 6..at + 11))
            .collect()
    }
    fn cursor(text: &str) -> Option<String> {
        let rest = text.lines().last()?.strip_prefix("more: ")?;
        Some(rest.split(' ').next()?.to_owned())
    }

    said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"open":site.url("big")}],"read":"none"}),
    )
    .await;
    let first = said(&mut peer, "browser_read", json!({"what":"page"})).await;
    assert!(first.starts_with("Two thousand rows — http://"), "{first}");
    assert!(
        first.lines().last().unwrap().ends_with("% read)"),
        "{first}"
    );
    let second = said(
        &mut peer,
        "browser_read",
        json!({"what":"page","more":cursor(&first).unwrap()}),
    )
    .await;
    let (one, two) = (orders(&first), orders(&second));
    assert_eq!(one.first(), Some(&"G0001"), "{first}");
    assert!(
        !two.is_empty() && one.last() < two.first() && cursor(&second).is_some(),
        "{:?} then {:?}",
        one.last(),
        two.first()
    );
    let found = said(
        &mut peer,
        "browser_read",
        json!({"what":"page","find":"Order G1500"}),
    )
    .await;
    assert_eq!(orders(&found), ["G1500"], "{found}");
    assert!(found.len() < 400 && cursor(&found).is_none(), "{found}");
    let short = said(
        &mut peer,
        "browser_read",
        json!({"what":"page","max_chars":600}),
    )
    .await;
    assert!(
        short.len() < first.len() && cursor(&short).is_some(),
        "{short}"
    );

    // The frame's content reads as part of the page, and is searched with it.
    let framed = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"open":site.url("xframe")},{"wait":{"text":"Order X2"}}]}),
    )
    .await;
    assert!(!framed.contains('✗'), "{framed}");
    let whole = said(&mut peer, "browser_read", json!({"what":"page"})).await;
    assert!(
        whole.contains("Summary: 2 open")
            && whole.contains("Order X1")
            && whole.contains("Order X2"),
        "{whole}"
    );
    let inner = said(
        &mut peer,
        "browser_read",
        json!({"what":"page","find":"Order X2"}),
    )
    .await;
    assert!(
        inner.contains("Order X2") && !inner.contains("Order X1") && !inner.contains("Summary"),
        "{inner}"
    );

    // Asked for by its words, something that is no control is listed with a
    // reference to act on.
    said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"open":site.url("cards")}],"read":"none"}),
    )
    .await;
    let worded = said(
        &mut peer,
        "browser_read",
        json!({"what":"page","filter":"text","find":"Quiet row 14"}),
    )
    .await;
    let line = worded
        .lines()
        .find(|line| line.ends_with(": Quiet row 14"))
        .unwrap_or_else(|| panic!("{worded}"));
    let reference = line.split(['[', ']']).nth(1).unwrap();
    let clicked = said(
        &mut peer,
        "browser_do",
        json!({"steps":[{"click":reference}]}),
    )
    .await;
    assert!(clicked.contains("Opened quiet row 14"), "{clicked}");
}

/// A forwarding proxy that passes names under `.test` to this machine and
/// keeps a list of every host it was asked for.
struct Proxy {
    child: std::process::Child,
    port: u16,
    log: PathBuf,
}
impl Proxy {
    fn start(log: PathBuf) -> Self {
        use std::io::BufRead;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/browser-scenarios/proxy.py");
        let mut child = std::process::Command::new("python3")
            .arg(script)
            .arg(port.to_string())
            .arg(&log)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3 runs the scenario proxy");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("ready"), "{line}");
        Self { child, port, log }
    }
    /// The hosts the proxy was asked for on behalf of pages, in order. A
    /// branded Chrome asks once at start which accounts are signed in, and no
    /// switch found so far keeps it from that; nothing else of its own is let by.
    fn asked(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|host| *host != "accounts.google.com")
            .map(str::to_owned)
            .collect()
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A device that reaches other sites only through a proxy: the browser asks
/// the proxy, the site rules are applied before it does, and what the owner
/// lists is reached directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, python3 and a sandbox-capable native host"]
async fn front_proxy() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-front-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let site = Site::start();
    let proxy = Proxy::start(temp.0.join("proxy.log"));
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"executable":executable,
            "proxy":{"server":format!("http://127.0.0.1:{}", proxy.port),"bypass":[".direct.test"]},
            "sites":{"deny":["blocked.test"]}}),
    );
    async fn did(peer: &mut Peer, steps: Value) -> String {
        let reply = peer.call("browser_do", json!({"steps":steps})).await;
        reply["result"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{reply}"))
            .to_owned()
    }
    // A name that resolves nowhere on this device is the proxy's to find.
    let through = format!("http://shop.test:{}/cards.html", site.port);
    let opened = did(&mut peer, json!([{"open":through}])).await;
    assert!(
        opened.starts_with("1 open http://shop.test:") && opened.contains("heading: Orders"),
        "{opened}"
    );
    let clicked = did(&mut peer, json!([{"click":{"name":"Order 12"}}])).await;
    assert!(clicked.contains("Opened order 12"), "{clicked}");
    assert!(
        proxy.asked().iter().all(|host| host == "shop.test"),
        "{:?}",
        proxy.asked()
    );
    assert!(!proxy.asked().is_empty());
    // What the owner refuses, and the addresses no session may reach, are
    // refused before the proxy hears of them.
    for refused in [
        format!("http://blocked.test:{}/cards.html", site.port),
        "http://169.254.169.254/latest/meta-data/".to_owned(),
    ] {
        let denied = did(&mut peer, json!([{"open":refused}])).await;
        assert!(
            denied.starts_with("✗ 1 open: browser_site_denied"),
            "{refused}: {denied}"
        );
    }
    // A host the owner lists is asked for directly, where its name leads
    // nowhere; this device itself is always reached directly.
    let direct = did(
        &mut peer,
        json!([{"open":format!("http://app.direct.test:{}/cards.html", site.port)}]),
    )
    .await;
    assert!(
        direct.starts_with(
            "✗ 1 open: browser_navigation_failed\n  the host name leads to no address"
        ),
        "{direct}"
    );
    let local = did(&mut peer, json!([{"open":site.url("cards")}])).await;
    assert!(local.contains("heading: Orders"), "{local}");
    assert!(
        proxy.asked().iter().all(|host| host == "shop.test"),
        "{:?}",
        proxy.asked()
    );
    did(&mut peer, json!([{"close":true}])).await;

    // A proxy that is not there is the reason the page did not load.
    drop(proxy);
    let unreachable = did(&mut peer, json!([{"open":through}])).await;
    assert!(
        unreachable.starts_with("✗ 1 open: browser_navigation_failed")
            && unreachable.contains("\n  net::ERR_PROXY_CONNECTION_FAILED"),
        "{unreachable}"
    );
}

/// Pages as older sites write them: boxes named only by the words beside
/// them, a list with no label, a caption a style rule shows on hover, boxes
/// that are dragged onto one another, and a page that fails to load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, python3 and a sandbox-capable native host"]
async fn front_plain() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-front-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"executable":executable}),
    );
    let site = Site::start();
    async fn did(peer: &mut Peer, steps: Value) -> String {
        let reply = peer.call("browser_do", json!({"steps":steps})).await;
        reply["result"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{reply}"))
            .to_owned()
    }
    let opened = did(&mut peer, json!([{"open":site.url("plain")}])).await;
    assert!(opened.contains(" unchecked: first box\n"), "{opened}");
    // A label is its field's name, not a second thing to click; a menu item
    // that only wraps a link is that link; a container is not called by
    // what the controls in it say.
    assert!(
        opened.contains(": Username\n") && !opened.contains("tag=label: Username"),
        "{opened}"
    );
    assert!(
        opened.contains(": Reports\n") && !opened.contains("menuitem ["),
        "{opened}"
    );
    assert!(!opened.contains("Reports Yearly"), "{opened}");
    // A box is also named by a label that merely follows it. A picture that
    // is all a link holds is listed once, as the link; a link that says
    // nothing is called by the titled box in it; and a list item that holds
    // one link is one line.
    assert!(opened.contains(" unchecked: water plants\n"), "{opened}");
    assert!(
        opened.contains(" href=#logo: Home logo\n") && !opened.contains("img alt=\"Home logo\""),
        "{opened}"
    );
    assert!(opened.contains(" href=#vote: upvote\n"), "{opened}");
    assert!(
        opened.contains(" href=#first: First section\n")
            && !opened.contains("<li> <a href=#first>"),
        "{opened}"
    );
    // Words beside the link stay: such an item keeps its own line.
    assert!(
        opened
            .lines()
            .any(|line| line.starts_with("<li> <a href=#top>") && line.ends_with(" 10"))
            && opened.contains(": Top 10 books\n"),
        "{opened}"
    );
    for (step, said) in [
        (
            json!({"check":{"name":"second box"},"on":false}),
            vec!["1 check checkbox \"second box\"", " unchecked: second box"],
        ),
        // The only list on the page needs no name.
        (
            json!({"select":{"role":"combobox"},"value":"2"}),
            vec!["1 select combobox\n", " value=2"],
        ),
        // A picture is hovered by what it is called, and what a style rule
        // shows beside it then is part of the change.
        (
            json!({"hover":{"name":"Portrait"}}),
            vec!["1 hover ", "name: user1", ": View profile"],
        ),
        // Boxes that are sorted by dragging are dropped on one another.
        (
            json!({"drag":{"name":"Alpha"},"to":{"name":"Beta"}}),
            vec!["1 drag ", "Swapped"],
        ),
        (json!({"click":{"name":"Add row"}}), vec![": Delete row"]),
        (
            json!({"click":{"name":"Delete row"}}),
            vec!["(something was removed from the page; nothing else changed)"],
        ),
        (
            json!({"click":{"name":"Reports"}}),
            vec!["1 click link \"Reports\""],
        ),
        // A click is one press of the main button unless the step says more.
        (
            json!({"click":{"name":"Editable cell"}}),
            vec!["1 click ", "Cell chosen"],
        ),
        (
            json!({"click":{"name":"Editable cell"},"count":2}),
            vec!["1 double-click ", "Editing cell"],
        ),
        (
            json!({"click":{"name":"Menu box"},"button":"right"}),
            vec!["1 right-click ", "Menu shown"],
        ),
        // A button whose face is drawn by a pseudo-element of its own.
        (
            json!({"click":{"name":"Close panel"}}),
            vec!["1 click button \"Close panel\"", "Panel closed"],
        ),
        // A click with the other button that the page does nothing with
        // leaves nothing in the way of the next step.
        (
            json!({"click":{"name":"Add row"},"button":"right"}),
            vec!["1 right-click button \"Add row\""],
        ),
        (json!({"click":{"name":"Add row"}}), vec![": Delete row"]),
        (
            json!({"click":{"name":"Delete row","nth":1}}),
            vec!["1 click button \"Delete row\""],
        ),
        // The role "text" asks for the words, where a control is called the same.
        (
            json!({"click":{"name":"water plants","role":"text"},"count":2}),
            vec!["1 double-click inferred \"water plants\"", "Editing plants"],
        ),
        (
            json!({"check":{"name":"water plants"}}),
            vec![
                "1 check checkbox \"water plants\"",
                " checked: water plants",
            ],
        ),
    ] {
        let done = did(&mut peer, json!([step])).await;
        assert!(
            !done.contains('✗') && said.iter().all(|part| done.contains(part)),
            "{step}: {done}"
        );
    }
    // Requests that fail alike are one line.
    let burst = did(
        &mut peer,
        json!([{"click":{"name":"Burst"}},{"wait":{"url":"plain"}}]),
    )
    .await;
    let burst = if burst.contains("more like it") {
        burst
    } else {
        did(&mut peer, json!([{"wait":{"url":"plain"}}])).await
    };
    assert!(
        burst.matches(" GET /fail?n=").count() == 1 && burst.contains(" more like it: n"),
        "{burst}"
    );
    // What the page reports between two calls is told ahead of the next
    // call's steps, not as the doing of its first.
    let late = did(&mut peer, json!([{"click":{"name":"Late"}}])).await;
    assert!(!late.contains("problems:"), "{late}");
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    let after = did(&mut peer, json!([{"click":{"name":"Add row"}}])).await;
    assert!(
        after.starts_with("before these steps\n  problems:\n    n")
            && after.contains(" GET /fail?late=1 fetch")
            && after.contains("\n1 click button \"Add row\"\n"),
        "{after}"
    );
    // A page is scrolled as far as is asked.
    let far = did(
        &mut peer,
        json!([{"scroll":3000},{"wait":{"text":"Scrolled far"}}]),
    )
    .await;
    assert!(!far.contains('✗'), "{far}");
    // A page that does not load says why, in an open session and in none.
    for _ in 0..2 {
        let unsafe_port = did(&mut peer, json!([{"open":"http://127.0.0.1:1/"}])).await;
        assert!(
            unsafe_port.starts_with("✗ 1 open: browser_navigation_failed")
                && unsafe_port.contains("\n  net::ERR_"),
            "{unsafe_port}"
        );
        let nowhere = did(&mut peer, json!([{"open":"http://no-such-host.invalid/"}])).await;
        assert!(
            nowhere.starts_with(
                "✗ 1 open: browser_navigation_failed\n  the host name leads to no address"
            ),
            "{nowhere}"
        );
        did(&mut peer, json!([{"close":true}])).await;
    }
}

/// Fields that take no typing, elements that take clicks without being
/// controls, and the names a label gives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE, python3 and a sandbox-capable native host"]
async fn front_controls() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-front-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"executable":executable}),
    );
    let site = Site::start();
    async fn did(peer: &mut Peer, steps: Value) -> String {
        let reply = peer.call("browser_do", json!({"steps":steps})).await;
        reply["result"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{reply}"))
            .to_owned()
    }

    // A date, a time, a colour and a slider are given their value by fill,
    // and the page hears of the change as it does from a person.
    let opened = did(&mut peer, json!([{"open":site.url("picked")}])).await;
    assert!(
        opened.contains(" value=2026-01-02 type=date: Day\n"),
        "{opened}"
    );
    for (name, value, heard) in [
        ("Day", "2026-03-04", "Changed day 2026-03-04"),
        ("Hour", "09:30", "Changed hour 09:30"),
        ("Tint", "#112233", "Changed tint #112233"),
        ("Level", "7", "Changed level 7"),
    ] {
        let filled = did(&mut peer, json!([{"fill":{"name":name},"text":value}])).await;
        assert!(
            filled.starts_with("1 fill ") && filled.contains(heard),
            "{name}: {filled}"
        );
    }
    for (name, value) in [("Day", "not a date"), ("Level", "70"), ("Tint", "red")] {
        let refused = did(&mut peer, json!([{"fill":{"name":name},"text":value}])).await;
        assert!(
            refused.starts_with("✗ 1 fill: browser_value_rejected — "),
            "{name}: {refused}"
        );
    }
    let kept = did(&mut peer, json!([{"wait":{"text":"Changed level 7"}}])).await;
    assert!(!kept.contains('✗'), "{kept}");

    // A card that takes clicks is clicked where it is the card's own, also
    // when a button of its own lies at its middle.
    let cards = did(&mut peer, json!([{"open":site.url("cards")}])).await;
    // The card is called by its own words, not by those of the button in it.
    assert!(cards.contains(" tag=div: Order 13\n"), "{cards}");
    for (step, heard) in [
        (json!({"click":{"name":"Order 12"}}), "Opened order 12"),
        (json!({"click":{"name":"Remove 13"}}), "Removed order 13"),
        (json!({"click":{"name":"Order 13"}}), "Opened order 13"),
        (json!({"press":"Enter","on":{"name":"Chip"}}), "Chip chosen"),
        // Nothing on the page says these take clicks: only a script listens.
        (
            json!({"click":{"name":"Quiet row 14"}}),
            "Opened quiet row 14",
        ),
        (
            json!({"click":{"name":"Delegated item 16"}}),
            "Picked Delegated item 16",
        ),
        (json!({"click":{"name":"quiet row 14"}}), "7 actions"),
    ] {
        let done = did(&mut peer, json!([step])).await;
        assert!(
            !done.contains('✗') && done.contains(heard),
            "{step}: {done}"
        );
    }
    // Words that are nowhere on the page name nothing, and words inside a
    // control do not make a second target of what the control holds.
    let absent = did(&mut peer, json!([{"click":{"name":"Order 99"}}])).await;
    assert!(
        absent.starts_with("✗ 1 click: browser_target_not_found"),
        "{absent}"
    );

    // A label names its field without what the field holds.
    let controls = did(&mut peer, json!([{"open":site.url("controls")}])).await;
    for line in [
        " value=Small: Size\n",
        " type=textarea: Notes\n",
        " type=select-multiple: Tags\n",
    ] {
        assert!(controls.contains(line), "{line:?} in {controls}");
    }
    assert!(
        !controls.contains("SmallLarge") && !controls.contains("Notes draft"),
        "{controls}"
    );
    for (step, heard) in [
        (
            json!({"click":{"name":"Paragraph with handler"}}),
            "Noted paragraph",
        ),
        (json!({"click":{"name":"Sortable header"}}), "Noted header"),
        (
            json!({"click":{"name":"Anchor with handler"}}),
            "Noted anchor",
        ),
        (
            json!({"click":{"name":"Paragraph with listener"}}),
            "Noted scripted",
        ),
        (
            json!({"click":{"name":"Anchor with listener"}}),
            "Noted wired",
        ),
        (
            json!({"click":{"name":"More options"}}),
            "Shown when opened",
        ),
        (
            json!({"select":{"name":"Size"},"value":"Large"}),
            " value=Large: Size",
        ),
        (
            json!({"fill":{"name":"Notes"},"text":"final"}),
            " value=final type=textarea: Notes",
        ),
        (
            json!({"fill":{"name":"Labelled editor"},"text":"typed"}),
            " value=typed editable: Labelled editor",
        ),
    ] {
        let done = did(&mut peer, json!([step])).await;
        assert!(
            !done.contains('✗') && done.contains(heard),
            "{step}: {done}"
        );
    }
    // A field nothing names is known by what it holds, and what is typed
    // into it comes back as that field alone, not as the page around it.
    let edited = did(
        &mut peer,
        json!([{"fill":{"name":"Edit me boldly"},"text":"plain"}]),
    )
    .await;
    assert!(
        edited.starts_with("1 fill textbox ")
            && edited.contains(" value=plain")
            && !edited.contains("Plain button")
            && edited.len() < 300,
        "{edited}"
    );
    // A control and something around or inside it that reads the same.
    for (name, written) in [
        ("Button with inner span", "1 click button "),
        ("Wrapped link", "1 click link "),
    ] {
        let done = did(&mut peer, json!([{"click":{"name":name}}])).await;
        assert!(done.starts_with(written), "{name}: {done}");
    }
    // A number is typed, a slider also answers to the keys a person would
    // use, and a link within the page changes the address without loading.
    let state = did(&mut peer, json!([{"open":site.url("state")}])).await;
    assert!(
        state.contains(" value=3: Quantity\n") && state.contains(" value=7 "),
        "{state}"
    );
    let typed = did(
        &mut peer,
        json!([
            {"fill":{"name":"Quantity"},"text":"42"},
            {"press":"ArrowRight","on":{"name":"Volume"}},
            {"click":{"name":"Show details"}},
            {"wait":{"url":"#details"}},
        ]),
    )
    .await;
    assert!(!typed.contains('✗'), "{typed}");
    let after = peer.call("browser_read", json!({})).await["result"]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        after.contains("/state.html#details\n")
            && after.contains(" value=42: Quantity\n")
            && after.contains(" value=8 "),
        "{after}"
    );
    assert_eq!(did(&mut peer, json!([{"close":true}])).await, "1 close");
}
