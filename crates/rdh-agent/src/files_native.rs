//! Protocol validation, encoding, search and mutation admission shared by file backends.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

#[cfg(unix)]
#[path = "files_posix.rs"]
mod platform;
#[cfg(windows)]
#[path = "files_windows.rs"]
mod platform;
#[cfg(all(windows, feature = "experimental-windows-processes"))]
pub(super) use platform::Opened as WindowsDirectory;
pub(super) use platform::Root;
use platform::{Opened, Parent, entries_bounded, make_directories, read_existing, write_atomic};

const MAX_FILE: usize = 1024 * 1024;
const MAX_CHUNK_FILE: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 10000;
const MAX_SAFE: u64 = 9007199254740991;

fn io_error(e: std::io::Error) -> String {
    // OS errors contain no requested paths or user file contents.
    format!("File operation failed: {e}")
}
#[cfg(unix)]
pub(super) fn working_directory(files: &Files, name: &str, path: &str) -> Result<File, String> {
    let root = files
        .roots
        .get(name)
        .ok_or_else(|| "Unknown root; use device_info to discover configured roots".to_owned())?;
    let directory = root.open(path, true)?;
    root.verify()?;
    Ok(directory)
}

#[cfg(all(windows, feature = "experimental-windows-processes"))]
pub(super) fn windows_working_directory(
    files: &Files,
    name: &str,
    path: &str,
) -> Result<WindowsDirectory, String> {
    let root = files
        .roots
        .get(name)
        .ok_or_else(|| "Unknown root; use device_info to discover configured roots".to_owned())?;
    let directory = root.working_directory(path)?;
    root.verify()?;
    Ok(directory)
}

pub(super) fn validate_search_path(files: &Files, name: &str, path: &str) -> Result<(), String> {
    if !files.roots.contains_key(name) {
        return Err("Unknown root; use device_info to discover configured roots".into());
    }
    normalize(path).map(|_| ())
}

pub(super) fn check_search_directory(files: &Files, name: &str, path: &str) -> Result<(), String> {
    let root = files
        .roots
        .get(name)
        .ok_or_else(|| "Unknown root; use device_info to discover configured roots".to_owned())?;
    let _directory = root.open(path, true)?;
    root.verify()
}

fn normalize(path: &str) -> Result<String, String> {
    if path.starts_with('/') || path.contains('\0') || path.encode_utf16().count() > 4096 {
        return Err("Path must be relative and contain no NUL bytes".into());
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        if part == ".." {
            return Err("Path must be relative to a root, with no '..' or NUL".into());
        }
        #[cfg(windows)]
        platform::validate_component(part)?;
        if !part.is_empty() && part != "." {
            parts.push(part);
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}
fn string<'a>(p: &'a Value, name: &str, default: Option<&'a str>) -> Result<&'a str, String> {
    match p.get(name) {
        None => default.ok_or_else(|| format!("Missing {name}")),
        Some(v) => v.as_str().ok_or_else(|| format!("Invalid {name}")),
    }
}
fn number(p: &Value, name: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    let Some(value) = p.get(name) else {
        return Ok(default);
    };
    // Protocol v1 accepts integral JSON numbers such as 1.0, but not bools.
    let n = value.as_f64().ok_or_else(|| format!("Invalid {name}"))?;
    if !n.is_finite() || n.fract() != 0.0 || n < min as f64 || n > max as f64 {
        return Err(format!("Invalid {name}"));
    }
    Ok(n as u64)
}
fn boolean(p: &Value, name: &str, default: bool) -> Result<bool, String> {
    match p.get(name) {
        None => Ok(default),
        Some(v) => v.as_bool().ok_or_else(|| format!("Invalid {name}")),
    }
}
fn root<'a>(files: &'a Files, params: &Value) -> Result<&'a Root, String> {
    params
        .get("root")
        .and_then(Value::as_str)
        .and_then(|name| files.roots.get(name))
        .ok_or_else(|| "Unknown root; use device_info to discover configured roots".into())
}

fn file_path(params: &Value) -> Result<&str, String> {
    params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "Path must be relative and contain no NUL bytes".into())
}
fn regular(file: &Opened, cap: u64) -> Result<MetadataFacts, String> {
    let m = platform::metadata(file)?;
    if !m.is_file {
        return Err("Only regular files are supported".into());
    }
    if m.size > cap {
        return Err(if cap == MAX_FILE as u64 {
            "File exceeds 1 MiB"
        } else {
            "File exceeds 64 MiB"
        }
        .into());
    }
    Ok(m)
}
fn read_full(file: &mut Opened) -> Result<Vec<u8>, String> {
    regular(file, MAX_FILE as u64)?;
    let mut bytes = Vec::new();
    file.take((MAX_FILE + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > MAX_FILE {
        return Err("File exceeds 1 MiB".into());
    }
    Ok(bytes)
}
// The old Node Agent embeds Number.toString() values in fingerprints.
// serde_json supplies the shortest rounded decimal digits; arrange those
// digits using ECMAScript's fixed/scientific notation thresholds. Metadata
// values are finite, including after converting the integer fields to f64.
// https://tc39.es/ecma262/2024/#sec-numeric-types-number-tostring
pub(super) fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let serialized = serde_json::to_string(&value).expect("finite metadata number");
    let (sign, number) = serialized
        .strip_prefix('-')
        .map_or(("", serialized.as_str()), |number| ("-", number));
    let (mantissa, exponent) = number.split_once('e').map_or((number, 0), |(part, exp)| {
        (part, exp.parse::<i32>().expect("JSON exponent"))
    });
    let decimal = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let all_digits = mantissa.replace('.', "");
    let first = all_digits
        .find(|digit| digit != '0')
        .expect("nonzero number");
    let position = decimal + exponent - first as i32;
    let digits = all_digits[first..].trim_end_matches('0');
    let length = digits.len() as i32;

    if length <= position && position <= 21 {
        format!("{sign}{digits}{}", "0".repeat((position - length) as usize))
    } else if position > 0 && position <= 21 {
        format!(
            "{sign}{}.{}",
            &digits[..position as usize],
            &digits[position as usize..]
        )
    } else if position > -6 && position <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-position) as usize))
    } else {
        let exp = position - 1;
        let fraction = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        let plus = if exp >= 0 { "+" } else { "" };
        format!("{sign}{}{fraction}e{plus}{exp}", &digits[..1])
    }
}

pub(super) fn js_unsigned(value: u64) -> String {
    if value <= 9_007_199_254_740_992 {
        value.to_string()
    } else {
        js_number(value as f64)
    }
}

