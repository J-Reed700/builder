use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;

/// Owns the operating-system watcher for as long as background maintenance is
/// alive. The channel has capacity one on purpose: a burst means "refresh the
/// current tree", so retaining every intermediate event would only queue stale
/// work. A periodic full scan remains the recovery path for missed events.
pub struct CodeIndexWatch {
    _watcher: RecommendedWatcher,
    changed: tokio::sync::mpsc::Receiver<()>,
}

impl CodeIndexWatch {
    pub fn new(root: &Path) -> Result<Self> {
        let (sender, changed) = tokio::sync::mpsc::channel(1);
        let root = root.to_path_buf();
        let callback_root = root.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                if event
                    .paths
                    .iter()
                    .any(|path| should_refresh(&callback_root, path))
                {
                    let _ = sender.try_send(());
                }
            })?;
        watcher.watch(root.as_path(), RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            changed,
        })
    }

    /// Wait for a source-tree event or the bounded fallback interval. Event
    /// bursts are debounced and drained so one save/build cycle causes one scan.
    pub async fn wait(&mut self, refresh_secs: u64, debounce_ms: u64) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(refresh_secs)) => false,
            event = self.changed.recv() => {
                if event.is_some() {
                    self.debounce(debounce_ms).await;
                    true
                } else {
                    false
                }
            }
        }
    }

    pub(super) async fn take_pending(&mut self, debounce_ms: u64) -> bool {
        if self.changed.try_recv().is_err() {
            return false;
        }
        self.debounce(debounce_ms).await;
        true
    }

    async fn debounce(&mut self, debounce_ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
        while self.changed.try_recv().is_ok() {}
    }
}

pub(super) fn should_refresh(root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    !relative.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(
                ".git"
                    | ".builder"
                    | "target"
                    | "node_modules"
                    | "vendor"
                    | "dist"
                    | "build"
                    | "__pycache__"
            )
        )
    })
}
