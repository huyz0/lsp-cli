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
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// One batch of watched-file changes for a project root, ready to forward
/// as `workspace/didChangeWatchedFiles` params.
pub type WatchBatch = (String, Vec<Value>);

struct WatcherHandle {
    // The only strong reference to the watcher. Dropping this (via `stop`
    // or `dispose`) unregisters every OS watch and closes the event
    // channel, which ends the debounce task's `recv()` loop naturally. The
    // task itself holds a `Weak`, so it cannot keep the watcher alive.
    _watcher: Arc<Mutex<notify::RecommendedWatcher>>,
}

/// How long to wait for the tree to go quiet before flushing a batch.
const DEBOUNCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);

/// Longest a batch may be held open, however busy the tree is. Without a
/// cap, a continuous event stream re-arms the debounce forever.
const MAX_BATCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// Registers a non-recursive watch on `dir` and every directory beneath it
/// that survives the shared ignore list. Returns how many were registered.
fn watch_tree(watcher: &mut notify::RecommendedWatcher, dir: &Path) -> usize {
    let mut count = 0;
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        // `depth() == 0` keeps the root itself even when the project lives
        // in a dot-directory (`~/.config/nvim`), the same trap the BM25
        // walk hits.
        .filter_entry(|e| {
            e.depth() == 0
                || (e.file_type().is_dir()
                    && !crate::bm25::is_ignored_dir_name(&e.file_name().to_string_lossy()))
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
    {
        if watcher
            .watch(entry.path(), RecursiveMode::NonRecursive)
            .is_ok()
        {
            count += 1;
        }
    }
    count
}

/// Keeps the watch set in step with the tree.
///
/// Non-recursive watches do not cover directories that appear later, so a
/// newly created directory has to be picked up explicitly — and it may
/// arrive already populated (a `git checkout`, an extracted archive), so
/// its whole subtree is registered, not just the directory itself.
async fn maintain_watches(
    watcher: &std::sync::Weak<Mutex<notify::RecommendedWatcher>>,
    event: &notify::Event,
) {
    let relevant = matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_));
    if !relevant {
        return;
    }
    let Some(watcher) = watcher.upgrade() else {
        return; // watcher already dropped; the task is about to end
    };
    for path in &event.paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if crate::bm25::is_ignored_dir_name(&name) {
            continue;
        }
        let mut guard = watcher.lock().await;
        if path.is_dir() {
            watch_tree(&mut guard, path);
        } else {
            // A removed directory: `is_dir()` is false now, so this is the
            // same call either way. `notify` errors when the path was
            // never watched, which is expected for plain files.
            let _ = guard.unwatch(path);
        }
    }
}

pub struct WatcherManager {
    watchers: Mutex<HashMap<String, WatcherHandle>>,
    /// Each watched root's pending-changes buffer, kept apart from
    /// `watchers`: `take_pending` runs before every request, and
    /// `ensure_watching` holds `watchers` across a whole directory walk,
    /// which would stall requests for every other project meanwhile.
    pending: std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<Vec<Value>>>>>,
    tx: mpsc::UnboundedSender<WatchBatch>,
}

