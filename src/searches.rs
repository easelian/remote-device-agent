//! Bounded background searches owned independently of a WebSocket connection.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::files::Files;
use crate::random;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files"),
    all(windows, feature = "experimental-windows-files")
))]
pub const SEARCH_OPERATIONS: &[&str] = &[
    "start_search",
    "get_search_results",
    "stop_search",
    "list_searches",
];
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files"),
    all(windows, feature = "experimental-windows-files")
)))]
pub const SEARCH_OPERATIONS: &[&str] = &[];

const MAX_CONCURRENT: usize = 2;
const MAX_RETAINED: usize = 16;
const MAX_RESULTS: usize = 1000;
const MAX_DEPTH: usize = 30;
const MAX_VISITED: usize = 10000;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const TASK_TTL: Duration = Duration::from_secs(30);
const RETENTION_TTL: Duration = Duration::from_secs(300);

pub const START_EXPIRED: &str = "Search request expired before it was started";
pub const SHUTTING_DOWN: &str = "Search manager is closed";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

struct JobState {
    status: &'static str,
    results: Vec<Value>,
    result_bytes: usize,
    visited: usize,
    skipped: usize,
    truncated_reason: Option<&'static str>,
    budget_stop: bool,
    error: Option<String>,
    created_at: u64,
    updated_at: u64,
    finished_at: Option<Instant>,
    finished_at_ms: Option<u64>,
    in_flight: bool,
    order: u64,
}

impl JobState {
    fn mark_finished(&mut self) {
        self.finished_at = Some(Instant::now());
        self.finished_at_ms = Some(now_ms());
    }
}

struct Job {
    id: String,
    root: String,
    path: String,
    query: String,
    folded: String,
    content: bool,
    deadline: Instant,
    cancelled: AtomicBool,
    state: Mutex<JobState>,
}

impl Job {
    fn refresh(&self, state: &mut JobState) {
        if matches!(state.status, "queued" | "running") && Instant::now() >= self.deadline {
            state.status = "expired";
            state.mark_finished();
            self.cancelled.store(true, Ordering::Release);
        }
    }

    fn running(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        self.refresh(&mut state);
        state.status == "running" && !state.budget_stop
    }

    fn stop(&self) -> &'static str {
        let mut state = self.state.lock().unwrap();
        self.refresh(&mut state);
        if matches!(state.status, "queued" | "running") {
            state.status = "stopped";
            state.mark_finished();
            self.cancelled.store(true, Ordering::Release);
        }
        state.status
    }

    fn mark_truncated(&self, reason: &'static str) {
        let mut state = self.state.lock().unwrap();
        if state.status == "running" {
            state.truncated_reason = Some(reason);
        }
    }

    fn skipped(&self) {
        let mut state = self.state.lock().unwrap();
        if state.status == "running" {
            state.skipped += 1;
        }
    }

    fn add(&self, value: Value) {
        let bytes = serde_json::to_vec(&value).map_or(MAX_RESULT_BYTES + 1, |bytes| bytes.len());
        let mut state = self.state.lock().unwrap();
        self.refresh(&mut state);
        if state.status != "running" || state.budget_stop {
            return;
        }
        if state.results.len() >= MAX_RESULTS
            || state.result_bytes.saturating_add(bytes) > MAX_RESULT_BYTES
        {
            state.truncated_reason = Some(if state.results.len() >= MAX_RESULTS {
                "max_entries"
            } else {
                "max_result_bytes"
            });
            state.budget_stop = true;
            return;
        }
        state.result_bytes += bytes;
        state.results.push(value);
        state.updated_at = now_ms();
    }
}

struct ManagerState {
    closed: bool,
    jobs: BTreeMap<String, Arc<Job>>,
    in_flight: usize,
    sequence: u64,
}

pub struct Searches {
    files: Arc<Files>,
    state: Mutex<ManagerState>,
    finished: Notify,
    #[cfg(test)]
    worker_gate: Mutex<Option<Arc<WorkerGate>>>,
}

#[cfg(test)]
#[derive(Default)]
struct WorkerGate {
    entered: std::sync::atomic::AtomicUsize,
    notify: Notify,
    released: Mutex<bool>,
    wake: std::sync::Condvar,
}

