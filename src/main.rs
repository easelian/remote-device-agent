use rdh_agent::cli::{Action, parse};
use rdh_agent::error::Result;
use std::future::Future;

#[cfg(unix)]
async fn wait_for_shutdown() {
    if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
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
fn run_async(task: impl Future<Output = Result<()>>) {
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap_or_else(|error| fail(error));
    if let Err(error) = runtime.block_on(task) {
        fail(error);
    }
}
async fn launch_agent(options: rdh_agent::binding::Options) -> Result<()> {
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let notify = stop.clone();
    let signal = tokio::spawn(async move {
        wait_for_shutdown().await;
        let _ = notify.send(true);
    });
    let result = rdh_agent::app::launch(options, shutdown, stop).await;
    signal.abort();
    result
}
fn main() {
    if let Some(result) = rdh_agent::browser::run_internal_supervisor() {
        if let Err(error) = result {
            fail(error);
        }
        return;
    }
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match parse(&args) {
        Ok(Action::Help) => print!("{}", rdh_agent::cli::help()),
        Ok(Action::Version) => println!("{}", env!("CARGO_PKG_VERSION")),
        Ok(Action::Launch) => {
            let options =
                rdh_agent::binding::prepare_options(true).unwrap_or_else(|error| fail(error));
            run_async(launch_agent(options));
        }
        Ok(Action::Unbind) => run_async(rdh_agent::binding::unbind()),
        Ok(Action::Update) => run_async(rdh_agent::binding::update()),
        Err(error) => fail(error),
    }
}