fn fingerprint_parts(dev: u64, ino: u64, length: u64, modified_ms: f64) -> String {
    format!(
        "{}:{}:{}:{}",
        js_unsigned(dev),
        js_unsigned(ino),
        js_unsigned(length),
        js_number(modified_ms)
    )
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    id: u128,
}

struct MetadataFacts {
    identity: FileIdentity,
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    modified_ms: f64,
    is_file: bool,
    is_directory: bool,
}
impl MetadataFacts {
    fn fingerprint(&self) -> String {
        fingerprint_parts(self.dev, self.ino, self.size, self.modified_ms)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Encoding {
    Utf8,
    Le,
    Be,
}
impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Le => "utf-16le",
            Self::Be => "utf-16be",
        }
    }
    fn decode(self, b: &[u8]) -> Result<String, String> {
        let invalid = || {
            format!(
                "File is not valid {}; use read_file_chunk for exact bytes",
                self.name().to_uppercase()
            )
        };
        if self == Self::Utf8 {
            return std::str::from_utf8(b)
                .map(str::to_owned)
                .map_err(|_| invalid());
        }
        if !b.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let words: Vec<u16> = b
            .chunks_exact(2)
            .map(|x| {
                if self == Self::Le {
                    u16::from_le_bytes([x[0], x[1]])
                } else {
                    u16::from_be_bytes([x[0], x[1]])
                }
            })
            .collect();
        String::from_utf16(&words).map_err(|_| invalid())
    }
}
struct TextRange {
    text: String,
    encoding: Encoding,
    start: usize,
    end: usize,
}
fn text_range(bytes: &[u8], offset: u64, length: usize) -> Result<TextRange, String> {
    let encoding = if bytes.starts_with(&[0xff, 0xfe]) {
        Encoding::Le
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        Encoding::Be
    } else {
        Encoding::Utf8
    };
    encoding.decode(bytes)?; // Validate the whole file, preserving any BOM.
    let start = offset.min(bytes.len() as u64) as usize;
    if encoding != Encoding::Utf8 && !start.is_multiple_of(2) {
        return Err("offset must be aligned to a UTF-16 code unit".into());
    }
    let requested_end = (start + length).min(bytes.len());
    for trim in 0..=3 {
        let Some(end) = requested_end.checked_sub(trim).filter(|end| *end >= start) else {
            break;
        };
        if let Ok(text) = encoding.decode(&bytes[start..end]) {
            if end == start && start < bytes.len() {
                break;
            }
            return Ok(TextRange {
                text,
                encoding,
                start,
                end,
            });
        }
    }
    Err(format!(
        "Byte range cannot fit complete {} characters; align offset or increase length",
        encoding.name().to_uppercase()
    ))
}
fn text_value(path: &str, bytes: &[u8], r: &TextRange) -> Value {
    json!({"path":path,"content":r.text,"content_base64":STANDARD.encode(&bytes[r.start..r.end]),
        "content_encoding":r.encoding.name(),"utf8_valid":r.encoding == Encoding::Utf8,
        "offset":r.start,"next_offset":r.end,"size_bytes":bytes.len(),"truncated":r.end < bytes.len()})
}
fn read(files: &Files, p: &Value) -> Result<Value, String> {
    let invalid_range = |_| "Invalid byte offset or length (1..65536)".to_string();
    let offset = number(p, "offset", 0, 0, MAX_SAFE).map_err(invalid_range)?;
    let length = number(p, "length", 32768, 1, 65536).map_err(invalid_range)?;
    let root = root(files, p)?;
    let path = file_path(p)?;
    if normalize(path)? == "." {
        return Err("A file name is required".into());
    }
    let bytes = read_full(&mut root.open(path, false)?)?;
    root.verify()?;
    let range = text_range(&bytes, offset, length as usize)?;
    let mut result = text_value(path, &bytes, &range);
    result["sha256"] = json!(format!("{:x}", Sha256::digest(&bytes)));
    Ok(result)
}
fn info(files: &Files, p: &Value) -> Result<Value, String> {
    let root = root(files, p)?;
    let path = file_path(p)?;
    let m = root.metadata(path)?;
    if !m.is_file && !m.is_directory {
        return Err("Only regular files and directories are supported".into());
    }
    root.verify()?;
    Ok(
        json!({"path":path,"type":if m.is_file {"file"} else {"directory"},
        "size_bytes":m.size,"mode":m.mode & 0o777,"mtime_ms":m.modified_ms,
        "read_only":!files.allow_write,"fingerprint":m.fingerprint()}),
    )
}
fn chunk(files: &Files, p: &Value) -> Result<Value, String> {
    let encoding = if p.get("encoding").is_none_or(Value::is_null) {
        "both"
    } else {
        string(p, "encoding", None).map_err(|_| "Invalid chunk encoding")?
    };
    if !matches!(encoding, "both" | "text" | "base64") {
        return Err("Invalid chunk encoding".into());
    }
    let invalid_range = |_| "Invalid byte offset or length".to_string();
    let offset = number(p, "offset", 0, 0, MAX_SAFE).map_err(invalid_range)?;
    let length = number(p, "length", 32768, 1, 65536).map_err(invalid_range)?;
    let from_end = boolean(p, "from_end", false).map_err(invalid_range)?;
    let root = root(files, p)?;
    let path = file_path(p)?;
    if normalize(path)? == "." {
        return Err("A file name is required".into());
    }
    let metadata = root.metadata(path)?;
    if !metadata.is_file {
        return Err("Only regular files are supported".into());
    }
    let end = if from_end {
        metadata.size.saturating_sub(offset)
    } else {
        metadata.size
    };
    let start = if from_end {
        end.saturating_sub(length)
    } else {
        offset
    };
    let available = end.saturating_sub(start).min(length);
    if from_end && available == 0 {
        root.verify()?;
        let mut result = json!({"path":path,"utf8_valid":true,"content_encoding":"utf-8",
            "offset":start,"next_offset":start,"size_bytes":metadata.size,"truncated":false,
            "read_only":true,"fingerprint":metadata.fingerprint()});
        if encoding != "base64" {
            result["content"] = json!("");
        }
        if encoding != "text" {
            result["content_base64"] = json!("");
        }
        return Ok(result);
    }
    let mut file = root.open(path, false)?;
    let m = regular(&file, MAX_CHUNK_FILE)?;
    if metadata.identity != m.identity {
        return Err("File changed while opening".into());
    }
    file.seek(SeekFrom::Start(start)).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.take(available)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    root.verify()?;
    let valid = std::str::from_utf8(&bytes).is_ok();
    let mut result = json!({"path":path,"utf8_valid":valid,"content_encoding":if valid {"utf-8"} else {"utf-8-replacement"},
        "offset":start,"next_offset":start + bytes.len() as u64,"size_bytes":m.size,
        "truncated": !(from_end && available == 0) && start + (bytes.len() as u64) < m.size,
        "read_only":true,"fingerprint":m.fingerprint()});
    if encoding != "base64" {
        result["content"] = json!(String::from_utf8_lossy(&bytes));
    }
    if encoding != "text" {
        result["content_base64"] = json!(STANDARD.encode(&bytes));
    }
    Ok(result)
}