#[cfg(test)]
impl WorkerGate {
    fn wait(&self) {
        let released = self.released.lock().unwrap();
        self.entered.fetch_add(1, Ordering::Release);
        self.notify.notify_one();
        let (released, timeout) = self
            .wake
            .wait_timeout_while(released, Duration::from_secs(30), |released| !*released)
            .unwrap();
        assert!(
            *released && !timeout.timed_out(),
            "search worker gate timed out"
        );
    }
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files")
    ))]
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

impl Searches {
    pub fn quiescent(&self) -> bool {
        self.state
            .try_lock()
            .is_ok_and(|state| state.in_flight == 0)
    }
    pub fn new(files: Arc<Files>) -> Arc<Self> {
        Arc::new(Self {
            files,
            state: Mutex::new(ManagerState {
                closed: false,
                jobs: BTreeMap::new(),
                in_flight: 0,
                sequence: 0,
            }),
            finished: Notify::new(),
            #[cfg(test)]
            worker_gate: Mutex::new(None),
        })
    }

    fn prune(state: &mut ManagerState) {
        state.jobs.retain(|_, job| {
            let mut job_state = job.state.lock().unwrap();
            job.refresh(&mut job_state);
            job_state.in_flight
                || job_state
                    .finished_at
                    .is_none_or(|time| time.elapsed() < RETENTION_TTL)
        });
    }

