//! Reservations and bounded ingestion shared by native browser downloads.
use super::*;

pub(crate) struct Download {
    #[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
    pub(super) files: Arc<Files>,
    pub(super) meta: Metadata,
    pub(super) path: PathBuf,
    pub(super) cancel: Arc<AtomicBool>,
}
#[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
impl Download {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire) || self.files.stopping.load(Ordering::Acquire)
    }
    pub fn complete(&mut self, mut source: File) -> Result<()> {
        if source.metadata().map_err(io_error)?.len() != self.meta.size_bytes {
            return Err(Failure::new("browser_file_incomplete"));
        }
        let mut file =
            crate::storage::create_private_file(&self.path.join("content")).map_err(io_error)?;
        let mut hash = Sha256::new();
        let mut received = 0u64;
        let mut buffer = [0u8; CHUNK];
        loop {
            if self.cancelled() {
                return Err(Failure::new("browser_file_cancelled"));
            }
            let size = source.read(&mut buffer).map_err(io_error)?;
            if size == 0 {
                break;
            }
            received += size as u64;
            if received > self.meta.size_bytes {
                return Err(Failure::new("browser_file_limit"));
            }
            file.write_all(&buffer[..size]).map_err(io_error)?;
            hash.update(&buffer[..size]);
        }
        if received != self.meta.size_bytes {
            return Err(Failure::new("browser_file_incomplete"));
        }
        file.sync_all().map_err(io_error)?;
        let mut catalog = self.files.catalog.lock().unwrap();
        let entry = catalog
            .entries
            .get_mut(&self.meta.id)
            .ok_or_else(|| Failure::new("browser_file_unavailable"))?;
        if self.cancelled() {
            return Err(Failure::new("browser_file_cancelled"));
        }
        self.meta.ai_visible = entry.meta.ai_visible;
        self.meta.state = "ready".into();
        self.meta.sha256 = format!("{:x}", hash.finalize());
        self.files.save(&self.meta)?;
        entry.meta = self.meta.clone();
        Ok(())
    }
    pub fn fail(self, code: &'static str, native_stopped: bool) {
        if native_stopped {
            self.files.failure(self.meta, code);
        } else {
            // An uncertain native writer retains its quota until startup recovery.
            let mut catalog = self.files.catalog.lock().unwrap();
            let mut meta = self.meta;
            if let Some(entry) = catalog.entries.get(&meta.id) {
                meta.ai_visible = entry.meta.ai_visible;
            }
            catalog
                .failures
                .push_back((meta, "browser_cleanup_pending"));
            while catalog.failures.len() > MAX_FILES {
                catalog.failures.pop_front();
            }
        }
    }
}
