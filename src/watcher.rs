//! File-system watcher for daemon-managed LSP servers, mirroring
//! manager/watcher.ts (chokidar there, `notify` here). Only relevant to
//! the daemon's `lsp server start`-warmed servers (see the "Navigation
//! commands don't proxy through the daemon" deviation in the README) —
//! per-invocation navigation commands always read the current file off
//! disk directly, so they never need this.
//!
//! Watches a project root for create/change/delete events on files whose
//! extension matches the language's registered extensions, debounces them
//! (100ms, same as the TS original), and forwards batched
//! `workspace/didChangeWatchedFiles` change lists to whoever owns the
//! watcher (the daemon's `Manager`, via an mpsc channel) rather than
//! calling back into it directly — this keeps the watcher decoupled from
//! `Manager`'s internals instead of needing a circular `Arc<Manager>`.

use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tokio::sync::{mpsc, Mutex};

/// One batch of watched-file changes for a project root, ready to forward
/// as `workspace/didChangeWatchedFiles` params.
pub type WatchBatch = (String, Vec<Value>);

struct WatcherHandle {
    // Kept alive only so the OS watch stays registered — dropping this
    // (via `stop`) unregisters it and closes the event channel, which ends
    // the debounce task's `rx.recv()` loop naturally.
    _watcher: notify::RecommendedWatcher,
}

pub struct WatcherManager {
    watchers: Mutex<HashMap<String, WatcherHandle>>,
    tx: mpsc::UnboundedSender<WatchBatch>,
}

impl WatcherManager {
    pub fn new(tx: mpsc::UnboundedSender<WatchBatch>) -> Self {
        Self {
            watchers: Mutex::new(HashMap::new()),
            tx,
        }
    }

    /// Starts watching `project_root` for changes to files with the given
    /// extensions, if not already watching it. Extensions are fixed at
    /// first watch (matching the TS original's practical behavior — the
    /// common case is one language server per project root).
    pub async fn ensure_watching(&self, project_root: &str, extensions: &[&str]) {
        let mut watchers = self.watchers.lock().await;
        if watchers.contains_key(project_root) {
            return;
        }

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<notify::Event>();
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    let _ = event_tx.send(event);
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("[watcher] failed to create watcher for {project_root}: {e}");
                    return;
                }
            };

        if let Err(e) = watcher.watch(Path::new(project_root), RecursiveMode::Recursive) {
            eprintln!("[watcher] failed to watch {project_root}: {e}");
            return;
        }

        let extensions: HashSet<String> = extensions.iter().map(|s| s.to_lowercase()).collect();
        let root = project_root.to_string();
        let batch_tx = self.tx.clone();

        tokio::spawn(async move {
            let mut pending: Vec<Value> = Vec::new();
            // Block for the first event in a batch, then drain anything
            // else that arrives within the debounce window before flushing
            // — same shape as the TS watcher's setTimeout-based debounce.
            while let Some(first) = event_rx.recv().await {
                if let Some(v) = to_change(&first, &extensions) {
                    pending.push(v);
                }
                while let Ok(Some(e)) =
                    tokio::time::timeout(std::time::Duration::from_millis(100), event_rx.recv())
                        .await
                {
                    if let Some(v) = to_change(&e, &extensions) {
                        pending.push(v);
                    }
                }
                if !pending.is_empty() {
                    let changes = std::mem::take(&mut pending);
                    eprintln!(
                        "[watcher] {} change(s) detected in {root}, notifying live servers",
                        changes.len()
                    );
                    if batch_tx.send((root.clone(), changes)).is_err() {
                        // Receiver gone (daemon shutting down) — stop watching.
                        return;
                    }
                }
            }
        });

        watchers.insert(
            project_root.to_string(),
            WatcherHandle { _watcher: watcher },
        );
    }

    pub async fn stop(&self, project_root: &str) {
        self.watchers.lock().await.remove(project_root);
    }

    pub async fn dispose(&self) {
        self.watchers.lock().await.clear();
    }
}

/// LSP `FileChangeType`: 1 = Created, 2 = Changed, 3 = Deleted.
fn to_change(event: &notify::Event, extensions: &HashSet<String>) -> Option<Value> {
    let path = event.paths.first()?;

    let path_str = path.to_string_lossy();
    if path_str.split('/').any(|c| c.starts_with('.'))
        || ["node_modules", "dist", "build", "target"]
            .iter()
            .any(|d| path_str.contains(&format!("/{d}/")))
    {
        return None;
    }

    let ext = format!(".{}", path.extension()?.to_str()?.to_lowercase());
    if !extensions.contains(&ext) {
        return None;
    }

    let ty = match event.kind {
        EventKind::Create(_) => 1,
        EventKind::Modify(_) => 2,
        EventKind::Remove(_) => 3,
        _ => return None,
    };

    Some(json!({ "uri": format!("file://{}", path.display()), "type": ty }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RemoveKind};

    fn evt(kind: EventKind, path: &str) -> notify::Event {
        notify::Event {
            kind,
            paths: vec![std::path::PathBuf::from(path)],
            attrs: Default::default(),
        }
    }

    #[test]
    fn maps_create_modify_remove_to_lsp_file_change_types() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            to_change(&evt(EventKind::Create(CreateKind::File), "/p/a.ts"), &exts).unwrap()["type"],
            1
        );
        assert_eq!(
            to_change(&evt(EventKind::Modify(ModifyKind::Any), "/p/a.ts"), &exts).unwrap()["type"],
            2
        );
        assert_eq!(
            to_change(&evt(EventKind::Remove(RemoveKind::File), "/p/a.ts"), &exts).unwrap()["type"],
            3
        );
    }

    #[test]
    fn filters_out_extensions_not_being_watched() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        assert!(to_change(&evt(EventKind::Modify(ModifyKind::Any), "/p/a.py"), &exts).is_none());
    }

    #[test]
    fn filters_out_dotfiles_and_ignored_directories() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/.git/a.ts"),
            &exts
        )
        .is_none());
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/node_modules/a.ts"),
            &exts
        )
        .is_none());
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/dist/a.ts"),
            &exts
        )
        .is_none());
    }

    #[test]
    fn builds_a_file_uri_from_the_absolute_path() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let v = to_change(&evt(EventKind::Modify(ModifyKind::Any), "/p/a.ts"), &exts).unwrap();
        assert_eq!(v["uri"], "file:///p/a.ts");
    }
}