    pub fn start(
        self: &Arc<Self>,
        params: &Value,
        deadline: Instant,
        closing: &AtomicBool,
    ) -> Result<Value, String> {
        if !params.is_object() {
            return Err("query must be 1..512 characters".into());
        }
        if self
            .state
            .lock()
            .map_err(|_| "Search manager unavailable")?
            .closed
            || closing.load(Ordering::Acquire)
        {
            return Err(SHUTTING_DOWN.into());
        }
        let root = params
            .get("root")
            .and_then(Value::as_str)
            .ok_or("query must be 1..512 characters")?;
        let path = match params.get("path") {
            None => ".",
            Some(value) => value.as_str().ok_or("query must be 1..512 characters")?,
        };
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .filter(|query| !query.is_empty() && query.encode_utf16().count() <= 512)
            .ok_or("query must be 1..512 characters")?;
        if params
            .get("pattern")
            .is_some_and(|value| value.as_str() != Some("literal"))
        {
            return Err("Only literal search patterns are supported".into());
        }
        let content = match params.get("content") {
            None => false,
            Some(value) => value.as_bool().ok_or("content must be boolean")?,
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Search manager unavailable")?;
        if state.closed || closing.load(Ordering::Acquire) {
            return Err(SHUTTING_DOWN.into());
        }
        if Instant::now() >= deadline {
            return Err(START_EXPIRED.into());
        }
        Self::prune(&mut state);
        if state.in_flight >= MAX_CONCURRENT {
            return Err("Maximum concurrent searches reached".into());
        }
        // This is bounded syntax/root-name validation only. The worker opens
        // the directory and records an open failure as a failed search task.
        self.files.validate_search_path(root, path)?;
        let id = random::hex_token::<16>().map_err(|error| error.to_string())?;
        if state.jobs.contains_key(&id) {
            return Err("Cannot allocate unique search ID".into());
        }
        // Randomness and validation may have taken time. Do not discard a
        // retained result or publish a job after shutdown or budget expiry.
        if closing.load(Ordering::Acquire) {
            return Err(SHUTTING_DOWN.into());
        }
        if Instant::now() >= deadline {
            return Err(START_EXPIRED.into());
        }
        if state.jobs.len() >= MAX_RETAINED {
            let oldest = state
                .jobs
                .iter()
                .filter_map(|(id, job)| {
                    let job_state = job.state.lock().unwrap();
                    (!job_state.in_flight).then_some((
                        job_state.finished_at_ms.unwrap_or(job_state.created_at),
                        job_state.order,
                        id.clone(),
                    ))
                })
                .min();
            if let Some((_, _, id)) = oldest {
                state.jobs.remove(&id);
            } else {
                return Err("Maximum retained search tasks reached".into());
            }
        }
        state.sequence += 1;
        let stamp = now_ms();
        let job = Arc::new(Job {
            id: id.clone(),
            root: root.into(),
            path: path.into(),
            query: query.into(),
            folded: Files::search_fold(query),
            content,
            deadline: Instant::now() + TASK_TTL,
            cancelled: AtomicBool::new(false),
            state: Mutex::new(JobState {
                status: "queued",
                results: Vec::new(),
                result_bytes: 0,
                visited: 0,
                skipped: 0,
                truncated_reason: None,
                budget_stop: false,
                error: None,
                created_at: stamp,
                updated_at: stamp,
                finished_at: None,
                finished_at_ms: None,
                in_flight: true,
                order: state.sequence,
            }),
        });
        state.jobs.insert(id.clone(), job.clone());
        state.in_flight += 1;
        let manager = Arc::clone(self);
        let worker = std::thread::Builder::new()
            .name("rdh-search".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(test)]
                    {
                        let gate = manager.worker_gate.lock().unwrap().clone();
                        if let Some(gate) = gate {
                            gate.wait();
                        }
                    }
                    manager.run(&job)
                }));
                {
                    let mut state = job
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    job.refresh(&mut state);
                    if matches!(state.status, "queued" | "running") {
                        if outcome.is_err() {
                            state.status = "failed";
                            state.error = Some("Search failed unexpectedly".into());
                        } else {
                            state.status = "completed";
                        }
                        state.mark_finished();
                    }
                    state.in_flight = false;
                }
                if let Ok(mut state) = manager.state.lock() {
                    state.in_flight -= 1;
                }
                manager.finished.notify_one();
            });
        if worker.is_err() {
            state.jobs.remove(&id);
            state.in_flight -= 1;
            return Err("Unable to start search worker".into());
        }
        Ok(json!({"search_id":id,"status":"queued","offset":0,"next_offset":0}))
    }

    fn job(&self, params: &Value) -> Result<Arc<Job>, String> {
        let id = params
            .get("search_id")
            .and_then(Value::as_str)
            .ok_or("Unknown search_id")?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Search manager unavailable")?;
        Self::prune(&mut state);
        state
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| "Unknown search_id".into())
    }

    pub fn get(&self, params: &Value) -> Result<Value, String> {
        let job = self.job(params)?;
        fn integer(
            params: &Value,
            key: &str,
            default: f64,
            min: f64,
            max: f64,
        ) -> Result<f64, String> {
            let value = match params.get(key) {
                None | Some(Value::Null) => return Ok(default),
                Some(value) => value,
            };
            value
                .as_f64()
                .filter(|value| {
                    value.is_finite() && value.fract() == 0.0 && *value >= min && *value <= max
                })
                .ok_or_else(|| "Invalid offset/limit".into())
        }
        // JavaScript accepts any finite integral Number here. Test the offset
        // against the bounded result count before converting it to usize.
        let offset = integer(params, "offset", 0.0, 0.0, f64::MAX)?;
        let limit = integer(params, "limit", 100.0, 1.0, 200.0)? as usize;
        let mut state = job.state.lock().map_err(|_| "Search unavailable")?;
        job.refresh(&mut state);
        if offset > state.results.len() as f64 {
            return Err("offset is beyond results currently available".into());
        }
        let offset = offset as usize;
        let end = offset.saturating_add(limit).min(state.results.len());
        let next = if end < state.results.len() || matches!(state.status, "queued" | "running") {
            Some(end)
        } else {
            None
        };
        let mut result = json!({"search_id":job.id, "status":state.status, "results":state.results[offset..end],
            "total":state.results.len(), "offset":offset, "next_offset":next,
            "truncated":state.truncated_reason.is_some(), "visited":state.visited, "skipped":state.skipped});
        if let Some(reason) = state.truncated_reason {
            result["truncated_reason"] = json!(reason);
        }
        if let Some(error) = &state.error {
            result["error"] = json!(error);
        }
        Ok(result)
    }

    pub fn stop(&self, params: &Value) -> Result<Value, String> {
        let job = self.job(params)?;
        Ok(json!({"search_id":job.id,"status":job.stop()}))
    }

    pub fn list(&self) -> Result<Value, String> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| "Search manager unavailable")?;
        Self::prune(&mut manager);
        let mut jobs: Vec<_> = manager.jobs.values().map(|job| {
            let mut state = job.state.lock().unwrap();
            job.refresh(&mut state);
            let mut result = json!({"search_id":job.id,"status":state.status,"total":state.results.len(),
                "visited":state.visited,"skipped":state.skipped,"truncated":state.truncated_reason.is_some(),
                "created_at":state.created_at,"updated_at":state.updated_at});
            if let Some(reason) = state.truncated_reason { result["truncated_reason"] = json!(reason); }
            (state.order, result)
        }).collect();
        jobs.sort_by_key(|(order, _)| *order);
        Ok(json!({"searches":jobs.into_iter().map(|(_, value)| value).collect::<Vec<_>>()}))
    }

    fn run(&self, job: &Job) {
        {
            let mut state = job.state.lock().unwrap();
            job.refresh(&mut state);
            if state.status != "queued" {
                return;
            }
            state.status = "running";
        }
        if let Err(error) = self.files.check_search_directory(&job.root, &job.path) {
            let mut state = job.state.lock().unwrap();
            job.refresh(&mut state);
            if state.status == "running" {
                state.status = "failed";
                state.error = Some(error);
                state.mark_finished();
            }
            return;
        }
        let lookup_path = if job.path.is_empty() { "." } else { &job.path };
        self.walk(job, &job.path, lookup_path, 0);
    }

    fn walk(&self, job: &Job, display_path: &str, lookup_path: &str, depth: usize) {
        if !job.running() {
            return;
        }
        if depth > MAX_DEPTH {
            job.mark_truncated("max_depth");
            return;
        }
        let remaining = MAX_VISITED.saturating_sub(job.state.lock().unwrap().visited);
        let (entries, truncated) = match self.files.search_entries(
            &job.root,
            lookup_path,
            remaining,
            &job.cancelled,
            job.deadline,
        ) {
            Ok(value) => value,
            Err(_) => {
                if job.running() {
                    job.mark_truncated("directory_error");
                    job.skipped();
                }
                return;
            }
        };
        if truncated {
            job.mark_truncated("max_visited");
        }
        for entry in entries {
            if !job.running() {
                return;
            }
            {
                let mut state = job.state.lock().unwrap();
                if state.visited >= MAX_VISITED {
                    state.truncated_reason = Some("max_visited");
                    return;
                }
                state.visited += 1;
            }
            let relative = if display_path == "." {
                entry.name.clone()
            } else {
                format!("{display_path}/{}", entry.name)
            };
            // Keep the legacy spelling in results while performing I/O only
            // with a separately built root-relative path. In particular, an
            // empty input path has a leading slash in legacy result strings.
            let child_path = if lookup_path == "." {
                entry.name.clone()
            } else {
                format!("{lookup_path}/{}", entry.name)
            };
            if entry.kind != "symlink"
                && !job.content
                && Files::search_fold(&entry.name).contains(&job.folded)
            {
                job.add(json!({"path":relative}));
            }
            if entry.kind == "directory" {
                self.walk(job, &relative, &child_path, depth + 1);
            } else if entry.kind == "file" && job.content {
                match self
                    .files
                    .search_read(&job.root, &child_path)
                    .and_then(|bytes| String::from_utf8(bytes).map_err(|_| "Invalid UTF-8".into()))
                {
                    Ok(text) => {
                        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                        let mut lines = text.split('\n').enumerate().peekable();
                        while let Some((index, line)) = lines.next() {
                            if !job.running() {
                                return;
                            }
                            let line = if lines.peek().is_some() {
                                line.strip_suffix('\r').unwrap_or(line)
                            } else {
                                line
                            };
                            if line.contains(&job.query) {
                                let preview: String = line.chars().take(500).collect();
                                job.add(json!({"path":relative,"line":index + 1,"text":preview}));
                            }
                        }
                    }
                    Err(_) => job.skipped(),
                }
            }
        }
    }

    pub async fn close(&self, timeout: Duration) -> bool {
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return false,
            };
            state.closed = true;
            for job in state.jobs.values() {
                job.stop();
            }
        }
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.finished.notified();
                if self
                    .state
                    .lock()
                    .map(|state| state.in_flight == 0)
                    .unwrap_or(false)
                {
                    break;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
    }
}

