use crate::error::Result;

pub(super) fn run(args: &[String], server: Option<&str>) -> Result<()> {
    let (name, launch, state) = super::arguments(args, server)?;
    crate::service_install::check_service_entry(&name, &launch, state.as_deref())?;
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?
        .block_on(async {
            // Install the signal stream before any configuration or network work.
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let (stop, shutdown) = tokio::sync::watch::channel(false);
            let notify = stop.clone();
            let signal = tokio::spawn(async move {
                tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
                let _ = notify.send(true);
            });
            let result = crate::app::launch_existing(launch, shutdown, stop, state).await;
            signal.abort();
            result
        })
}