struct Entry {
    name: String,
    kind: &'static str,
    size: u64,
}
fn entries(file: Opened) -> Result<Vec<Entry>, String> {
    let (entries, truncated) = entries_bounded(file, MAX_ENTRIES, None)?;
    if truncated {
        Err("Directory exceeds 10000 entries".into())
    } else {
        Ok(entries)
    }
}

pub(super) fn search_entries(
    files: &Files,
    name: &str,
    path: &str,
    remaining: usize,
    cancelled: &std::sync::atomic::AtomicBool,
    deadline: Instant,
) -> Result<(Vec<SearchEntry>, bool), String> {
    let root = files.roots.get(name).ok_or("Unknown root")?;
    let (mut entries, truncated) = entries_bounded(
        root.open(path, true)?,
        remaining,
        Some((cancelled, deadline)),
    )?;
    root.verify()?;
    entries.sort_by(|a, b| a.name.encode_utf16().cmp(b.name.encode_utf16()));
    Ok((
        entries
            .into_iter()
            .map(|entry| SearchEntry {
                name: entry.name,
                kind: entry.kind,
            })
            .collect(),
        truncated,
    ))
}

pub(super) fn search_read(files: &Files, name: &str, path: &str) -> Result<Vec<u8>, String> {
    let root = files.roots.get(name).ok_or("Unknown root")?;
    let result = read_full(&mut root.open(path, false)?)?;
    root.verify()?;
    Ok(result)
}
fn list(files: &Files, p: &Value) -> Result<Value, String> {
    let invalid_page = || "Invalid offset/limit".to_string();
    let offset = match p.get("offset") {
        None => 0.0,
        Some(value) => value.as_f64().ok_or_else(invalid_page)?,
    };
    if !offset.is_finite() || offset.fract() != 0.0 || offset < 0.0 {
        return Err(invalid_page());
    }
    let limit = number(p, "limit", 100, 1, 1000).map_err(|_| invalid_page())? as usize;
    let root = root(files, p)?;
    let path = match p.get("path") {
        None => ".",
        Some(value) => value
            .as_str()
            .ok_or("Path must be relative and contain no NUL bytes")?,
    };
    let mut entries = entries(root.open(path, true)?)?;
    root.verify()?;
    // UTF-8 lexical order agrees with Unicode scalar-value order.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let total = entries.len();
    // The legacy API accepts any nonnegative integral JS number. An
    // offset beyond this bounded directory is equivalent to its end.
    let offset = if offset >= total as f64 {
        total
    } else {
        offset as usize
    };
    let selected: Vec<Value> = entries
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|e| json!({"name":e.name,"type":e.kind,"size_bytes":e.size}))
        .collect();
    Ok(
        json!({"entries":selected,"total":total,"next_offset":if offset + limit < total { Some(offset + limit) } else { None }}),
    )
}

fn multiple(files: &Files, p: &Value) -> Result<Value, String> {
    let canonical = p.get("paths").and_then(Value::as_array);
    let requests = if let Some(paths) = canonical {
        paths
            .iter()
            .map(|path| {
                let mut request = json!({"root":p.get("root"),"path":path});
                for field in ["offset", "length"] {
                    if let Some(v) = p.get(field) {
                        request[field] = v.clone();
                    }
                }
                request
            })
            .collect::<Vec<_>>()
    } else {
        p.get("files")
            .and_then(Value::as_array)
            .ok_or("Invalid files/total_limit")?
            .clone()
    };
    let budget = if p.get("total_limit").is_some_and(Value::is_null) {
        256 * 1024
    } else {
        number(p, "total_limit", 256 * 1024, 1, 256 * 1024)? as usize
    };
    if requests.is_empty() || requests.len() > 16 {
        return Err("Invalid files/total_limit".into());
    }
    let mut used = 0;
    let mut truncated = false;
    let mut results = Vec::new();
    for mut request in requests {
        // The batch API uses nullish defaults, unlike read_file's strict
        // offset/length validation. Preserve this distinction.
        for name in ["offset", "length"] {
            if request.get(name).is_some_and(Value::is_null)
                && let Some(object) = request.as_object_mut()
            {
                object.remove(name);
            }
        }
        let outcome = (|| -> Result<Value, String> {
            if !request.is_object() || !request["root"].is_string() || !request["path"].is_string()
            {
                return Err("Invalid file request".into());
            }
            let remaining = budget - used;
            if remaining == 0 {
                truncated = true;
                return Err("Total response limit exceeded".into());
            }
            let invalid_range = |_| "Invalid byte offset or length".to_string();
            let length = number(
                &request,
                "length",
                if canonical.is_some() { 16384 } else { 32768 },
                1,
                if canonical.is_some() { 16384 } else { 65536 },
            )
            .map_err(invalid_range)?;
            let offset = number(&request, "offset", 0, 0, MAX_SAFE).map_err(invalid_range)?;
            let path = string(&request, "path", None)?;
            let root = root(files, &request)?;
            if normalize(path)? == "." {
                return Err("A file name is required".into());
            }
            let mut file = root.open(path, false)?;
            let metadata = regular(&file, MAX_FILE as u64)?;
            let bytes = read_full(&mut file)?;
            root.verify()?;
            let mut range = text_range(&bytes, offset, length as usize)?;
            if range.text.len() > remaining {
                let mut end = 0;
                for (index, c) in range.text.char_indices() {
                    if index + c.len_utf8() > remaining {
                        break;
                    }
                    end = index + c.len_utf8();
                }
                truncated = true;
                if end == 0 {
                    return Err("Output budget cannot fit the next UTF-8 character".into());
                }
                range.text.truncate(end);
                let source_bytes = if range.encoding == Encoding::Utf8 {
                    end
                } else {
                    range.text.encode_utf16().count() * 2
                };
                range.end = range.start + source_bytes;
            }
            used += range.text.len();
            truncated |= range.end < bytes.len();
            let mut result = text_value(path, &bytes, &range);
            result["read_only"] = json!(true);
            result["fingerprint"] = json!(metadata.fingerprint());
            Ok(result)
        })();
        results.push(match outcome {
            Ok(value) => value,
            Err(error) => {
                let mut error_value = json!({"error":error});
                if let Some(path) = request.get("path") {
                    error_value["path"] = path.clone();
                }
                error_value
            }
        });
    }
    Ok(json!({"files":results,"total_bytes":used,"truncated":truncated}))
}

