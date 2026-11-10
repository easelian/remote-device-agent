//! Explicit Linux browser process-ownership fixture, separate from Agent RPC.
#[cfg(target_os = "linux")]
mod bridge;
#[cfg(target_os = "linux")]
mod browser;
#[cfg(target_os = "linux")]
mod cdp;
#[cfg(target_os = "linux")]
mod fixture;
#[cfg(target_os = "linux")]
mod page;
#[cfg(target_os = "linux")]
mod scenarios;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<()> {
    scenarios::run().await
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<()> {
    Err("This P0 process-ownership harness currently supports Linux only".into())
}
