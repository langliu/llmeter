use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver},
};

use anyhow::Result;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

pub fn start(paths: &[PathBuf]) -> Result<(RecommendedWatcher, Receiver<notify::Result<Event>>)> {
    let (sender, receiver) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            let _ = sender.send(event);
        },
        Config::default(),
    )?;
    for path in paths {
        if path.exists() {
            watcher.watch(path, RecursiveMode::Recursive)?;
        }
    }
    Ok((watcher, receiver))
}
