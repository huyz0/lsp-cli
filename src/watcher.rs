//! File-system watcher for daemon-managed LSP servers, mirroring
//! manager/watcher.ts (chokidar there, `notify` here).
//!
//! This keeps warm servers current with edits made *outside* the file a
//! command is querying. The file under the cursor is always re-read from
//! disk and pushed with `didOpen`/`didChange` by `ensure_daemon_session`,
//! so the watcher's job is everything else in the project.
//!
//! Watches a project root for create/change/delete events on files whose
//! extension matches the language's registered extensions, debounces them
//! (100ms, same as the TS original), and forwards batched
//! `workspace/didChangeWatchedFiles` change lists to whoever owns the
//! watcher (the daemon's `Manager`, via an mpsc channel) rather than
//! calling back into it directly — this keeps the watcher decoupled from
//! `Manager`'s internals instead of needing a circular `Arc<Manager>`.

use notify::event::ModifyKind;
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
                if let Some(v) = to_change(&first, &root, &extensions) {
                    pending.push(v);
                }
                while let Ok(Some(e)) =
                    tokio::time::timeout(std::time::Duration::from_millis(100), event_rx.recv())
                        .await
                {
                    if let Some(v) = to_change(&e, &root, &extensions) {
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
fn to_change(
    event: &notify::Event,
    project_root: &str,
    extensions: &HashSet<String>,
) -> Option<Value> {
    // A rename delivers `[from, to]`; the destination is the path that now
    // exists and is what a server needs told about. Taking only
    // `paths.first()` meant a file renamed *into* the project was reported
    // as a change to its old name and the new file was never announced.
    let path = match event.kind {
        EventKind::Modify(ModifyKind::Name(_)) if event.paths.len() > 1 => event.paths.last()?,
        _ => event.paths.first()?,
    };

    // Ignore checks run against the path *relative to the project root*.
    // Testing the absolute path meant a project that merely lives under a
    // dot-directory — `~/.config/nvim`, `~/.dotfiles` — matched on its own
    // parent and had every single event discarded, so warm servers there
    // never learned about external edits at all.
    let relative = path.strip_prefix(project_root).unwrap_or(path);
    let rel_str = relative.to_string_lossy();
    if rel_str
        .split('/')
        .any(|c| c.starts_with('.') || matches!(c, "node_modules" | "dist" | "build" | "target"))
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

    Some(json!({ "uri": lsp::uri::from_path(path), "type": ty }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind, RenameMode};

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
            to_change(
                &evt(EventKind::Create(CreateKind::File), "/p/a.ts"),
                "/p",
                &exts
            )
            .unwrap()["type"],
            1
        );
        assert_eq!(
            to_change(
                &evt(EventKind::Modify(ModifyKind::Any), "/p/a.ts"),
                "/p",
                &exts
            )
            .unwrap()["type"],
            2
        );
        assert_eq!(
            to_change(
                &evt(EventKind::Remove(RemoveKind::File), "/p/a.ts"),
                "/p",
                &exts
            )
            .unwrap()["type"],
            3
        );
    }

    #[test]
    fn filters_out_extensions_not_being_watched() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/a.py"),
            "/p",
            &exts
        )
        .is_none());
    }

    #[test]
    fn filters_out_dotfiles_and_ignored_directories() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/.git/a.ts"),
            "/p",
            &exts
        )
        .is_none());
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/node_modules/a.ts"),
            "/p",
            &exts
        )
        .is_none());
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/dist/a.ts"),
            "/p",
            &exts
        )
        .is_none());
    }

    #[test]
    fn builds_a_file_uri_from_the_absolute_path() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let v = to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/a.ts"),
            "/p",
            &exts,
        )
        .unwrap();
        assert_eq!(v["uri"], "file:///p/a.ts");
    }

    #[test]
    fn a_project_inside_a_dot_directory_still_reports_changes() {
        // The ignore check runs on the path relative to the root. Applied
        // to the absolute path, `.config` matched and every event for a
        // project at ~/.config/nvim was silently dropped — so warm servers
        // there never saw an external edit.
        let exts: HashSet<String> = [".lua"].iter().map(|s| s.to_string()).collect();
        let v = to_change(
            &evt(
                EventKind::Modify(ModifyKind::Any),
                "/home/u/.config/nvim/init.lua",
            ),
            "/home/u/.config/nvim",
            &exts,
        );
        assert!(v.is_some(), "event under a dot-directory root was dropped");
    }

    #[test]
    fn dot_directories_below_the_root_are_still_ignored() {
        let exts: HashSet<String> = [".lua"].iter().map(|s| s.to_string()).collect();
        assert!(to_change(
            &evt(
                EventKind::Modify(ModifyKind::Any),
                "/home/u/.config/nvim/.git/x.lua"
            ),
            "/home/u/.config/nvim",
            &exts,
        )
        .is_none());
    }

    #[test]
    fn a_rename_reports_the_destination_not_the_source() {
        // notify delivers [from, to]. Reporting `from` told the server
        // about a path that no longer exists and never mentioned the file
        // that now does.
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let event = notify::Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![
                std::path::PathBuf::from("/p/old.ts"),
                std::path::PathBuf::from("/p/new.ts"),
            ],
            attrs: Default::default(),
        };
        let v = to_change(&event, "/p", &exts).unwrap();
        assert_eq!(v["uri"], "file:///p/new.ts");
    }

    #[test]
    fn paths_with_spaces_are_percent_encoded_in_the_uri() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let v = to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/my proj/a.ts"),
            "/my proj",
            &exts,
        )
        .unwrap();
        assert_eq!(v["uri"], "file:///my%20proj/a.ts");
    }
}
