//! Darwin uses libproc; there is no /proc tree or Linux subreaper.
use std::io;
pub fn prepare() -> io::Result<()> {
    Ok(())
}
pub fn reap(_: u32) {}
pub fn stopped(group: u32) -> io::Result<bool> {
    crate::processes::macos::group_has_live_members(group).map(|live| !live)
}
pub fn retired(group: u32) -> io::Result<bool> {
    crate::processes::macos::group_is_empty(group)
}
