//! Private, bounded browser artifacts. Chromium's own download writer stays disabled.
use super::{
    Failure, Principal, Result,
    control::Control,
    engine::{DocumentRequest, RequestDecision, TransferPort},
    io_error,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "experimental-browser-bidi")]
#[derive(Clone)]
pub(crate) struct Downloads {
    pub files: Arc<Files>,
    pub principal: Principal,
    pub session: String,
    pub control: Arc<Control>,
}
#[cfg(feature = "experimental-browser-bidi")]
impl Downloads {
    pub fn owner(&self, tab: String, url: &str) -> Result<Owner> {
        Ok(Owner {
            principal: self.principal.clone(),
            session: self.session.clone(),
            tab,
            origin: crate::browser::address::origin(url)?,
            control: self.control.clone(),
        })
    }
}
#[cfg(feature = "experimental-browser-bidi")]
pub(crate) fn attachment(headers: &[super::engine::Header]) -> bool {
    headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case("content-disposition")
            && h.value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("attachment")
    })
}
mod download;
pub(crate) use download::Download;

pub const MAX_FILE: u64 = 32 * 1024 * 1024;
pub const MAX_TOTAL: u64 = 128 * 1024 * 1024;
pub const MAX_FILES: usize = 16;
pub const CHUNK: usize = 64 * 1024;
pub const RETENTION: u64 = 86400;
mod upload;
pub use upload::UPLOAD_TTL;
use upload::Upload;

#[derive(Clone)]
pub struct Owner {
    pub principal: Principal,
    pub session: String,
    pub tab: String,
    pub origin: String,
    pub control: Arc<Control>,
}

