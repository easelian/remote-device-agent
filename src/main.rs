use std::future::Future;

use rdh_agent::cli::{Action, LaunchArgs, parse};
use rdh_agent::error::Result;

#[cfg(unix)]
async fn wait_for_shutdown() {
    if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    } else {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

fn fail(error: impl std::fmt::Display) -> ! {
    eprintln!("Remote Device Hub: {error}");
    std::process::exit(1);
}

fn check(result: Result<()>) {
    if let Err(error) = result {
        fail(error);
    }
}

/// I/O and timers need a runtime, but synchronous CLI commands do not. Device
/// operations use their own admission limits for blocking work, so scheduler
/// threads do not need to scale with the number of visible CPUs.
fn run_async(task: impl Future<Output = Result<()>>) {
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap_or_else(|error| fail(error));
    if let Err(error) = runtime.block_on(task) {
        // As before, an error exits without waiting indefinitely for a
        // blocking operation whose outcome could not be confirmed.
        fail(error);
    }
}

async fn launch_agent(args: LaunchArgs) -> Result<()> {
    let (signal, shutdown) = tokio::sync::watch::channel(false);
    let notify = signal.clone();
    let signal_task = tokio::spawn(async move {
        wait_for_shutdown().await;
        let _ = notify.send(true);
    });
    let result = rdh_agent::app::launch(args, shutdown, signal.clone()).await;
    signal_task.abort();
    drop(signal);
    result
}

fn main() {
    #[cfg(all(windows, feature = "experimental-windows-install"))]
    match rdh_agent::native_launcher::forward() {
        Ok(Some(code)) => std::process::exit(code),
        Ok(None) => {}
        Err(error) => fail(error),
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let server = std::env::var("RDH_SERVER").ok();
    match parse(&args, server.as_deref()) {
        Ok(Action::Help) => print!("{}", rdh_agent::cli::help()),
        Ok(Action::Version) => println!("{}", env!("CARGO_PKG_VERSION")),
        Ok(Action::ServiceUpdateProtocol) => println!("rdh-service-update-v1"),
        Ok(Action::ServiceRun(args)) => {
            check(rdh_agent::service_host::run(&args, server.as_deref()))
        }
        Ok(Action::Startup(args)) => check(rdh_agent::startup::cli::run(&args, server.as_deref())),
        Ok(Action::InstallNative(args)) => check(rdh_agent::upgrade_cli::install(&args)),
        Ok(Action::InstallService(args)) => check(rdh_agent::service_install::run(&args)),
        Ok(Action::Upgrade(args)) => run_async(rdh_agent::upgrade_cli::upgrade(&args)),
        Ok(Action::Rollback(args)) => check(rdh_agent::upgrade_cli::rollback(&args)),
        Ok(Action::ValidateConfig(args)) => {
            check(rdh_agent::app::validate_config(&args));
            println!("configuration valid");
        }
        Ok(Action::Launch(args)) => run_async(launch_agent(args)),
        Err(error) => fail(error),
    }
}
