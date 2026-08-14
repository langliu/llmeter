use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use llmeter_core::{Provider, SyncResult};
use llmeter_storage::Database;
use tracing::warn;

use crate::{
    hooks,
    providers::home_dir,
    sync::{SyncEngine, SyncOptions},
    watcher,
};

#[derive(Clone, Debug)]
pub enum CollectorEvent {
    UsageChanged(SyncResult),
}

#[derive(Clone)]
pub struct Collector {
    engine: SyncEngine,
    event_sender: Sender<CollectorEvent>,
    event_receiver: Arc<Mutex<Receiver<CollectorEvent>>>,
    sync_lock: Arc<Mutex<()>>,
}

impl Collector {
    pub fn new(database: Database) -> Self {
        let (event_sender, event_receiver) = mpsc::channel();
        Self {
            engine: SyncEngine::new(database),
            event_sender,
            event_receiver: Arc::new(Mutex::new(event_receiver)),
            sync_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn engine(&self) -> &SyncEngine {
        &self.engine
    }

    pub fn sync_now(&self) -> Result<SyncResult> {
        self.sync_with_options(SyncOptions::default())
    }

    pub fn sync_provider(&self, provider: Provider) -> Result<SyncResult> {
        self.sync_with_options(SyncOptions::only(provider))
    }

    pub fn full_rescan(&self) -> Result<SyncResult> {
        let _guard = self
            .sync_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("sync lock poisoned"))?;
        self.engine.database().clear_usage_and_cursors()?;
        let result = self.engine.sync(SyncOptions::default())?;
        let _ = self
            .event_sender
            .send(CollectorEvent::UsageChanged(result.clone()));
        Ok(result)
    }

    pub fn detect_all(&self) -> Vec<llmeter_core::ProviderDetection> {
        self.engine.detect_all()
    }

    fn sync_with_options(&self, options: SyncOptions) -> Result<SyncResult> {
        let _guard = self
            .sync_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("sync lock poisoned"))?;
        let result = self.engine.sync(options)?;
        let _ = self
            .event_sender
            .send(CollectorEvent::UsageChanged(result.clone()));
        Ok(result)
    }

    pub fn try_recv(&self) -> Option<CollectorEvent> {
        let receiver = self.event_receiver.lock().ok()?;
        receiver.try_recv().ok()
    }

    pub fn start_background(&self) {
        let collector = self.clone();
        thread::Builder::new()
            .name("llmeter-collector".into())
            .spawn(move || {
                let roots = watch_roots();
                let Ok((_watcher, receiver)) = watcher::start(&roots) else {
                    warn!(
                        "filesystem watcher could not be started; periodic rescan remains active"
                    );
                    let _ = collector.sync_now();
                    loop {
                        thread::sleep(Duration::from_secs(300));
                        let _ = collector.sync_now();
                    }
                };

                let _ = collector.sync_now();
                loop {
                    match receiver.recv_timeout(Duration::from_secs(300)) {
                        Ok(Ok(_event)) => {
                            // Drain bursts and debounce before a single sync.
                            thread::sleep(Duration::from_millis(500));
                            while receiver.try_recv().is_ok() {}
                            let _ = collector.sync_now();
                        }
                        Ok(Err(error)) => warn!(error = %error, "filesystem watcher event failed"),
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let _ = collector.sync_now();
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("failed to start collector thread");
    }
}

fn watch_roots() -> Vec<PathBuf> {
    let home = home_dir();
    // Watch the signal directory instead of the signal file itself. Hooks may
    // create the signal file after the app has already started.
    let signal_directory = hooks::signal_path()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(hooks::data_dir);
    vec![
        home.join(".codex").join("sessions"),
        home.join(".claude").join("projects"),
        home.join(".pi").join("agent").join("sessions"),
        home.join(".omp").join("agent").join("sessions"),
        home.join(".local").join("share").join("opencode"),
        home.join(".config").join("opencode"),
        home.join("Library")
            .join("Application Support")
            .join("opencode"),
        home.join(".opencode"),
        signal_directory,
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}