/// A private one-use upload authorization, consumed after semantic validation.
#[cfg(feature = "experimental-browser-bidi")]
#[derive(Clone)]
pub struct Attachment {
    pub files: Arc<Files>,
    pub owner: Owner,
    pub grant: String,
    pub operation: String,
    pub epoch: u64,
}
#[cfg(feature = "experimental-browser-bidi")]
impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Attachment")
    }
}
#[cfg(feature = "experimental-browser-bidi")]
impl Attachment {
    pub fn consume(self, origin: String) -> Result<PathBuf> {
        let mut owner = self.owner;
        owner.origin = origin;
        let state = owner.control.state.lock().unwrap();
        if *state != (self.epoch, false) {
            return Err(Failure::new("browser_control_changed"));
        }
        self.files
            .consume_upload(&owner, &self.grant, &self.operation)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u32,
    id: String,
    owner: String,
    user: String,
    session_id: String,
    tab_id: String,
    origin: String,
    name: String,
    size_bytes: u64,
    sha256: String,
    state: String,
    created_at: u64,
    expires_at: u64,
    ai_visible: bool,
    upload: Option<Upload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cancellation_scope: Option<String>,
}
impl Metadata {
    fn allowed(&self, principal: &Principal) -> bool {
        self.user == principal.user
            && (principal.channel == "human" || (self.owner == principal.owner && self.ai_visible))
    }
    fn value(&self) -> Value {
        let mut value = json!({"artifact_id":self.id,"session_id":self.session_id,"tab_id":self.tab_id,
            "name":self.name,"source_origin":self.origin,"size_bytes":self.size_bytes,"sha256":self.sha256,
            "state":self.state,"created_at":self.created_at,"expires_at":self.expires_at});
        if self.state == "receiving"
            && let Some(scope) = &self.cancellation_scope
        {
            value["cancellation_scope"] = scope.clone().into();
        }
        value
    }
}
struct Entry {
    meta: Metadata,
    cancel: Arc<AtomicBool>,
}
#[derive(Default)]
struct Catalog {
    initialized: bool,
    entries: HashMap<String, Entry>,
    failures: VecDeque<(Metadata, &'static str)>,
}
pub struct Files {
    root: Option<PathBuf>,
    catalog: Mutex<Catalog>,
    root_lock: Mutex<Option<Arc<crate::profile_lock::ProfileLocks>>>,
    stopping: AtomicBool,
    jobs: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    janitor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    waiting: tokio::sync::Mutex<()>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn id_valid(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn hash_valid(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn open_content(path: &Path) -> Result<File> {
    crate::storage::open_private_file(path, false)
        .map_err(|_| Failure::new("browser_file_unavailable"))
}

fn hash_file(file: &mut File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let count = file.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > MAX_FILE {
            return Err(Failure::new("browser_file_limit"));
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub fn filename(header: &str) -> String {
    // Display only: the supplied filename is never used as a filesystem path.
    let name = header
        .split(';')
        .skip(1)
        .find_map(|part| part.trim().strip_prefix("filename="))
        .unwrap_or("download.bin")
        .trim_matches('"');
    let name = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect::<String>();
    if name.trim().is_empty() {
        "download.bin".into()
    } else {
        name
    }
}
impl Files {
    pub fn new(root: Option<&Path>) -> Arc<Self> {
        Arc::new(Self {
            root: root.map(|p| p.join("artifacts")),
            catalog: Mutex::new(Catalog::default()),
            root_lock: Mutex::new(None),
            stopping: AtomicBool::new(false),
            jobs: Mutex::new(Vec::new()),
            janitor: Mutex::new(None),
            waiting: tokio::sync::Mutex::new(()),
        })
    }
    fn root(&self) -> Result<&Path> {
        self.root
            .as_deref()
            .ok_or_else(|| Failure::new("browser_file_unavailable"))
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        if !id_valid(id) {
            return Err(Failure::new("browser_file_unavailable"));
        }
        Ok(self.root()?.join(id))
    }
    fn save(&self, meta: &Metadata) -> Result<()> {
        crate::storage::write_private_replace(
            &self.path(&meta.id)?.join("metadata.json"),
            &serde_json::to_vec(meta).map_err(io_error)?,
        )
        .map_err(io_error)
    }
    pub fn prepare(&self) -> Result<()> {
        let mut catalog = self.catalog.lock().unwrap();
        if catalog.initialized {
            return Ok(());
        }
        let root = self.root()?;
        match crate::storage::create_private_dir(root) {
            Ok(_) => {}
            Err(crate::error::Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::storage::private_metadata(root, true).map_err(io_error)?;
            }
            Err(e) => return Err(io_error(e)),
        }
        let entries = fs::read_dir(root)
            .map_err(io_error)?
            .take(MAX_FILES + 1)
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(io_error)?;
        if entries.len() > MAX_FILES {
            return Err(Failure::new("browser_file_limit"));
        }
        let mut total = 0;
        for entry in entries {
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| Failure::new("browser_file_unavailable"))?;
            let path = self.path(&id)?;
            crate::storage::private_metadata(&path, true).map_err(io_error)?;
            let meta: Metadata = serde_json::from_slice(
                &crate::storage::read_private(&path.join("metadata.json")).map_err(io_error)?,
            )
            .map_err(io_error)?;
            if !match &meta.upload {
                Some(upload) => upload.format_matches(meta.version),
                None => meta.version == 1,
            } || meta.id != id
                || !hash_valid(&meta.owner)
                || !hash_valid(&meta.user)
                || meta.size_bytes > MAX_FILE
                || meta.name.len() > 512
                || meta.origin.len() > 4096
                || meta
                    .cancellation_scope
                    .as_deref()
                    .is_some_and(|s| s != "session")
                || !matches!(
                    meta.state.as_str(),
                    "receiving" | "ready" | "staging" | "approved" | "consumed"
                )
                || meta.expires_at < meta.created_at
                || meta.expires_at - meta.created_at > RETENTION
            {
                return Err(Failure::new("browser_file_unavailable"));
            }
            // Upload grants never survive the session's Agent instance.
            if meta.upload.is_some() || meta.state == "receiving" || meta.expires_at <= now() {
                fs::remove_dir_all(path).map_err(io_error)?;
                continue;
            }
            let mut content = open_content(&path.join("content"))?;
            total += meta.size_bytes;
            if total > MAX_TOTAL
                || content.metadata().map_err(io_error)?.len() != meta.size_bytes
                || hash_file(&mut content)? != meta.sha256
            {
                return Err(Failure::new("browser_file_unavailable"));
            }
            catalog.entries.insert(
                id,
                Entry {
                    meta,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        catalog.initialized = true;
        Ok(())
    }
    pub fn start(self: &Arc<Self>, lock: Arc<crate::profile_lock::ProfileLocks>) {
        *self.root_lock.lock().unwrap() = Some(lock);
        let weak = Arc::downgrade(self);
        *self.janitor.lock().unwrap() = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(files) = weak.upgrade() else {
                    break;
                };
                if files.stopping.load(Ordering::Acquire) {
                    break;
                }
                let _ = files.expire();
            }
        }));
    }
    fn expire(&self) -> Result<()> {
        let mut catalog = self.catalog.lock().unwrap();
        let expired: Vec<_> = catalog
            .entries
            .iter()
            .filter(|(_, e)| {
                e.meta.expires_at <= now() && (e.meta.state == "ready" || e.meta.upload.is_some())
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            fs::remove_dir_all(self.path(&id)?).map_err(io_error)?;
            catalog.entries.remove(&id);
        }
        Ok(())
    }
    pub fn list(&self, principal: &Principal) -> Result<Value> {
        self.expire()?;
        let catalog = self.catalog.lock().unwrap();
        let mut files: Vec<_> = catalog
            .entries
            .values()
            .filter(|e| {
                e.meta.upload.is_none() && e.meta.allowed(principal) && e.meta.expires_at > now()
            })
            .map(|e| e.meta.value())
            .collect();
        files.sort_by(|a, b| a["created_at"].as_u64().cmp(&b["created_at"].as_u64()));
        let uploads: Vec<_> = catalog
            .entries
            .values()
            .filter(|e| {
                e.meta.upload.is_some() && e.meta.allowed(principal) && e.meta.expires_at > now()
            })
            .map(|e| upload::value(&e.meta))
            .collect();
        let failures:Vec<_>=catalog.failures.iter().filter(|(m,_)|m.allowed(principal)).map(|(m,code)|json!({"session_id":m.session_id,"name":m.name,"source_origin":m.origin,"error_code":code})).collect();
        Ok(
            json!({"files":files,"uploads":uploads,"recent_failures":failures,"max_file_bytes":MAX_FILE,"max_total_bytes":MAX_TOTAL,"retention_seconds":RETENTION}),
        )
    }
    pub fn read(&self, principal: &Principal, id: &str, offset: u64) -> Result<Value> {
        let catalog = self.catalog.lock().unwrap();
        let meta = &catalog
            .entries
            .get(id)
            .filter(|e| {
                e.meta.upload.is_none()
                    && e.meta.allowed(principal)
                    && e.meta.state == "ready"
                    && e.meta.expires_at > now()
            })
            .ok_or_else(|| Failure::new("browser_file_unavailable"))?
            .meta;
        if offset > meta.size_bytes {
            return Err(Failure::new("browser_invalid_params"));
        }
        let mut file = open_content(&self.path(id)?.join("content"))?;
        let before = file.metadata().map_err(io_error)?;
        if before.len() != meta.size_bytes {
            return Err(Failure::new("browser_file_changed"));
        }
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let mut bytes = vec![0u8; CHUNK.min((meta.size_bytes - offset) as usize)];
        file.read_exact(&mut bytes).map_err(io_error)?;
        let after = file.metadata().map_err(io_error)?;
        // Both snapshots describe the same held file handle. Its identity is
        // fixed; changes to length or write time invalidate this chunk.
        if before.len() != after.len()
            || before.modified().map_err(io_error)? != after.modified().map_err(io_error)?
        {
            return Err(Failure::new("browser_file_changed"));
        }
        Ok(
            json!({"artifact_id":id,"offset":offset,"next_offset":offset+bytes.len() as u64,"size_bytes":meta.size_bytes,"sha256":meta.sha256,"eof":offset+bytes.len() as u64==meta.size_bytes,"data":base64::engine::general_purpose::STANDARD.encode(bytes)}),
        )
    }
    pub fn delete(&self, principal: &Principal, id: &str) -> Result<Value> {
        let mut catalog = self.catalog.lock().unwrap();
        let entry = catalog
            .entries
            .get(id)
            .filter(|e| e.meta.allowed(principal))
            .ok_or_else(|| Failure::new("browser_file_unavailable"))?;
        if entry.meta.upload.is_none() && entry.meta.state != "ready" {
            entry.cancel.store(true, Ordering::Release);
            return Ok(
                json!({"artifact_id":id,"file_deleted":false,"cancel_requested":true,"state":"cancelling"}),
            );
        }
        fs::remove_dir_all(self.path(id)?).map_err(|_| Failure::unknown("browser_io_error"))?;
        catalog.entries.remove(id);
        Ok(json!({"artifact_id":id,"file_deleted":true}))
    }
    pub fn disclosure(&self, session: &str, visible: bool) -> Result<()> {
        let mut catalog = self.catalog.lock().unwrap();
        // Hide the complete in-memory set before any fallible write. A failed
        // takeover must not leave later entries or failure metadata visible.
        for entry in catalog
            .entries
            .values_mut()
            .filter(|e| e.meta.session_id == session)
        {
            entry.meta.ai_visible = false;
        }
        for (meta, _) in &mut catalog.failures {
            if meta.session_id == session {
                meta.ai_visible = false;
            }
        }
        let mut failure = None;
        for entry in catalog
            .entries
            .values()
            .filter(|e| e.meta.session_id == session)
        {
            let mut persisted = entry.meta.clone();
            persisted.ai_visible =
                visible && persisted.upload.as_ref().is_none_or(Upload::disclosable);
            if let Err(error) = self.save(&persisted) {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if visible {
            for entry in catalog
                .entries
                .values_mut()
                .filter(|e| e.meta.session_id == session)
            {
                entry.meta.ai_visible = entry.meta.upload.as_ref().is_none_or(Upload::disclosable);
            }
            for (meta, _) in &mut catalog.failures {
                if meta.session_id == session {
                    meta.ai_visible = true;
                }
            }
        }
        Ok(())
    }
    fn failure(&self, mut meta: Metadata, code: &'static str) {
        let mut catalog = self.catalog.lock().unwrap();
        if let Some(entry) = catalog.entries.get(&meta.id) {
            meta.ai_visible = entry.meta.ai_visible;
        }
        // Keep occupancy if removal fails; a failed cleanup must not free its budget.
        if self
            .path(&meta.id)
            .is_ok_and(|p| !p.exists() || fs::remove_dir_all(p).is_ok())
        {
            catalog.entries.remove(&meta.id);
        }
        catalog.failures.push_back((meta, code));
        while catalog.failures.len() > MAX_FILES {
            catalog.failures.pop_front();
        }
    }
    pub(crate) fn reserve(
        self: &Arc<Self>,
        status: Option<u64>,
        headers: &[super::engine::Header],
        owner: Owner,
        session_cancellation: bool,
    ) -> Result<Download> {
        let values = |name: &str| {
            headers
                .iter()
                .filter(|h| h.name.eq_ignore_ascii_case(name))
                .map(|h| h.value.as_str())
                .collect::<Vec<_>>()
        };
        let disposition = values("content-disposition");
        let name = filename(disposition.first().copied().unwrap_or(""));
        let lengths = values("content-length");
        let size = if lengths.len() == 1 {
            lengths[0].parse::<u64>().ok()
        } else {
            None
        };
        // Hold the same fence as takeover until the entry or failure is in the
        // catalog. Otherwise a pre-takeover visibility snapshot could be
        // inserted after takeover has hidden this session's existing files.
        let control = owner.control.state.lock().unwrap();
        let created = now();
        let meta = Metadata {
            version: 1,
            id: crate::random::hex_token::<16>().map_err(io_error)?,
            owner: owner.principal.owner,
            user: owner.principal.user,
            session_id: owner.session,
            tab_id: owner.tab,
            origin: owner.origin,
            name,
            size_bytes: size.unwrap_or(0),
            sha256: String::new(),
            state: "receiving".into(),
            created_at: created,
            expires_at: created + RETENTION,
            ai_visible: !control.1,
            upload: None,
            cancellation_scope: session_cancellation.then(|| "session".into()),
        };
        let encoding = values("content-encoding");
        let denied = if !matches!(status, Some(200))
            || size.is_none()
            || encoding.iter().any(|e| !e.eq_ignore_ascii_case("identity"))
        {
            Some("browser_download_unsupported")
        } else if size.is_some_and(|size| size > MAX_FILE) {
            Some("browser_file_limit")
        } else {
            None
        };
        if let Some(code) = denied {
            self.failure(meta, code);
            return Err(Failure::new(code));
        }
        let path = self.path(&meta.id)?;
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut catalog = self.catalog.lock().unwrap();
            if self.stopping.load(Ordering::Acquire)
                || catalog.entries.len() >= MAX_FILES
                || catalog
                    .entries
                    .values()
                    .map(|e| e.meta.size_bytes)
                    .sum::<u64>()
                    + meta.size_bytes
                    > MAX_TOTAL
            {
                drop(catalog);
                self.failure(meta, "browser_file_limit");
                return Err(Failure::new("browser_file_limit"));
            }
            crate::storage::create_private_dir(&path).map_err(io_error)?;
            if let Err(error) = self.save(&meta) {
                let _ = fs::remove_dir_all(&path);
                return Err(error);
            }
            catalog.entries.insert(
                meta.id.clone(),
                Entry {
                    meta: meta.clone(),
                    cancel: cancel.clone(),
                },
            );
        }
        drop(control);
        Ok(Download {
            #[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
            files: self.clone(),
            meta,
            path,
            cancel,
        })
    }
    #[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
    pub(crate) fn track(&self, job: tokio::task::JoinHandle<()>) {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|j| !j.is_finished());
        jobs.push(job);
    }
    pub fn intercept(
        self: &Arc<Self>,
        engine: impl TransferPort,
        params: &DocumentRequest,
        owner: Owner,
    ) -> Result<()> {
        let headers = params
            .headers
            .as_ref()
            .ok_or_else(|| Failure::new("browser_file_incomplete"))?;
        let Download {
            mut meta,
            path,
            cancel,
            ..
        } = self.reserve(params.status, headers, owner, false)?;
        let files = self.clone();
        let request = params.handle.clone();
        let job = tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut stream = None;
            let outcome: Result<()> = async {
                stream = Some(engine.open_stream(&request, deadline).await?);
                let mut file = crate::storage::create_private_file(&path.join("content"))
                    .map_err(io_error)?;
                let mut received = 0u64;
                let mut hash = Sha256::new();
                loop {
                    if files.stopping.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
                        return Err(Failure::new("browser_file_cancelled"));
                    }
                    let data = tokio::select! {
                        result=engine.read_stream(stream.as_ref().unwrap(), CHUNK, deadline)=>result?,
                        _=async {while !files.stopping.load(Ordering::Acquire)&&!cancel.load(Ordering::Acquire){tokio::time::sleep(Duration::from_millis(50)).await;}}=>return Err(Failure::new("browser_file_cancelled")),
                    };
                    let bytes=data.bytes;
                    if bytes.len() > CHUNK || received + bytes.len() as u64 > meta.size_bytes {
                        return Err(Failure::new("browser_file_limit"));
                    }
                    file.write_all(&bytes).map_err(io_error)?;
                    hash.update(&bytes);
                    received += bytes.len() as u64;
                    if data.eof {
                        break;
                    }
                }
                if received != meta.size_bytes {
                    return Err(Failure::new("browser_file_incomplete"));
                }
                file.sync_all().map_err(io_error)?;
                drop(file);
                meta.state = "ready".into();
                meta.sha256 = format!("{:x}", hash.finalize());
                let mut catalog = files.catalog.lock().unwrap();
                let entry = catalog
                    .entries
                    .get_mut(&meta.id)
                    .ok_or_else(|| Failure::new("browser_file_unavailable"))?;
                if entry.cancel.load(Ordering::Acquire)||files.stopping.load(Ordering::Acquire){return Err(Failure::new("browser_file_cancelled"));}
                meta.ai_visible = entry.meta.ai_visible;
                files.save(&meta)?;
                entry.meta = meta.clone();
                Ok(())
            }
            .await;
            if let Some(stream) = stream {
                let _ = engine
                    .close_stream(stream, Instant::now() + Duration::from_secs(1))
                    .await;
            }
            let _ = engine
                .respond(
                    &request,
                    RequestDecision::Abort,
                    Instant::now() + Duration::from_secs(1),
                )
                .await;
            if let Err(error) = outcome {
                files.failure(meta, error.code);
            }
        });
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|j| !j.is_finished());
        jobs.push(job);
        Ok(())
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(janitor) = self.janitor.lock().unwrap().as_ref() {
            janitor.abort();
        }
        for entry in self.catalog.lock().unwrap().entries.values() {
            entry.cancel.store(true, Ordering::Release);
        }
    }
    pub async fn wait(&self) {
        use std::{future::Future, pin::Pin, task::Poll};
        let _waiting = self.waiting.lock().await;
        // Event producers have joined. Keep handles in their owner while
        // polling: an outer shutdown deadline may cancel this wait and retry.
        let janitor = self.janitor.lock().unwrap().take();
        if let Some(janitor) = janitor {
            self.jobs.lock().unwrap().push(janitor);
        }
        std::future::poll_fn(|cx| {
            let mut jobs = self.jobs.lock().unwrap();
            let mut index = 0;
            while index < jobs.len() {
                if Pin::new(&mut jobs[index]).poll(cx).is_ready() {
                    drop(jobs.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            if jobs.is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
    pub fn release_stopped_storage(&self) {
        self.root_lock.lock().unwrap().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_wait_retains_unfinished_job_ownership() {
        let files = Files::new(None);
        let finish = Arc::new(tokio::sync::Notify::new());
        let signal = finish.clone();
        files.jobs.lock().unwrap().push(tokio::spawn(async move {
            signal.notified().await;
        }));
        for _ in 0..2 {
            assert!(
                tokio::time::timeout(Duration::from_millis(5), files.wait())
                    .await
                    .is_err()
            );
        }
        finish.notify_one();
        tokio::time::timeout(Duration::from_secs(1), files.wait())
            .await
            .unwrap();
        assert!(files.jobs.lock().unwrap().is_empty());
    }

    #[test]
    fn persistence_failure_keeps_all_file_metadata_private() {
        let root = std::env::temp_dir().join(format!(
            "rdh-browser-disclosure-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        crate::storage::create_private_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let files = Files::new(Some(&root));
        files.prepare().unwrap();
        let ai = Principal {
            version: 1,
            owner: "a".repeat(64),
            user: "b".repeat(64),
            channel: "ai".into(),
        };
        for digit in ['1', '2'] {
            let meta = Metadata {
                version: 1,
                id: digit.to_string().repeat(32),
                owner: ai.owner.clone(),
                user: ai.user.clone(),
                session_id: "session".into(),
                tab_id: "tab".into(),
                origin: "https://example.test".into(),
                name: "fixture.bin".into(),
                size_bytes: 0,
                sha256: String::new(),
                state: "ready".into(),
                created_at: now(),
                expires_at: now() + 60,
                ai_visible: true,
                upload: None,
                cancellation_scope: None,
            };
            let path = files.path(&meta.id).unwrap();
            crate::storage::create_private_dir(&path).unwrap();
            files.save(&meta).unwrap();
            files.catalog.lock().unwrap().entries.insert(
                meta.id.clone(),
                Entry {
                    meta,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        let blocked = files.path(&"1".repeat(32)).unwrap().join("metadata.json");
        fs::remove_file(&blocked).unwrap();
        fs::create_dir(&blocked).unwrap();
        for visible in [false, true] {
            assert!(files.disclosure("session", visible).is_err());
            assert_eq!(files.list(&ai).unwrap()["files"], json!([]));
            let human = Principal {
                channel: "human".into(),
                ..ai.clone()
            };
            assert_eq!(
                files.list(&human).unwrap()["files"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
    }
}
