//! Bridges GPUI's executor and tokio. Database drivers need a tokio reactor, while views run
//! on GPUI's own executor, so every driver call is spawned onto a shared tokio runtime and its
//! `JoinHandle` (an ordinary future) is awaited from GPUI tasks.

use std::future::Future;
use std::sync::{Arc, OnceLock};

use ovs_core::{ConnectionConfig, Driver, Session};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

/// A session shared between the view that owns it and the tokio tasks that use it.
pub type SharedSession = Arc<Mutex<Box<dyn Session>>>;

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ovs-db")
            .enable_all()
            .build()
            .expect("failed to start the database runtime")
    })
}

/// Runs `fut` on the database runtime and resolves with its output.
pub async fn run<F>(fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match runtime().spawn(fut).await {
        Ok(output) => output,
        Err(err) => std::panic::resume_unwind(err.into_panic()),
    }
}

pub fn driver_for(config: &ConnectionConfig) -> Arc<dyn Driver> {
    match config.engine {
        ovs_core::Engine::MySql => Arc::new(ovs_mysql::MySqlDriver),
    }
}

/// Opens a new session for `config`.
pub async fn connect(
    config: ConnectionConfig,
    password: Option<String>,
) -> ovs_core::Result<SharedSession> {
    run(async move {
        let driver = driver_for(&config);
        let session = driver.connect(&config, password.as_deref()).await?;
        Ok(Arc::new(Mutex::new(session)))
    })
    .await
}