impl WatcherManager {
    pub fn new(tx: mpsc::UnboundedSender<WatchBatch>) -> Self {
        Self {
            watchers: Mutex::new(HashMap::new()),
            pending: std::sync::Mutex::new(HashMap::new()),
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
        let watcher =
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
        let watcher = Arc::new(Mutex::new(watcher));

        // Register one non-recursive watch per directory that survives the
        // ignore list, rather than one recursive watch on the root.
        //
        // A recursive watch is one inotify watch per directory underneath,
        // including every directory in `target/`, `node_modules/` and
        // `.git/` — commonly tens of thousands for a Rust project, against
        // a default `max_user_watches` of 8192 on many systems. Filtering
        // in `to_change` (as this used to) happens only after the kernel
        // has already registered the watch and delivered the event, so it
        // saved neither the watch descriptors nor the wakeups: a `cargo
        // build` would flood this loop with events that all get discarded.
        let watched = {
            let mut guard = watcher.lock().await;
            watch_tree(&mut guard, Path::new(project_root))
        };
        if watched == 0 {
            eprintln!("[watcher] failed to watch {project_root}");
            return;
        }

        let extensions: HashSet<String> = extensions.iter().map(|s| s.to_lowercase()).collect();
        let root = project_root.to_string();
        let batch_tx = self.tx.clone();
        // Weak, deliberately: the strong reference lives in the
        // `WatcherHandle` below, so dropping that (via `stop`/`dispose`)
        // drops the watcher, which drops the event sender, which ends this
        // task. A strong clone here would keep the watcher alive forever
        // and the task would never see the channel close.
        let watcher_ref = Arc::downgrade(&watcher);

        let pending: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
        self.pending
            .lock()
            .unwrap()
            .insert(project_root.to_string(), pending.clone());
        let task_pending = pending.clone();
        tokio::spawn(async move {
            let pending = task_pending;
            // Block for the first event in a batch, then drain anything
            // else that arrives within the debounce window before flushing.
            while let Some(first) = event_rx.recv().await {
                let flush_by = std::time::Instant::now() + MAX_BATCH_WINDOW;
                maintain_watches(&watcher_ref, &first).await;
                pending
                    .lock()
                    .unwrap()
                    .extend(to_change(&first, &root, &extensions));
                loop {
                    // The debounce window re-arms on every event, so a
                    // continuous stream (a build, an `npm install`) could
                    // otherwise defer the flush indefinitely while
                    // `pending` grew. `flush_by` caps how long a batch can
                    // be held open regardless of how busy the tree is.
                    let now = std::time::Instant::now();
                    if now >= flush_by {
                        break;
                    }
                    let window = DEBOUNCE_WINDOW.min(flush_by - now);
                    match tokio::time::timeout(window, event_rx.recv()).await {
                        Ok(Some(e)) => {
                            maintain_watches(&watcher_ref, &e).await;
                            pending
                                .lock()
                                .unwrap()
                                .extend(to_change(&e, &root, &extensions));
                        }
                        // Quiet for a full debounce window, or the sender
                        // is gone; either way the batch is done.
                        Ok(None) | Err(_) => break,
                    }
                }
                let changes = coalesce(std::mem::take(&mut *pending.lock().unwrap()));
                if !changes.is_empty() {
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

    /// Hands over (and clears) the changes seen for `project_root` that
    /// the debounce window is still holding.
    ///
    /// A batch waits 100ms to 1s before it is sent, so an agent that edits
    /// a file and immediately asks for diagnostics used to be answered
    /// before the server had heard of the edit. The daemon calls this
    /// before every request, so nothing seen so far is held back.
    pub async fn take_pending(&self, project_root: &str) -> Vec<Value> {
        let Some(buffer) = self.pending.lock().unwrap().get(project_root).cloned() else {
            return vec![];
        };
        let taken = std::mem::take(&mut *buffer.lock().unwrap());
        coalesce(taken)
    }

    pub async fn stop(&self, project_root: &str) {
        self.pending.lock().unwrap().remove(project_root);
        self.watchers.lock().await.remove(project_root);
    }

    pub async fn dispose(&self) {
        self.pending.lock().unwrap().clear();
        self.watchers.lock().await.clear();
    }
}

/// LSP `FileChangeType`: 1 = Created, 2 = Changed, 3 = Deleted.
///
/// A rename becomes a deletion of the old path and a creation of the new
/// one, which is what it is to a language server: inotify reports it as
/// `Name(From)` + `Name(To)` (and/or `Name(Both)` with both paths), and
/// every one of those used to be sent as "Changed" — the old path was never
/// reported gone, and the new one never reported new. A rename with no
/// stated direction (`Name(Any)`, as on macOS) is decided by whether the
/// path still exists.
fn to_change(
    event: &notify::Event,
    project_root: &str,
    extensions: &HashSet<String>,
) -> Vec<Value> {
    use notify::event::RenameMode;
    let typed: Vec<(&std::path::PathBuf, u8)> = match event.kind {
        EventKind::Create(_) => event.paths.iter().map(|p| (p, 1)).collect(),
        EventKind::Remove(_) => event.paths.iter().map(|p| (p, 3)).collect(),
        EventKind::Modify(ModifyKind::Name(mode)) => match (mode, event.paths.as_slice()) {
            (RenameMode::Both, [from, to, ..]) => vec![(from, 3), (to, 1)],
            (RenameMode::From, paths) => paths.iter().map(|p| (p, 3)).collect(),
            (RenameMode::To, paths) => paths.iter().map(|p| (p, 1)).collect(),
            (_, paths) => paths
                .iter()
                .map(|p| (p, if p.exists() { 1 } else { 3 }))
                .collect(),
        },
        EventKind::Modify(_) => event.paths.iter().map(|p| (p, 2)).collect(),
        _ => vec![],
    };
    typed
        .into_iter()
        .filter(|(path, _)| is_relevant(path, project_root, extensions))
        .map(|(path, ty)| json!({ "uri": lsp::uri::from_path(path), "type": ty }))
        .collect()
}

fn is_relevant(path: &Path, project_root: &str, extensions: &HashSet<String>) -> bool {
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
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.contains(&format!(".{}", e.to_lowercase())))
}

/// Collapses a batch to at most one change per URI, in first-seen order.
///
/// Saving one file repeatedly (or a build rewriting it) produced one entry
/// per write — 300 appends, 300 "Changed" notifications for a single
/// file. What a server needs is the net effect: created-then-changed is
/// created, deleted-then-created is a change, and otherwise the latest
/// wins. Created-then-deleted is reported as deleted rather than dropped: a
/// "created" can come from a rename onto a file the server already knew (an
/// editor's atomic save), and deleting a file a server never heard of is
/// harmless.
fn coalesce(changes: Vec<Value>) -> Vec<Value> {
    let mut order: Vec<String> = vec![];
    let mut net: HashMap<String, Option<u64>> = HashMap::new();
    for change in changes {
        let (Some(uri), Some(ty)) = (change["uri"].as_str(), change["type"].as_u64()) else {
            continue;
        };
        let merged = match net.get(uri).copied().flatten() {
            None => Some(ty),
            Some(1) if ty == 2 => Some(1),
            Some(3) if ty == 1 => Some(2),
            Some(_) => Some(ty),
        };
        if !net.contains_key(uri) {
            order.push(uri.to_string());
        }
        net.insert(uri.to_string(), merged);
    }
    order
        .into_iter()
        .filter_map(|uri| {
            let ty = net.get(&uri).copied().flatten()?;
            Some(json!({ "uri": uri, "type": ty }))
        })
        .collect()
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
            )[0]["type"],
            1
        );
        assert_eq!(
            to_change(
                &evt(EventKind::Modify(ModifyKind::Any), "/p/a.ts"),
                "/p",
                &exts
            )[0]["type"],
            2
        );
        assert_eq!(
            to_change(
                &evt(EventKind::Remove(RemoveKind::File), "/p/a.ts"),
                "/p",
                &exts
            )[0]["type"],
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
        .is_empty());
    }

    #[test]
    fn filters_out_dotfiles_and_ignored_directories() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/.git/a.ts"),
            "/p",
            &exts
        )
        .is_empty());
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/node_modules/a.ts"),
            "/p",
            &exts
        )
        .is_empty());
        assert!(to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/dist/a.ts"),
            "/p",
            &exts
        )
        .is_empty());
    }

    #[test]
    fn builds_a_file_uri_from_the_absolute_path() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let v = to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/p/a.ts"),
            "/p",
            &exts,
        );
        assert_eq!(v[0]["uri"], "file:///p/a.ts");
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
        assert_eq!(v.len(), 1, "event under a dot-directory root was dropped");
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
        .is_empty());
    }

    fn rename(mode: RenameMode, paths: &[&str]) -> notify::Event {
        notify::Event {
            kind: EventKind::Modify(ModifyKind::Name(mode)),
            paths: paths.iter().map(std::path::PathBuf::from).collect(),
            attrs: Default::default(),
        }
    }

    fn summary(changes: &[Value]) -> Vec<(String, u64)> {
        changes
            .iter()
            .map(|c| {
                (
                    c["uri"].as_str().unwrap().to_string(),
                    c["type"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn a_rename_is_a_deletion_of_the_old_path_and_a_creation_of_the_new() {
        // Every rename event used to be reported as "Changed": the old
        // path was never said to be gone and the new one never said to
        // exist.
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let both = to_change(
            &rename(RenameMode::Both, &["/p/old.ts", "/p/new.ts"]),
            "/p",
            &exts,
        );
        assert_eq!(
            summary(&both),
            [
                ("file:///p/old.ts".into(), 3),
                ("file:///p/new.ts".into(), 1)
            ]
        );
        let from = to_change(&rename(RenameMode::From, &["/p/old.ts"]), "/p", &exts);
        assert_eq!(summary(&from), [("file:///p/old.ts".into(), 3)]);
        let to = to_change(&rename(RenameMode::To, &["/p/new.ts"]), "/p", &exts);
        assert_eq!(summary(&to), [("file:///p/new.ts".into(), 1)]);
        // inotify sends From, To and Both for one `mv`; coalesced, that is
        // exactly one deletion and one creation.
        let all: Vec<Value> = [from, to, both].concat();
        assert_eq!(
            summary(&coalesce(all)),
            [
                ("file:///p/old.ts".into(), 3),
                ("file:///p/new.ts".into(), 1)
            ]
        );
    }

    #[test]
    fn an_undirected_rename_is_judged_by_whether_the_path_exists() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("here.ts");
        std::fs::write(&present, "").unwrap();
        let gone = dir.path().join("gone.ts");
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let root = dir.path().to_str().unwrap();
        let v = to_change(
            &rename(RenameMode::Any, &[present.to_str().unwrap()]),
            root,
            &exts,
        );
        assert_eq!(v[0]["type"], 1);
        let v = to_change(
            &rename(RenameMode::Any, &[gone.to_str().unwrap()]),
            root,
            &exts,
        );
        assert_eq!(v[0]["type"], 3);
    }

    #[test]
    fn a_rename_out_of_the_watched_extensions_reports_only_the_relevant_side() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let v = to_change(
            &rename(RenameMode::Both, &["/p/a.ts", "/p/a.ts.bak"]),
            "/p",
            &exts,
        );
        assert_eq!(summary(&v), [("file:///p/a.ts".into(), 3)]);
    }

    #[test]
    fn coalesce_keeps_one_net_change_per_file_in_first_seen_order() {
        let c = |uri: &str, ty: u64| json!({ "uri": uri, "type": ty });
        let repeated: Vec<Value> = (0..300).map(|_| c("file:///a", 2)).collect();
        assert_eq!(summary(&coalesce(repeated)), [("file:///a".into(), 2)]);
        let batch = vec![
            c("file:///new", 1),
            c("file:///b", 2),
            c("file:///new", 2), // created then written: still created
            c("file:///tmp", 1),
            c("file:///tmp", 3), // created and gone again: gone
            c("file:///x", 3),
            c("file:///x", 1), // deleted and recreated: changed
        ];
        assert_eq!(
            summary(&coalesce(batch)),
            [
                ("file:///new".into(), 1),
                ("file:///b".into(), 2),
                ("file:///tmp".into(), 3),
                ("file:///x".into(), 2)
            ]
        );
    }

    #[test]
    fn paths_with_spaces_are_percent_encoded_in_the_uri() {
        let exts: HashSet<String> = [".ts"].iter().map(|s| s.to_string()).collect();
        let v = to_change(
            &evt(EventKind::Modify(ModifyKind::Any), "/my proj/a.ts"),
            "/my proj",
            &exts,
        );
        assert_eq!(v[0]["uri"], "file:///my%20proj/a.ts");
    }

    // --- watch registration -------------------------------------------
    // The watch set used to be one recursive watch on the project root,
    // which on Linux is one inotify descriptor per directory underneath —
    // including every directory in target/ and node_modules/. Filtering in
    // `to_change` happened only after the kernel had already registered
    // the watch and delivered the event, so it saved neither.

    fn mkdirs(root: &std::path::Path, rels: &[&str]) {
        for rel in rels {
            std::fs::create_dir_all(root.join(rel)).unwrap();
        }
    }

    fn new_watcher() -> notify::RecommendedWatcher {
        notify::recommended_watcher(|_res: notify::Result<notify::Event>| {}).unwrap()
    }

    #[test]
    fn watch_tree_skips_ignored_directories() {
        let dir = tempfile::tempdir().unwrap();
        mkdirs(
            dir.path(),
            &[
                "src",
                "src/nested",
                "node_modules/pkg/dist",
                "target/debug/build",
                ".git/objects",
                "dist",
            ],
        );

        let mut watcher = new_watcher();
        let count = watch_tree(&mut watcher, dir.path());

        // root + src + src/nested. Everything under node_modules, target,
        // .git and dist is skipped, and so are those directories
        // themselves.
        assert_eq!(
            count, 3,
            "expected only the root and src tree to be watched"
        );
    }

    #[test]
    fn watch_tree_watches_a_root_that_is_itself_a_dot_directory() {
        // walkdir applies filter_entry to the root, so without the
        // depth() == 0 exemption a project at ~/.config/nvim would prune
        // itself and end up with no watches at all.
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join(".dotfiles");
        std::fs::create_dir_all(root.join("nested")).unwrap();

        let mut watcher = new_watcher();
        assert_eq!(watch_tree(&mut watcher, &root), 2);
    }

    #[test]
    fn watch_tree_counts_every_surviving_directory() {
        let dir = tempfile::tempdir().unwrap();
        mkdirs(dir.path(), &["a", "a/b", "a/b/c", "d"]);
        let mut watcher = new_watcher();
        // root, a, a/b, a/b/c, d
        assert_eq!(watch_tree(&mut watcher, dir.path()), 5);
    }

    #[test]
    fn watch_tree_on_a_missing_path_registers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = new_watcher();
        assert_eq!(watch_tree(&mut watcher, &dir.path().join("nope")), 0);
    }
}