struct Search<'a> {
    root: &'a Root,
    query: &'a str,
    folded: String,
    content: bool,
    limit: usize,
    deadline: Instant,
    visited: usize,
    truncated: bool,
    matches: Vec<Value>,
}
impl Search<'_> {
    fn walk(&mut self, path: &str, depth: usize) {
        if depth > 30 {
            self.truncated = true;
            return;
        }
        let Ok(entries) = self.root.open(path, true).and_then(entries) else {
            return;
        };
        for entry in entries {
            self.visited += 1;
            if self.visited > MAX_ENTRIES
                || Instant::now() > self.deadline
                || self.matches.len() >= self.limit
            {
                self.truncated = true;
                return;
            }
            if entry.kind == "symlink" {
                continue;
            }
            let relative = if path == "." {
                entry.name.clone()
            } else {
                format!("{path}/{}", entry.name)
            };
            if !self.content && casefold(&entry.name).contains(&self.folded) {
                self.matches.push(json!({"path":relative}));
            }
            if entry.kind == "directory" {
                self.walk(&relative, depth + 1);
            } else if self.content && entry.kind == "file" {
                let Ok(bytes) = self
                    .root
                    .open(&relative, false)
                    .and_then(|mut f| read_full(&mut f))
                else {
                    continue;
                };
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                let text = text.strip_prefix('\u{feff}').unwrap_or(text);
                let mut lines = text.split('\n').enumerate().peekable();
                while let Some((index, line)) = lines.next() {
                    if Instant::now() > self.deadline {
                        self.truncated = true;
                        return;
                    }
                    let line = if lines.peek().is_some() {
                        line.strip_suffix('\r').unwrap_or(line)
                    } else {
                        line
                    };
                    if line.contains(self.query) {
                        // JS slices UTF-16 code units. Avoid generating an
                        // unpaired surrogate at the 500-unit boundary.
                        let mut units = 0;
                        let preview: String = line
                            .chars()
                            .take_while(|c| {
                                units += c.len_utf16();
                                units <= 500
                            })
                            .collect();
                        self.matches
                            .push(json!({"path":relative,"line":index+1,"text":preview}));
                        if self.matches.len() >= self.limit {
                            self.truncated = true;
                            return;
                        }
                    }
                }
            }
            if self.visited > MAX_ENTRIES || Instant::now() > self.deadline {
                self.truncated = true;
                return;
            }
        }
    }
}
fn search(files: &Files, p: &Value) -> Result<Value, String> {
    let invalid = || "Query must be 1..512 characters; limit 1..200".to_string();
    let query = p
        .get("query")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.encode_utf16().count() <= 512)
        .ok_or_else(invalid)?;
    let limit = number(p, "limit", 100, 1, 200).map_err(|_| invalid())? as usize;
    let content = boolean(p, "content", false)?;
    let root = root(files, p)?;
    let path = match p.get("path") {
        None => ".",
        Some(value) => value
            .as_str()
            .ok_or("Path must be relative and contain no NUL bytes")?,
    };
    drop(root.open(path, true)?);
    let mut search = Search {
        root,
        query,
        folded: casefold(query),
        content,
        limit,
        deadline: Instant::now() + Duration::from_secs(3),
        visited: 0,
        truncated: false,
        matches: Vec::new(),
    };
    search.walk(path, 0);
    root.verify()?;
    search.matches.truncate(limit);
    Ok(json!({"matches":search.matches,"visited":search.visited,"truncated":search.truncated}))
}
fn mutation_result(result: Result<Value, String>) -> Result<Value, String> {
    // Includes partial recursive directory creation, commit followed by a
    // failed fsync/identity check, and failed temporary cleanup. RPC must not
    // report not_sent/no-effect for errors returned from this phase.
    result.map_err(|error| {
        format!("File mutation failed; filesystem effects may have occurred: {error}")
    })
}
type Control<'a> = Option<(Instant, &'a std::sync::atomic::AtomicBool)>;