#[cfg(all(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files")
    )
))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "rdh-search-{}-{}",
                std::process::id(),
                random::hex_token::<8>().unwrap()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn manager(&self) -> Arc<Searches> {
            let files = Files::new(
                &BTreeMap::from([("work".into(), self.0.to_str().unwrap().into())]),
                false,
            )
            .unwrap();
            Searches::new(Arc::new(files))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn start(manager: &Arc<Searches>, params: Value) -> String {
        manager
            .start(
                &params,
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            )
            .unwrap()["search_id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn complete(manager: &Searches, id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let result = manager.get(&json!({"search_id":id})).unwrap();
                if !matches!(result["status"].as_str(), Some("queued" | "running")) {
                    return result;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }
    struct GateGuard(Arc<WorkerGate>);
    impl Drop for GateGuard {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn searches_are_paged_unicode_aware_and_do_not_follow_symlinks() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("nested")).unwrap();
        std::fs::write(fixture.0.join("Straße"), b"alpha\nneedle\r\n").unwrap();
        std::fs::write(fixture.0.join("nested/STRASSE"), b"needle again").unwrap();
        std::fs::write(fixture.0.join("invalid"), [0xff, 0xfe]).unwrap();
        symlink(fixture.0.join("Straße"), fixture.0.join("strasse-link")).unwrap();
        let manager = fixture.manager();
        let id = start(&manager, json!({"root":"work","query":"strasse"}));
        let full = complete(&manager, &id).await;
        assert_eq!(full["status"], "completed");
        assert_eq!(full["total"], 2);
        assert_eq!(
            full["results"],
            json!([{"path":"Straße"},{"path":"nested/STRASSE"}])
        );
        let first = manager.get(&json!({"search_id":id,"limit":1})).unwrap();
        assert_eq!(first["next_offset"], 1);
        let second = manager
            .get(&json!({"search_id":id,"offset":1,"limit":1}))
            .unwrap();
        assert_eq!(second["next_offset"], Value::Null);
        assert!(manager.get(&json!({"search_id":id,"offset":3})).is_err());
        let content = start(
            &manager,
            json!({"root":"work","query":"needle","content":true}),
        );
        let content = complete(&manager, &content).await;
        assert_eq!(
            content["results"],
            json!([{"path":"Straße","line":2,"text":"needle"},{"path":"nested/STRASSE","line":1,"text":"needle again"}])
        );
        assert_eq!(content["skipped"], 1);
        assert!(manager.close(Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn search_admission_and_failed_directory_tasks_keep_legacy_contract() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("match"), "needle").unwrap();
        symlink(&fixture.0, fixture.0.join("linked-directory")).unwrap();
        let manager = fixture.manager();

        assert_eq!(
            start_error(&manager, json!({"root":"work","path":null,"query":"x"})),
            "query must be 1..512 characters"
        );
        assert_eq!(
            start_error(
                &manager,
                json!({"root":"work","query":"x","pattern":0,"content":0})
            ),
            "Only literal search patterns are supported"
        );
        assert_eq!(
            start_error(
                &manager,
                json!({"root":"work","path":"../outside","query":"x"})
            ),
            "Path must be relative to a root, with no '..' or NUL"
        );
        assert!(
            manager.list().unwrap()["searches"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        // An acceptable relative path creates a task even if it cannot be
        // opened. Symlink traversal must still fail inside the worker.
        for path in ["missing", "linked-directory"] {
            let id = start(&manager, json!({"root":"work","path":path,"query":"x"}));
            let result = complete(&manager, &id).await;
            assert_eq!(result["status"], "failed");
            assert!(result["error"].as_str().is_some_and(|s| !s.is_empty()));
            assert_eq!(result["results"], json!([]));
            tokio::time::timeout(Duration::from_secs(3), async {
                while !manager.quiescent() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }

        let id = start(&manager, json!({"root":"work","query":"match"}));
        assert_eq!(complete(&manager, &id).await["status"], "completed");
        for offset in [9_007_199_254_740_992.0, 1e100, f64::MAX] {
            assert_eq!(
                manager
                    .get(&json!({"search_id":id,"offset":offset}))
                    .unwrap_err(),
                "offset is beyond results currently available"
            );
        }
        assert_eq!(
            manager
                .get(&json!({"search_id":id,"offset":1e100,"limit":0}))
                .unwrap_err(),
            "Invalid offset/limit"
        );
        let retained = manager.list().unwrap()["searches"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(
            manager
                .start(
                    &json!({"root":"work","query":"x"}),
                    Instant::now() - Duration::from_secs(1),
                    &AtomicBool::new(false),
                )
                .unwrap_err(),
            START_EXPIRED
        );
        assert_eq!(
            manager.list().unwrap()["searches"]
                .as_array()
                .unwrap()
                .len(),
            retained
        );
        assert!(manager.close(Duration::from_secs(3)).await);
        assert_eq!(
            start_error(&manager, json!({"root":"work","query":""})),
            SHUTTING_DOWN
        );
    }

    #[tokio::test]
    async fn empty_search_path_uses_root_relative_io_and_preserves_legacy_results() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("nested")).unwrap();
        std::fs::write(fixture.0.join("top-target"), "needle top").unwrap();
        std::fs::write(fixture.0.join("nested/target"), "needle nested").unwrap();
        symlink(fixture.0.join("nested"), fixture.0.join("link")).unwrap();
        let manager = fixture.manager();
        let names = start(&manager, json!({"root":"work","path":"","query":"target"}));
        let names = complete(&manager, &names).await;
        assert_eq!(names["status"], "completed");
        assert_eq!(names["truncated"], false);
        assert_eq!(
            names["results"],
            json!([
                {"path":"/nested/target"}, {"path":"/top-target"}
            ])
        );
        let content = start(
            &manager,
            json!({"root":"work","path":"","query":"needle","content":true}),
        );
        let content = complete(&manager, &content).await;
        assert_eq!(content["status"], "completed");
        assert_eq!(content["skipped"], 0);
        assert_eq!(
            content["results"],
            json!([
                {"path":"/nested/target","line":1,"text":"needle nested"},
                {"path":"/top-target","line":1,"text":"needle top"}
            ])
        );
        assert!(manager.close(Duration::from_secs(3)).await);
    }

    #[tokio::test]
    async fn retention_evicts_the_earliest_finished_task() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let gate = Arc::new(WorkerGate::default());
        let _release_on_failure = GateGuard(gate.clone());
        *manager.worker_gate.lock().unwrap() = Some(gate.clone());
        let slow = start(&manager, json!({"root":"work","query":"slow"}));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let notified = gate.notify.notified();
                if gate.entered.load(Ordering::Acquire) == 1 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap();
        *manager.worker_gate.lock().unwrap() = None;
        let fast = start(&manager, json!({"root":"work","query":"fast"}));
        assert_eq!(complete(&manager, &fast).await["status"], "completed");
        tokio::time::timeout(Duration::from_secs(3), async {
            while manager.state.lock().unwrap().in_flight != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // Distinguish completion times even on the legacy millisecond clock.
        tokio::time::sleep(Duration::from_millis(5)).await;
        gate.release();
        assert_eq!(complete(&manager, &slow).await["status"], "completed");
        for index in 0..(MAX_RETAINED - 2) {
            tokio::time::timeout(Duration::from_secs(3), async {
                while !manager.quiescent() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let id = start(&manager, json!({"root":"work","query":format!("{index}")}));
            assert_eq!(complete(&manager, &id).await["status"], "completed");
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while !manager.quiescent() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let replacement = start(&manager, json!({"root":"work","query":"replacement"}));
        assert_eq!(
            complete(&manager, &replacement).await["status"],
            "completed"
        );
        assert!(manager.get(&json!({"search_id":slow})).is_ok());
        assert_eq!(
            manager.get(&json!({"search_id":fast})).unwrap_err(),
            "Unknown search_id"
        );
        assert_eq!(
            manager.list().unwrap()["searches"]
                .as_array()
                .unwrap()
                .len(),
            MAX_RETAINED
        );
        assert!(manager.close(Duration::from_secs(3)).await);
    }

    fn start_error(manager: &Arc<Searches>, params: Value) -> String {
        manager
            .start(
                &params,
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            )
            .unwrap_err()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_does_not_free_worker_capacity_until_cleanup_finishes() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let gate = Arc::new(WorkerGate::default());
        let _release_on_failure = GateGuard(gate.clone());
        *manager.worker_gate.lock().unwrap() = Some(gate.clone());
        let first = start(&manager, json!({"root":"work","query":"x"}));
        let second = start(&manager, json!({"root":"work","query":"y"}));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let notified = gate.notify.notified();
                if gate.entered.load(Ordering::Acquire) == 2 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            manager.stop(&json!({"search_id":first})).unwrap()["status"],
            "stopped"
        );
        assert!(!manager.quiescent());
        assert!(
            manager
                .start(
                    &json!({"root":"work","query":"z"}),
                    Instant::now() + Duration::from_secs(2),
                    &AtomicBool::new(false)
                )
                .unwrap_err()
                .contains("concurrent")
        );
        assert!(!manager.close(Duration::from_millis(30)).await);
        assert_eq!(
            manager.get(&json!({"search_id":second})).unwrap()["status"],
            "stopped"
        );
        gate.release();
        assert!(manager.close(Duration::from_secs(2)).await);
        assert_eq!(manager.state.lock().unwrap().in_flight, 0);
        assert!(manager.quiescent());
        assert!(
            manager
                .start(
                    &json!({"root":"work","query":"z"}),
                    Instant::now() + Duration::from_secs(2),
                    &AtomicBool::new(false)
                )
                .unwrap_err()
                .contains("closed")
        );
    }
}
