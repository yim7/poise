use std::{future::Future, thread};

pub(crate) fn spawn_task<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(future);
        return;
    }

    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build background tokio runtime");
        runtime.block_on(future);
    });
}