fn lock_mutation<'a>(
    files: &'a Files,
    control: Control<'_>,
) -> Result<std::sync::MutexGuard<'a, ()>, String> {
    if !files.allow_write {
        return Err("File writes are disabled in this device's configuration".into());
    }
    let Some((deadline, closing)) = control else {
        return files
            .mutation
            .lock()
            .map_err(|_| "File mutation state is uncertain after an interrupted operation".into());
    };
    loop {
        if closing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(MUTATION_SHUTTING_DOWN.into());
        }
        if Instant::now() >= deadline {
            return Err(MUTATION_DEADLINE_EXPIRED.into());
        }
        match files.mutation.try_lock() {
            Ok(guard) => {
                if closing.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(MUTATION_SHUTTING_DOWN.into());
                }
                if Instant::now() >= deadline {
                    return Err(MUTATION_DEADLINE_EXPIRED.into());
                }
                return Ok(guard);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(
                    "File mutation state is uncertain after an interrupted operation".into(),
                );
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
}
fn written(path: &str, bytes: &[u8]) -> Value {
    json!({"path":path,"size_bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))})
}
fn write(files: &Files, p: &Value, control: Control<'_>) -> Result<Value, String> {
    if !files.allow_write {
        return Err("File writes are disabled in this device's configuration".into());
    }
    let content = p
        .get("content")
        .and_then(Value::as_str)
        .ok_or("content must be a string")?;
    let expected = match p.get("expected_sha256") {
        None => "",
        Some(value) => value.as_str().ok_or("expected_sha256 must be a string")?,
    };
    if content.len() > MAX_FILE {
        return Err("Content exceeds 1 MiB".into());
    }
    let root = root(files, p)?;
    let path = file_path(p)?;
    normalize(path)?;
    let _lock = lock_mutation(files, control)?;
    mutation_result((|| {
        let (parent, name) = Parent::file(root, path, expected.is_empty())?;
        let previous = read_existing(&parent, &name)?;
        match previous.as_ref() {
            None if !expected.is_empty() => return Err("File no longer exists; read it again before updating".into()),
            Some(previous) if format!("{:x}",Sha256::digest(&previous.bytes)) != expected =>
                return Err("Content conflict: supply the SHA-256 from read_file; empty hash means create only".into()),
            _ => {},
        }
        write_atomic(&parent, &name, content.as_bytes(), previous.as_ref())?;
        Ok(written(path, content.as_bytes()))
    })())
}
fn edit(files: &Files, p: &Value, control: Control<'_>) -> Result<Value, String> {
    let old_value = p.get("old_text");
    if old_value.is_none_or(Value::is_null)
        || old_value == Some(&Value::Bool(false))
        || old_value.and_then(Value::as_f64) == Some(0.0)
        || old_value.and_then(Value::as_str) == Some("")
    {
        return Err("old_text cannot be empty".into());
    }
    let old = old_value
        .and_then(Value::as_str)
        .ok_or("old_text and new_text must be strings")?;
    let new = p
        .get("new_text")
        .and_then(Value::as_str)
        .ok_or("old_text and new_text must be strings")?;
    let expected = p.get("expected_sha256").and_then(Value::as_str);
    let root = root(files, p)?;
    let path = file_path(p)?;
    normalize(path)?;
    let _lock = lock_mutation(files, control)?;
    mutation_result((|| {
        let (parent, name) = Parent::file(root, path, false)?;
        let previous =
            read_existing(&parent, &name)?.ok_or("File no longer exists; read_file again")?;
        let actual = format!("{:x}", Sha256::digest(&previous.bytes));
        if expected != Some(actual.as_str()) {
            return Err("Content conflict: read_file again".into());
        }
        // from_utf8 deliberately retains the BOM, matching ignoreBOM:true.
        let text = std::str::from_utf8(&previous.bytes).map_err(|_| "File is not valid UTF-8")?;
        let first = text.find(old).ok_or("old_text must match exactly once")?;
        let end = first + old.len();
        if text[end..].contains(old) {
            return Err("old_text must match exactly once".into());
        }
        let size = text
            .len()
            .checked_sub(old.len())
            .and_then(|n| n.checked_add(new.len()))
            .ok_or("Content exceeds 1 MiB")?;
        if size > MAX_FILE {
            return Err("Content exceeds 1 MiB".into());
        }
        let mut edited = String::with_capacity(size);
        edited.push_str(&text[..first]);
        edited.push_str(new);
        edited.push_str(&text[end..]);
        write_atomic(&parent, &name, edited.as_bytes(), Some(&previous))?;
        Ok(written(path, edited.as_bytes()))
    })())
}
fn mkdir(files: &Files, p: &Value, control: Control<'_>) -> Result<Value, String> {
    if !files.allow_write {
        return Err("File writes are disabled".into());
    }
    let root = root(files, p)?;
    let path = file_path(p)?;
    normalize(path)?;
    let _lock = lock_mutation(files, control)?;
    mutation_result(make_directories(root, path).map(|()| json!({"created":path})))
}

fn move_file(files: &Files, p: &Value, control: Control<'_>) -> Result<Value, String> {
    let modern = p.get("source").is_some() || p.get("destination").is_some();
    if modern
        && ["path", "destination_root", "destination_path"]
            .iter()
            .any(|key| p.get(key).is_some())
    {
        return Err("Mixed move parameter formats are not allowed".into());
    }
    let move_parameter = |name| {
        p.get(name)
            .and_then(Value::as_str)
            .ok_or("Invalid move parameters")
    };
    let root_name = move_parameter("root")?;
    let (source, destination_root, destination) = if modern {
        (
            move_parameter("source")?,
            root_name,
            move_parameter("destination")?,
        )
    } else {
        (
            move_parameter("path")?,
            move_parameter("destination_root")?,
            move_parameter("destination_path")?,
        )
    };
    if !files.allow_write {
        return Err("File writes are disabled in this device's configuration".into());
    }
    if root_name != destination_root {
        return Err("Moving files across roots is disabled".into());
    }
    let root = root(files, p)?;
    normalize(source)?;
    normalize(destination)?;
    let _lock = lock_mutation(files, control)?;
    mutation_result((|| {
        platform::move_file(root, source, destination)?;
        Ok(json!({"path":source,"destination_path":destination,"moved":true}))
    })())
}

pub(super) fn dispatch(
    files: &Files,
    operation: &str,
    p: &Value,
    control: Control<'_>,
) -> Result<Value, String> {
    if !p.is_object() {
        return Err("Parameters must be an object".into());
    }
    match operation {
        "read_file" => read(files, p),
        "file_info" => info(files, p),
        "read_file_chunk" => chunk(files, p),
        "list_directory" => list(files, p),
        "read_multiple_files" => multiple(files, p),
        "search_files" => search(files, p),
        "write_file" => write(files, p, control),
        "edit_file" => edit(files, p, control),
        "make_directory" => mkdir(files, p, control),
        "move_file" => move_file(files, p, control),
        _ => Err("Unsupported file operation".into()),
    }
}

// Exact exception mappings from src/casefold.ts (Unicode 13).
pub(super) fn casefold(value: &str) -> String {
    let mut result = String::new();
    for c in value.chars() {
        match c {
            '\u{b5}' => result.push('\u{3bc}'),
            '\u{df}' => result.push_str("\u{73}\u{73}"),
            '\u{149}' => result.push_str("\u{2bc}\u{6e}"),
            '\u{17f}' => result.push('\u{73}'),
            '\u{1f0}' => result.push_str("\u{6a}\u{30c}"),
            '\u{345}' => result.push('\u{3b9}'),
            '\u{390}' => result.push_str("\u{3b9}\u{308}\u{301}"),
            '\u{3b0}' => result.push_str("\u{3c5}\u{308}\u{301}"),
            '\u{3c2}' => result.push('\u{3c3}'),
            '\u{3d0}' => result.push('\u{3b2}'),
            '\u{3d1}' => result.push('\u{3b8}'),
            '\u{3d5}' => result.push('\u{3c6}'),
            '\u{3d6}' => result.push('\u{3c0}'),
            '\u{3f0}' => result.push('\u{3ba}'),
            '\u{3f1}' => result.push('\u{3c1}'),
            '\u{3f5}' => result.push('\u{3b5}'),
            '\u{587}' => result.push_str("\u{565}\u{582}"),
            '\u{13a0}' => result.push('\u{13a0}'),
            '\u{13a1}' => result.push('\u{13a1}'),
            '\u{13a2}' => result.push('\u{13a2}'),
            '\u{13a3}' => result.push('\u{13a3}'),
            '\u{13a4}' => result.push('\u{13a4}'),
            '\u{13a5}' => result.push('\u{13a5}'),
            '\u{13a6}' => result.push('\u{13a6}'),
            '\u{13a7}' => result.push('\u{13a7}'),
            '\u{13a8}' => result.push('\u{13a8}'),
            '\u{13a9}' => result.push('\u{13a9}'),
            '\u{13aa}' => result.push('\u{13aa}'),
            '\u{13ab}' => result.push('\u{13ab}'),
            '\u{13ac}' => result.push('\u{13ac}'),
            '\u{13ad}' => result.push('\u{13ad}'),
            '\u{13ae}' => result.push('\u{13ae}'),
            '\u{13af}' => result.push('\u{13af}'),
            '\u{13b0}' => result.push('\u{13b0}'),
            '\u{13b1}' => result.push('\u{13b1}'),
            '\u{13b2}' => result.push('\u{13b2}'),
            '\u{13b3}' => result.push('\u{13b3}'),
            '\u{13b4}' => result.push('\u{13b4}'),
            '\u{13b5}' => result.push('\u{13b5}'),
            '\u{13b6}' => result.push('\u{13b6}'),
            '\u{13b7}' => result.push('\u{13b7}'),
            '\u{13b8}' => result.push('\u{13b8}'),
            '\u{13b9}' => result.push('\u{13b9}'),
            '\u{13ba}' => result.push('\u{13ba}'),
            '\u{13bb}' => result.push('\u{13bb}'),
            '\u{13bc}' => result.push('\u{13bc}'),
            '\u{13bd}' => result.push('\u{13bd}'),
            '\u{13be}' => result.push('\u{13be}'),
            '\u{13bf}' => result.push('\u{13bf}'),
            '\u{13c0}' => result.push('\u{13c0}'),
            '\u{13c1}' => result.push('\u{13c1}'),
            '\u{13c2}' => result.push('\u{13c2}'),
            '\u{13c3}' => result.push('\u{13c3}'),
            '\u{13c4}' => result.push('\u{13c4}'),
            '\u{13c5}' => result.push('\u{13c5}'),
            '\u{13c6}' => result.push('\u{13c6}'),
            '\u{13c7}' => result.push('\u{13c7}'),
            '\u{13c8}' => result.push('\u{13c8}'),
            '\u{13c9}' => result.push('\u{13c9}'),
            '\u{13ca}' => result.push('\u{13ca}'),
            '\u{13cb}' => result.push('\u{13cb}'),
            '\u{13cc}' => result.push('\u{13cc}'),
            '\u{13cd}' => result.push('\u{13cd}'),
            '\u{13ce}' => result.push('\u{13ce}'),
            '\u{13cf}' => result.push('\u{13cf}'),
            '\u{13d0}' => result.push('\u{13d0}'),
            '\u{13d1}' => result.push('\u{13d1}'),
            '\u{13d2}' => result.push('\u{13d2}'),
            '\u{13d3}' => result.push('\u{13d3}'),
            '\u{13d4}' => result.push('\u{13d4}'),
            '\u{13d5}' => result.push('\u{13d5}'),
            '\u{13d6}' => result.push('\u{13d6}'),
            '\u{13d7}' => result.push('\u{13d7}'),
            '\u{13d8}' => result.push('\u{13d8}'),
            '\u{13d9}' => result.push('\u{13d9}'),
            '\u{13da}' => result.push('\u{13da}'),
            '\u{13db}' => result.push('\u{13db}'),
            '\u{13dc}' => result.push('\u{13dc}'),
            '\u{13dd}' => result.push('\u{13dd}'),
            '\u{13de}' => result.push('\u{13de}'),
            '\u{13df}' => result.push('\u{13df}'),
            '\u{13e0}' => result.push('\u{13e0}'),
            '\u{13e1}' => result.push('\u{13e1}'),
            '\u{13e2}' => result.push('\u{13e2}'),
            '\u{13e3}' => result.push('\u{13e3}'),
            '\u{13e4}' => result.push('\u{13e4}'),
            '\u{13e5}' => result.push('\u{13e5}'),
            '\u{13e6}' => result.push('\u{13e6}'),
            '\u{13e7}' => result.push('\u{13e7}'),
            '\u{13e8}' => result.push('\u{13e8}'),
            '\u{13e9}' => result.push('\u{13e9}'),
            '\u{13ea}' => result.push('\u{13ea}'),
            '\u{13eb}' => result.push('\u{13eb}'),
            '\u{13ec}' => result.push('\u{13ec}'),
            '\u{13ed}' => result.push('\u{13ed}'),
            '\u{13ee}' => result.push('\u{13ee}'),
            '\u{13ef}' => result.push('\u{13ef}'),
            '\u{13f0}' => result.push('\u{13f0}'),
            '\u{13f1}' => result.push('\u{13f1}'),
            '\u{13f2}' => result.push('\u{13f2}'),
            '\u{13f3}' => result.push('\u{13f3}'),
            '\u{13f4}' => result.push('\u{13f4}'),
            '\u{13f5}' => result.push('\u{13f5}'),
            '\u{13f8}' => result.push('\u{13f0}'),
            '\u{13f9}' => result.push('\u{13f1}'),
            '\u{13fa}' => result.push('\u{13f2}'),
            '\u{13fb}' => result.push('\u{13f3}'),
            '\u{13fc}' => result.push('\u{13f4}'),
            '\u{13fd}' => result.push('\u{13f5}'),
            '\u{1c80}' => result.push('\u{432}'),
            '\u{1c81}' => result.push('\u{434}'),
            '\u{1c82}' => result.push('\u{43e}'),
            '\u{1c83}' => result.push('\u{441}'),
            '\u{1c84}' => result.push('\u{442}'),
            '\u{1c85}' => result.push('\u{442}'),
            '\u{1c86}' => result.push('\u{44a}'),
            '\u{1c87}' => result.push('\u{463}'),
            '\u{1c88}' => result.push('\u{a64b}'),
            '\u{1e96}' => result.push_str("\u{68}\u{331}"),
            '\u{1e97}' => result.push_str("\u{74}\u{308}"),
            '\u{1e98}' => result.push_str("\u{77}\u{30a}"),
            '\u{1e99}' => result.push_str("\u{79}\u{30a}"),
            '\u{1e9a}' => result.push_str("\u{61}\u{2be}"),
            '\u{1e9b}' => result.push('\u{1e61}'),
            '\u{1e9e}' => result.push_str("\u{73}\u{73}"),
            '\u{1f50}' => result.push_str("\u{3c5}\u{313}"),
            '\u{1f52}' => result.push_str("\u{3c5}\u{313}\u{300}"),
            '\u{1f54}' => result.push_str("\u{3c5}\u{313}\u{301}"),
            '\u{1f56}' => result.push_str("\u{3c5}\u{313}\u{342}"),
            '\u{1f80}' => result.push_str("\u{1f00}\u{3b9}"),
            '\u{1f81}' => result.push_str("\u{1f01}\u{3b9}"),
            '\u{1f82}' => result.push_str("\u{1f02}\u{3b9}"),
            '\u{1f83}' => result.push_str("\u{1f03}\u{3b9}"),
            '\u{1f84}' => result.push_str("\u{1f04}\u{3b9}"),
            '\u{1f85}' => result.push_str("\u{1f05}\u{3b9}"),
            '\u{1f86}' => result.push_str("\u{1f06}\u{3b9}"),
            '\u{1f87}' => result.push_str("\u{1f07}\u{3b9}"),
            '\u{1f88}' => result.push_str("\u{1f00}\u{3b9}"),
            '\u{1f89}' => result.push_str("\u{1f01}\u{3b9}"),
            '\u{1f8a}' => result.push_str("\u{1f02}\u{3b9}"),
            '\u{1f8b}' => result.push_str("\u{1f03}\u{3b9}"),
            '\u{1f8c}' => result.push_str("\u{1f04}\u{3b9}"),
            '\u{1f8d}' => result.push_str("\u{1f05}\u{3b9}"),
            '\u{1f8e}' => result.push_str("\u{1f06}\u{3b9}"),
            '\u{1f8f}' => result.push_str("\u{1f07}\u{3b9}"),
            '\u{1f90}' => result.push_str("\u{1f20}\u{3b9}"),
            '\u{1f91}' => result.push_str("\u{1f21}\u{3b9}"),
            '\u{1f92}' => result.push_str("\u{1f22}\u{3b9}"),
            '\u{1f93}' => result.push_str("\u{1f23}\u{3b9}"),
            '\u{1f94}' => result.push_str("\u{1f24}\u{3b9}"),
            '\u{1f95}' => result.push_str("\u{1f25}\u{3b9}"),
            '\u{1f96}' => result.push_str("\u{1f26}\u{3b9}"),
            '\u{1f97}' => result.push_str("\u{1f27}\u{3b9}"),
            '\u{1f98}' => result.push_str("\u{1f20}\u{3b9}"),
            '\u{1f99}' => result.push_str("\u{1f21}\u{3b9}"),
            '\u{1f9a}' => result.push_str("\u{1f22}\u{3b9}"),
            '\u{1f9b}' => result.push_str("\u{1f23}\u{3b9}"),
            '\u{1f9c}' => result.push_str("\u{1f24}\u{3b9}"),
            '\u{1f9d}' => result.push_str("\u{1f25}\u{3b9}"),
            '\u{1f9e}' => result.push_str("\u{1f26}\u{3b9}"),
            '\u{1f9f}' => result.push_str("\u{1f27}\u{3b9}"),
            '\u{1fa0}' => result.push_str("\u{1f60}\u{3b9}"),
            '\u{1fa1}' => result.push_str("\u{1f61}\u{3b9}"),
            '\u{1fa2}' => result.push_str("\u{1f62}\u{3b9}"),
            '\u{1fa3}' => result.push_str("\u{1f63}\u{3b9}"),
            '\u{1fa4}' => result.push_str("\u{1f64}\u{3b9}"),
            '\u{1fa5}' => result.push_str("\u{1f65}\u{3b9}"),
            '\u{1fa6}' => result.push_str("\u{1f66}\u{3b9}"),
            '\u{1fa7}' => result.push_str("\u{1f67}\u{3b9}"),
            '\u{1fa8}' => result.push_str("\u{1f60}\u{3b9}"),
            '\u{1fa9}' => result.push_str("\u{1f61}\u{3b9}"),
            '\u{1faa}' => result.push_str("\u{1f62}\u{3b9}"),
            '\u{1fab}' => result.push_str("\u{1f63}\u{3b9}"),
            '\u{1fac}' => result.push_str("\u{1f64}\u{3b9}"),
            '\u{1fad}' => result.push_str("\u{1f65}\u{3b9}"),
            '\u{1fae}' => result.push_str("\u{1f66}\u{3b9}"),
            '\u{1faf}' => result.push_str("\u{1f67}\u{3b9}"),
            '\u{1fb2}' => result.push_str("\u{1f70}\u{3b9}"),
            '\u{1fb3}' => result.push_str("\u{3b1}\u{3b9}"),
            '\u{1fb4}' => result.push_str("\u{3ac}\u{3b9}"),
            '\u{1fb6}' => result.push_str("\u{3b1}\u{342}"),
            '\u{1fb7}' => result.push_str("\u{3b1}\u{342}\u{3b9}"),
            '\u{1fbc}' => result.push_str("\u{3b1}\u{3b9}"),
            '\u{1fbe}' => result.push('\u{3b9}'),
            '\u{1fc2}' => result.push_str("\u{1f74}\u{3b9}"),
            '\u{1fc3}' => result.push_str("\u{3b7}\u{3b9}"),
            '\u{1fc4}' => result.push_str("\u{3ae}\u{3b9}"),
            '\u{1fc6}' => result.push_str("\u{3b7}\u{342}"),
            '\u{1fc7}' => result.push_str("\u{3b7}\u{342}\u{3b9}"),
            '\u{1fcc}' => result.push_str("\u{3b7}\u{3b9}"),
            '\u{1fd2}' => result.push_str("\u{3b9}\u{308}\u{300}"),
            '\u{1fd3}' => result.push_str("\u{3b9}\u{308}\u{301}"),
            '\u{1fd6}' => result.push_str("\u{3b9}\u{342}"),
            '\u{1fd7}' => result.push_str("\u{3b9}\u{308}\u{342}"),
            '\u{1fe2}' => result.push_str("\u{3c5}\u{308}\u{300}"),
            '\u{1fe3}' => result.push_str("\u{3c5}\u{308}\u{301}"),
            '\u{1fe4}' => result.push_str("\u{3c1}\u{313}"),
            '\u{1fe6}' => result.push_str("\u{3c5}\u{342}"),
            '\u{1fe7}' => result.push_str("\u{3c5}\u{308}\u{342}"),
            '\u{1ff2}' => result.push_str("\u{1f7c}\u{3b9}"),
            '\u{1ff3}' => result.push_str("\u{3c9}\u{3b9}"),
            '\u{1ff4}' => result.push_str("\u{3ce}\u{3b9}"),
            '\u{1ff6}' => result.push_str("\u{3c9}\u{342}"),
            '\u{1ff7}' => result.push_str("\u{3c9}\u{342}\u{3b9}"),
            '\u{1ffc}' => result.push_str("\u{3c9}\u{3b9}"),
            '\u{ab70}' => result.push('\u{13a0}'),
            '\u{ab71}' => result.push('\u{13a1}'),
            '\u{ab72}' => result.push('\u{13a2}'),
            '\u{ab73}' => result.push('\u{13a3}'),
            '\u{ab74}' => result.push('\u{13a4}'),
            '\u{ab75}' => result.push('\u{13a5}'),
            '\u{ab76}' => result.push('\u{13a6}'),
            '\u{ab77}' => result.push('\u{13a7}'),
            '\u{ab78}' => result.push('\u{13a8}'),
            '\u{ab79}' => result.push('\u{13a9}'),
            '\u{ab7a}' => result.push('\u{13aa}'),
            '\u{ab7b}' => result.push('\u{13ab}'),
            '\u{ab7c}' => result.push('\u{13ac}'),
            '\u{ab7d}' => result.push('\u{13ad}'),
            '\u{ab7e}' => result.push('\u{13ae}'),
            '\u{ab7f}' => result.push('\u{13af}'),
            '\u{ab80}' => result.push('\u{13b0}'),
            '\u{ab81}' => result.push('\u{13b1}'),
            '\u{ab82}' => result.push('\u{13b2}'),
            '\u{ab83}' => result.push('\u{13b3}'),
            '\u{ab84}' => result.push('\u{13b4}'),
            '\u{ab85}' => result.push('\u{13b5}'),
            '\u{ab86}' => result.push('\u{13b6}'),
            '\u{ab87}' => result.push('\u{13b7}'),
            '\u{ab88}' => result.push('\u{13b8}'),
            '\u{ab89}' => result.push('\u{13b9}'),
            '\u{ab8a}' => result.push('\u{13ba}'),
            '\u{ab8b}' => result.push('\u{13bb}'),
            '\u{ab8c}' => result.push('\u{13bc}'),
            '\u{ab8d}' => result.push('\u{13bd}'),
            '\u{ab8e}' => result.push('\u{13be}'),
            '\u{ab8f}' => result.push('\u{13bf}'),
            '\u{ab90}' => result.push('\u{13c0}'),
            '\u{ab91}' => result.push('\u{13c1}'),
            '\u{ab92}' => result.push('\u{13c2}'),
            '\u{ab93}' => result.push('\u{13c3}'),
            '\u{ab94}' => result.push('\u{13c4}'),
            '\u{ab95}' => result.push('\u{13c5}'),
            '\u{ab96}' => result.push('\u{13c6}'),
            '\u{ab97}' => result.push('\u{13c7}'),
            '\u{ab98}' => result.push('\u{13c8}'),
            '\u{ab99}' => result.push('\u{13c9}'),
            '\u{ab9a}' => result.push('\u{13ca}'),
            '\u{ab9b}' => result.push('\u{13cb}'),
            '\u{ab9c}' => result.push('\u{13cc}'),
            '\u{ab9d}' => result.push('\u{13cd}'),
            '\u{ab9e}' => result.push('\u{13ce}'),
            '\u{ab9f}' => result.push('\u{13cf}'),
            '\u{aba0}' => result.push('\u{13d0}'),
            '\u{aba1}' => result.push('\u{13d1}'),
            '\u{aba2}' => result.push('\u{13d2}'),
            '\u{aba3}' => result.push('\u{13d3}'),
            '\u{aba4}' => result.push('\u{13d4}'),
            '\u{aba5}' => result.push('\u{13d5}'),
            '\u{aba6}' => result.push('\u{13d6}'),
            '\u{aba7}' => result.push('\u{13d7}'),
            '\u{aba8}' => result.push('\u{13d8}'),
            '\u{aba9}' => result.push('\u{13d9}'),
            '\u{abaa}' => result.push('\u{13da}'),
            '\u{abab}' => result.push('\u{13db}'),
            '\u{abac}' => result.push('\u{13dc}'),
            '\u{abad}' => result.push('\u{13dd}'),
            '\u{abae}' => result.push('\u{13de}'),
            '\u{abaf}' => result.push('\u{13df}'),
            '\u{abb0}' => result.push('\u{13e0}'),
            '\u{abb1}' => result.push('\u{13e1}'),
            '\u{abb2}' => result.push('\u{13e2}'),
            '\u{abb3}' => result.push('\u{13e3}'),
            '\u{abb4}' => result.push('\u{13e4}'),
            '\u{abb5}' => result.push('\u{13e5}'),
            '\u{abb6}' => result.push('\u{13e6}'),
            '\u{abb7}' => result.push('\u{13e7}'),
            '\u{abb8}' => result.push('\u{13e8}'),
            '\u{abb9}' => result.push('\u{13e9}'),
            '\u{abba}' => result.push('\u{13ea}'),
            '\u{abbb}' => result.push('\u{13eb}'),
            '\u{abbc}' => result.push('\u{13ec}'),
            '\u{abbd}' => result.push('\u{13ed}'),
            '\u{abbe}' => result.push('\u{13ee}'),
            '\u{abbf}' => result.push('\u{13ef}'),
            '\u{fb00}' => result.push_str("\u{66}\u{66}"),
            '\u{fb01}' => result.push_str("\u{66}\u{69}"),
            '\u{fb02}' => result.push_str("\u{66}\u{6c}"),
            '\u{fb03}' => result.push_str("\u{66}\u{66}\u{69}"),
            '\u{fb04}' => result.push_str("\u{66}\u{66}\u{6c}"),
            '\u{fb05}' => result.push_str("\u{73}\u{74}"),
            '\u{fb06}' => result.push_str("\u{73}\u{74}"),
            '\u{fb13}' => result.push_str("\u{574}\u{576}"),
            '\u{fb14}' => result.push_str("\u{574}\u{565}"),
            '\u{fb15}' => result.push_str("\u{574}\u{56b}"),
            '\u{fb16}' => result.push_str("\u{57e}\u{576}"),
            '\u{fb17}' => result.push_str("\u{574}\u{56d}"),
            _ => result.extend(c.to_lowercase()),
        }
    }
    result
}
