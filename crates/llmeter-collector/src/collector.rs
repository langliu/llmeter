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
use llmeter_core::{Provider, ProviderDetection, SyncResult};
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
    FxUpdated(crate::fx::ExchangeRates),
}

/// Holds the sync locks for the duration of one sync run.
struct SyncLockGuards<'a> {
    _remote: Option<std::sync::MutexGuard<'a, ()>>,
    _local: Option<std::sync::MutexGuard<'a, ()>>,
}

#[derive(Clone)]
pub struct Collector {
    engine: SyncEngine,
    event_sender: Sender<CollectorEvent>,
    event_receiver: Arc<Mutex<Receiver<CollectorEvent>>>,
    local_sync_lock: Arc<Mutex<()>>,
    remote_sync_lock: Arc<Mutex<()>>,
    detections: Arc<Mutex<Vec<ProviderDetection>>>,
}

impl Collector {
    pub fn new(database: Database) -> Self {
        let _ = crate::pricing::load_cached_pricing(hooks::data_dir().join("cache"));
        let (event_sender, event_receiver) = mpsc::channel();
        Self {
            engine: SyncEngine::new(database),
            event_sender,
            event_receiver: Arc::new(Mutex::new(event_receiver)),
            local_sync_lock: Arc::new(Mutex::new(())),
            remote_sync_lock: Arc::new(Mutex::new(())),
            detections: Arc::new(Mutex::new(Vec::new())),
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
        let _guards = self.acquire_sync_locks(true, true)?;
        self.engine.clear_rebuildable_usage()?;
        let result = self.engine.sync(SyncOptions::default())?;
        let _ = self
            .event_sender
            .send(CollectorEvent::UsageChanged(result.clone()));
        Ok(result)
    }

    pub fn detect_all(&self) -> Vec<ProviderDetection> {
        self.refresh_detections()
    }

    pub fn cached_detections(&self) -> Vec<ProviderDetection> {
        if let Ok(cache) = self.detections.lock()
            && !cache.is_empty()
        {
            return cache.clone();
        }
        self.refresh_detections()
    }

    pub fn refresh_detections(&self) -> Vec<ProviderDetection> {
        let detected = self.engine.detect_all();
        if let Ok(mut cache) = self.detections.lock() {
            *cache = detected.clone();
        }
        detected
    }

    fn acquire_sync_locks(&self, local: bool, remote: bool) -> Result<SyncLockGuards<'_>> {
        // Fixed order (remote, then local) so combined runs cannot deadlock
        // against runs that hold only one of the two locks.
        let remote_guard = remote
            .then(|| {
                self.remote_sync_lock
                    .lock()
                    .map_err(|_| anyhow::anyhow!("remote sync lock poisoned"))
            })
            .transpose()?;
        let local_guard = local
            .then(|| {
                self.local_sync_lock
                    .lock()
                    .map_err(|_| anyhow::anyhow!("local sync lock poisoned"))
            })
            .transpose()?;
        Ok(SyncLockGuards {
            _remote: remote_guard,
            _local: local_guard,
        })
    }

    fn sync_with_options(&self, options: SyncOptions) -> Result<SyncResult> {
        let (local, remote) = self.engine.sync_lock_scope(&options);
        let _guards = self.acquire_sync_locks(local, remote)?;
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
                refresh_exchange_rates(&collector);

                let Ok((mut watcher, receiver)) = watcher::start(&[]) else {
                    warn!(
                        "filesystem watcher could not be started; periodic rescan remains active"
                    );
                    let _ = collector.sync_now();
                    loop {
                        thread::sleep(Duration::from_secs(300));
                        refresh_exchange_rates(&collector);
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
                            refresh_exchange_rates(&collector);
                            let _ = collector.sync_now();
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("failed to start collector thread");
    }
}

fn refresh_exchange_rates(collector: &Collector) {
    match crate::fx::refresh_exchange_rates(hooks::data_dir().join("cache")) {
        Ok(rates) if rates.source == crate::fx::FxSource::Upstream => {
            let _ = collector
                .event_sender
                .send(CollectorEvent::FxUpdated(rates));
        }
        Ok(_) => {}
        Err(error) => warn!(error = %error, "exchange-rate refresh failed"),
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
    let watch_roots = engine.watch_roots();
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
            for (provider, root) in &watch_roots {
                if path_is_under(path, root) || path_is_under(root, path) {
                    providers.insert(*provider);
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
