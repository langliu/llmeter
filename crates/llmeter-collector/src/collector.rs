use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use llmeter_core::{Provider, SyncResult};
use llmeter_storage::Database;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::warn;

use crate::{
    hooks,

    sync::{SyncEngine, SyncOptions},
    watcher,
};

#[derive(Clone, Debug)]
pub enum CollectorEvent {
    UsageChanged(SyncResult),
    PricingUpdated,
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
        let _ = crate::pricing::load_cached_pricing(hooks::data_dir().join("cache"));
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
        let mut result = self.sync_with_options(SyncOptions::local_changes())?;
        result.merge(self.sync_with_options(SyncOptions::remote_snapshots())?);
        Ok(result)
    }

    pub fn sync_provider(&self, provider: Provider) -> Result<SyncResult> {
        self.sync_with_options(SyncOptions::only(provider))
    }

    pub fn full_rescan(&self) -> Result<SyncResult> {
        let _guard = self
            .sync_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("sync lock poisoned"))?;
        self.engine.clear_rebuildable_usage()?;
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
                match crate::pricing::refresh_pricing(
                    hooks::data_dir().join("cache"),
                    Some(collector.engine.database()),
                ) {
                    Ok(result) if result.repriced > 0 => {
                        let _ = collector.event_sender.send(CollectorEvent::PricingUpdated);
                    }
                    Ok(_) => {}
                    Err(error) => warn!(error = %error, "pricing refresh failed"),
                }

                let Ok((mut watcher, receiver)) = watcher::start(&[]) else {
                    warn!(
                        "filesystem watcher could not be started; periodic rescan remains active"
                    );
                    let _ = collector.sync_now();
                    loop {
                        thread::sleep(Duration::from_secs(300));
                        let _ = collector.sync_now();
                    }
                };

                let mut watched = HashSet::new();
                refresh_watches(&mut watcher, &mut watched, &collector.engine);
                let _ = collector.sync_now();
                loop {
                    match receiver.recv_timeout(Duration::from_secs(300)) {
                        Ok(Ok(event)) => {
                            thread::sleep(Duration::from_millis(500));
                            let mut events = vec![event];
                            while let Ok(Ok(extra)) = receiver.try_recv() {
                                events.push(extra);
                            }
                            refresh_watches(&mut watcher, &mut watched, &collector.engine);
                            let options = options_for_events(&collector.engine, &events);
                            if options.providers.as_ref().is_some_and(HashSet::is_empty) {
                                continue;
                            }
                            let _ = collector.sync_with_options(options);
                        }
                        Ok(Err(error)) => warn!(error = %error, "filesystem watcher event failed"),
                        Err(RecvTimeoutError::Timeout) => {
                            refresh_watches(&mut watcher, &mut watched, &collector.engine);
                            let _ = collector.sync_now();
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("failed to start collector thread");
    }
}

fn refresh_watches(
    watcher: &mut RecommendedWatcher,
    watched: &mut HashSet<PathBuf>,
    engine: &SyncEngine,
) {
    for path in watch_candidates(engine) {
        if !path.exists() || !watched.insert(path.clone()) {
            continue;
        }
        if let Err(error) = watcher.watch(&path, RecursiveMode::Recursive) {
            warn!(path = %path.display(), error = %error, "failed to watch provider root");
            watched.remove(&path);
        }
    }
}

fn watch_candidates(engine: &SyncEngine) -> Vec<PathBuf> {
    let mut roots = engine
        .watch_roots()
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    if let Some(parent) = hooks::signal_path().parent() {
        roots.push(parent.to_path_buf());
    } else {
        roots.push(hooks::data_dir());
    }
    roots
}

fn options_for_events(engine: &SyncEngine, events: &[Event]) -> SyncOptions {
    let signal_directory = hooks::signal_path()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(hooks::data_dir);
    let mut providers = HashSet::new();
    let mut saw_signal = false;
    for event in events {
        for path in &event.paths {
            if path_is_under(path, &signal_directory) {
                saw_signal = true;
                if let Some(provider) = provider_from_signal(path) {
                    providers.insert(provider);
                }
                continue;
            }
            for (provider, root) in engine.watch_roots() {
                if path_is_under(path, &root) || path_is_under(&root, path) {
                    providers.insert(provider);
                }
            }
        }
    }
    if providers.is_empty() && saw_signal {
        return SyncOptions::default();
    }
    SyncOptions::providers(providers)
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn provider_from_signal(path: &Path) -> Option<Provider> {
    let contents = fs::read_to_string(path).ok()?;
    contents
        .lines()
        .rev()
        .find_map(|line| line.split_whitespace().nth(1)?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_path_maps_to_named_provider() {
        let directory =
            std::env::temp_dir().join(format!("llmeter-signal-{}-{}", std::process::id(), "map"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("sync.signal");
        fs::write(&path, "1 grok\n2 claude\n").unwrap();
        assert_eq!(provider_from_signal(&path), Some(Provider::Claude));
        let _ = fs::remove_dir_all(directory);
    }
}
