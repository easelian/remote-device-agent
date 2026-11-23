//! Which addresses a browser session may be given, and the origin of one.
use super::{Failure, Result};

/// An ordinary web address: `http` or `https`, with no credentials in it.
pub fn allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.username().is_empty() && u.password().is_none()
    })
}

pub fn origin(value: &str) -> Result<String> {
    if !allowed(value) {
        return Err(Failure::new("browser_url_denied"));
    }
    Ok(url::Url::parse(value)
        .map_err(super::io_error)?
        .origin()
        .ascii_serialization())
}
