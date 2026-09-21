/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Active-content and editable-theme generation synchronization.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use log::{debug, info, warn};
use notify::{Event, RecursiveMode, Watcher};
use snafu::{Report, ResultExt, Whatever};

use crate::{git, live::LiveSource, proposal::OnlyRenderPlan, theme_live::LiveTheme};

#[derive(Debug)]
pub(crate) struct DirtyServeWatcher {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct ActiveRepoServeSync {
    source_root: PathBuf,
    build_repo_path: PathBuf,
    only_plan: Option<OnlyRenderPlan>,
    source_generation: Option<git::SourceGeneration>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalThemeServeSync {
    pub(crate) owned_root: PathBuf,
    pub(crate) theme_source_root: PathBuf,
    pub(crate) mounted_theme_dir: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ServeSyncConfig {
    configuration: crate::execution::ConfigurationSnapshot,
    active_repo: Option<ActiveRepoServeSync>,
    local_theme: Option<LocalThemeServeSync>,
}

impl ServeSyncConfig {
    pub(crate) fn set_configuration(&mut self, snapshot: crate::execution::ConfigurationSnapshot) {
        self.configuration = snapshot;
    }
    pub(crate) fn set_source_generation(&mut self, generation: git::SourceGeneration) {
        if let Some(active) = &mut self.active_repo {
            active.source_generation = Some(generation);
        }
    }

    pub(crate) fn has_targets(&self) -> bool {
        self.active_repo.is_some() || self.local_theme.is_some()
    }
}

impl DirtyServeWatcher {
    pub(crate) fn start(sync_config: ServeSyncConfig) -> Result<Self, Whatever> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread =
            thread::spawn(move || dirty_serve_sync_loop(sync_config, stop_thread, ready_tx));

        match ready_rx
            .recv()
            .whatever_context("dirty serve watcher exited before initialization")?
        {
            Ok(()) => Ok(Self { stop, thread }),
            Err(message) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                snafu::whatever!("{message}");
            }
        }
    }

    pub(crate) fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

fn path_is_watched_source_path(root_path: &Path, path: &Path) -> bool {
    let Ok(relative_path) = path.strip_prefix(root_path) else {
        return false;
    };

    relative_path
        .components()
        .next()
        .map(|component| component.as_os_str() != OsStr::new(".git"))
        .unwrap_or(false)
}

fn read_only_event(event: &Event) -> bool {
    matches!(event.kind, notify::EventKind::Access(_))
        && !matches!(
            event.kind,
            notify::EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write
            ))
        )
}

fn index_lock_path(index_path: &Path) -> Option<PathBuf> {
    let file_name = index_path.file_name()?.to_string_lossy();
    Some(index_path.with_file_name(format!("{file_name}.lock")))
}

fn event_has_theme_index_path(index_path: &Path, event: &Event) -> bool {
    let lock_path = index_lock_path(index_path);
    event.paths.iter().any(|path| {
        path == index_path
            || lock_path
                .as_ref()
                .map(|lock_path| path == lock_path)
                .unwrap_or(false)
    })
}

#[derive(Debug)]
struct GitWatchPaths {
    git_dir: PathBuf,
    common_dir: PathBuf,
    index: PathBuf,
}

impl GitWatchPaths {
    fn load(source: &Path) -> Result<Self, Whatever> {
        let repo = git::open_usable_repository(source)
            .whatever_context("unable to open watched source repository")?;
        Ok(Self {
            git_dir: repo.path().to_path_buf(),
            common_dir: repo.commondir().to_path_buf(),
            index: git::index_path(source).whatever_context("unable to locate active index")?,
        })
    }

    fn has_event(&self, source: &Path, excluded: Option<&Path>, event: &Event) -> bool {
        if read_only_event(event) {
            return false;
        }
        event.paths.iter().any(|path| {
            (path_is_watched_source_path(source, path)
                && !excluded.is_some_and(|root| path.starts_with(root)))
                || path == &self.git_dir.join("HEAD")
                || path == &self.common_dir.join("packed-refs")
                || path.starts_with(self.common_dir.join("refs"))
                || event_has_theme_index_path(&self.index, event)
        })
    }

    fn watch(&self, watcher: &mut notify::RecommendedWatcher) -> Result<(), String> {
        watcher
            .watch(&self.git_dir, RecursiveMode::NonRecursive)
            .map_err(|e| format!("unable to watch active Git metadata: {e}"))?;
        if self.common_dir != self.git_dir {
            watcher
                .watch(&self.common_dir, RecursiveMode::NonRecursive)
                .map_err(|e| format!("unable to watch common Git metadata: {e}"))?;
        }
        let refs = self.common_dir.join("refs");
        if refs.is_dir() {
            watcher
                .watch(&refs, RecursiveMode::Recursive)
                .map_err(|e| format!("unable to watch active source refs: {e}"))?;
        }
        watch_theme_index(watcher, &self.index)
    }
}

fn watch_theme_index(
    watcher: &mut notify::RecommendedWatcher,
    theme_index_path: &Path,
) -> Result<(), String> {
    let file_result = watcher.watch(theme_index_path, RecursiveMode::NonRecursive);
    let Some(parent) = theme_index_path.parent() else {
        return file_result.map_err(|file_error| {
            format!(
                "unable to watch local theme Git index `{}`: {file_error}",
                theme_index_path.to_string_lossy()
            )
        });
    };
    let parent_result = watcher.watch(parent, RecursiveMode::NonRecursive);

    match (file_result, parent_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(parent_error)) => {
            debug!(
                "unable to watch local theme Git index parent `{}`: {parent_error}",
                parent.to_string_lossy()
            );
            Ok(())
        }
        (Err(file_error), Ok(())) => {
            debug!(
                "using local theme Git index parent watch for `{}` after file watch failed: {file_error}",
                theme_index_path.to_string_lossy()
            );
            Ok(())
        }
        (Err(file_error), Err(parent_error)) => Err(format!(
            "unable to watch local theme Git index `{}`: {file_error}; fallback watch on `{}` also failed: {parent_error}",
            theme_index_path.to_string_lossy(),
            parent.to_string_lossy()
        )),
    }
}

fn dirty_serve_sync_loop(
    sync_config: ServeSyncConfig,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<(), String>>,
) {
    let (event_tx, event_rx) = mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |result| {
        let _ = event_tx.send(result);
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("unable to start dirty serve watcher: {error}")));
            return;
        }
    };

    if let Some(active_repo) = &sync_config.active_repo {
        if let Err(error) = watcher.watch(&active_repo.source_root, RecursiveMode::Recursive) {
            let _ = ready_tx.send(Err(format!(
                "unable to watch `{}` for dirty serve changes: {error}",
                active_repo.source_root.to_string_lossy()
            )));
            return;
        }
    }

    let active_watch_paths = match &sync_config.active_repo {
        Some(active) => match GitWatchPaths::load(&active.source_root) {
            Ok(paths) => {
                if let Err(message) = paths.watch(&mut watcher) {
                    let _ = ready_tx.send(Err(message));
                    return;
                }
                Some(paths)
            }
            Err(error) => {
                let _ = ready_tx.send(Err(format!(
                    "unable to watch active Git state: {}",
                    Report::from_error(error)
                )));
                return;
            }
        },
        None => None,
    };

    let theme_watch_paths = match &sync_config.local_theme {
        Some(theme) => {
            if let Err(error) = watcher.watch(&theme.theme_source_root, RecursiveMode::Recursive) {
                let _ = ready_tx.send(Err(format!("unable to watch theme source: {error}")));
                return;
            }
            match GitWatchPaths::load(&theme.theme_source_root) {
                Ok(paths) => {
                    if let Err(error) = paths.watch(&mut watcher) {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                    Some(paths)
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(format!(
                        "unable to watch theme Git state: {}",
                        Report::from_error(error)
                    )));
                    return;
                }
            }
        }
        None => None,
    };

    for (path, _) in &sync_config.configuration.0 {
        if let Some(parent) = path.parent() {
            if let Err(error) = watcher.watch(parent, RecursiveMode::NonRecursive) {
                let _ = ready_tx.send(Err(format!(
                    "unable to watch resolved configuration: {error}"
                )));
                return;
            }
        }
    }
    if let Err(error) = sync_config.configuration.verify() {
        let _ = ready_tx.send(Err(format!(
            "invalid serve configuration: {}",
            Report::from_error(error)
        )));
        return;
    }
    let mut active_state = match &sync_config.active_repo {
        Some(active) => {
            let Some(commit) = active.source_generation else {
                let _ = ready_tx.send(Err(
                    "serve requires the recorded prepared active generation".into(),
                ));
                return;
            };
            match LiveSource::new(
                &active.source_root,
                &active.build_repo_path,
                active.only_plan.as_ref(),
                commit,
                sync_config.configuration.clone(),
            ) {
                Ok(state) => Some(state),
                Err(error) => {
                    let _ = ready_tx.send(Err(format!(
                        "unable to initialize live source generation: {}",
                        Report::from_error(error)
                    )));
                    return;
                }
            }
        }
        None => None,
    };

    let mut theme_state = match &sync_config.local_theme {
        Some(config) => match LiveTheme::new(config.clone(), sync_config.configuration.clone()) {
            Ok(state) => Some(state),
            Err(error) => {
                let _ = ready_tx.send(Err(format!(
                    "unable to initialize live theme: {}",
                    Report::from_error(error)
                )));
                return;
            }
        },
        None => None,
    };

    if let Some(active_repo) = &sync_config.active_repo {
        info!(
            "watching `{}` for dirty serve changes",
            active_repo.source_root.to_string_lossy()
        );
    }
    if let Some(local_theme) = &sync_config.local_theme {
        info!(
            "watching `{}` for local theme serve changes",
            local_theme.theme_source_root.to_string_lossy()
        );
    }
    let _ = ready_tx.send(Ok(()));

    let mut retry_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let first_event = match event_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let retry_due = Instant::now() >= retry_at;
        let retry_active = retry_due
            && active_state
                .as_ref()
                .is_some_and(LiveSource::needs_publication_retry);
        let retry_theme = retry_due
            && theme_state
                .as_ref()
                .is_some_and(LiveTheme::needs_publication_retry);
        if first_event.is_none() && !retry_active && !retry_theme {
            continue;
        }
        let mut saw_active_event = retry_active;
        let mut saw_theme_event = retry_theme;
        let mut classify = |event: &Event| {
            if read_only_event(event) {
                return;
            }
            let configuration_changed = sync_config
                .configuration
                .0
                .iter()
                .any(|(p, _)| event.paths.contains(p));
            saw_active_event |= configuration_changed
                || sync_config.active_repo.as_ref().is_some_and(|active| {
                    active_watch_paths.as_ref().is_some_and(|paths| {
                        paths.has_event(&active.source_root, active.build_repo_path.parent(), event)
                    })
                });
            saw_theme_event |= configuration_changed
                || sync_config.local_theme.as_ref().is_some_and(|theme| {
                    theme_watch_paths.as_ref().is_some_and(|paths| {
                        paths.has_event(&theme.theme_source_root, theme.owned_root.parent(), event)
                    })
                });
        };
        match first_event {
            Some(Ok(event)) => classify(&event),
            Some(Err(error)) => warn!("filesystem watcher error: {error}"),
            None => {}
        }

        let debounce_start = Instant::now();
        loop {
            if debounce_start.elapsed() >= Duration::from_millis(500) {
                break;
            }
            match event_rx.recv_timeout(Duration::from_millis(75)) {
                Ok(Ok(event)) => classify(&event),
                Ok(Err(error)) => warn!("filesystem watcher error: {error}"),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        if saw_active_event || saw_theme_event {
            retry_at = Instant::now() + Duration::from_secs(1);
        }
        if (saw_active_event || saw_theme_event) && sync_config.configuration.verify().is_err() {
            warn!("resolved configuration changed or became unavailable; restart serve. Last-good input retained");
            continue;
        }

        if saw_active_event {
            if let Some(state) = &mut active_state {
                if let Err(error) = state.synchronize() {
                    warn!(
                        "unable to synchronize live source changes: {}",
                        Report::from_error(error)
                    );
                }
            }
        }

        if saw_theme_event {
            if let Some(state) = &mut theme_state {
                if let Err(error) = state.synchronize() {
                    warn!(
                        "unable to synchronize local theme: {}",
                        Report::from_error(error)
                    );
                }
            }
        }
    }
}

pub(crate) fn serve_sync_config(
    source_materialization: git::SourceMaterialization,
    source_root: &Path,
    repo_path: &Path,
    only_plan: Option<OnlyRenderPlan>,
    local_theme_sync: Option<LocalThemeServeSync>,
) -> ServeSyncConfig {
    ServeSyncConfig {
        configuration: Default::default(),
        active_repo: (source_materialization == git::SourceMaterialization::Dirty).then(|| {
            ActiveRepoServeSync {
                source_root: source_root.to_path_buf(),
                build_repo_path: repo_path.to_path_buf(),
                only_plan,
                source_generation: None,
            }
        }),
        local_theme: local_theme_sync,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use git2::{IndexAddOption, Repository, Signature};
    use notify::{Event, EventKind};
    use tempfile::TempDir;

    use crate::{
        git::{self, SourceMaterialization},
        proposal::{OnlyRenderPlan, ProposalNumber},
    };

    use super::{event_has_theme_index_path, serve_sync_config, LocalThemeServeSync};

    fn fake_theme_sync(root: &Path) -> LocalThemeServeSync {
        LocalThemeServeSync {
            owned_root: root.join("repo"),
            theme_source_root: root.join("theme"),
            mounted_theme_dir: root.join("repo/themes/eips-theme"),
        }
    }

    fn number(value: u32) -> ProposalNumber {
        ProposalNumber::from_u32(value).unwrap()
    }

    fn write_file(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn commit_all(repo: &Repository, message: &str) {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let signature = Signature::now("build-eips test", "build-eips@example.test").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repo.find_commit(oid).unwrap())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap();
    }

    fn init_repo(root: &Path, files: &[(&str, &str)]) -> Repository {
        std::fs::create_dir_all(root).unwrap();
        let repo = Repository::init(root).unwrap();
        repo.set_head("refs/heads/master").unwrap();
        for (relative, contents) in files {
            write_file(root, relative, contents);
        }
        commit_all(&repo, "initial");
        repo
    }

    fn proposal_markdown(value: u32, extra_preamble: &str, body: &str) -> String {
        format!("---\neip: {value}\ntitle: Proposal {value}\n{extra_preamble}---\n{body}\n")
    }

    fn only_plan(root: &Path) -> OnlyRenderPlan {
        let content = root.join("content");
        write_file(&content, "00555.md", &proposal_markdown(555, "", "Body"));
        write_file(&content, "00678.md", &proposal_markdown(678, "", "Body"));
        OnlyRenderPlan::build(&content, [number(555)].into_iter().collect()).unwrap()
    }

    fn rendered_body(path: &Path) -> String {
        let contents = std::fs::read_to_string(path).unwrap();
        contents.split_once("\n+++\n").unwrap().1.to_owned()
    }

    fn dirty_sync_fixture() -> (TempDir, PathBuf, PathBuf, OnlyRenderPlan) {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let build = temp.path().join("build/repo");
        let selected = proposal_markdown(555, "requires: 678\n", "See [EIP-678](/00678.md).");
        let unselected = proposal_markdown(678, "", "Unselected.");
        init_repo(
            &source,
            &[
                ("content/00555.md", selected.as_str()),
                ("content/00555/assets/diagram.png", "selected image\n"),
                ("content/00678.md", unselected.as_str()),
                ("content/00678/assets/diagram.png", "unselected image\n"),
                (
                    "content/_index.md",
                    "---\ntitle: Home\n---\nSee [EIP-678](/00678.md).\n",
                ),
            ],
        );
        Repository::clone(source.to_str().unwrap(), &build).unwrap();
        let plan =
            OnlyRenderPlan::build(&build.join("content"), [number(555)].into_iter().collect())
                .unwrap();
        crate::markdown::preprocess(&build.join("content"), Some(&plan)).unwrap();
        plan.prune_content(&build.join("content")).unwrap();
        (temp, source, build, plan)
    }

    #[test]
    fn local_theme_serve_syncs_tracked_edits_into_mounted_theme() {
        let temp = TempDir::new().unwrap();
        let theme_root = temp.path().join("workspace/theme");
        let mounted_theme_dir = temp.path().join("build/repo/themes/eips-theme");
        init_repo(
            &theme_root,
            &[
                ("config/zola.toml", "title = 'theme'\n"),
                ("templates/index.html", "committed local theme\n"),
            ],
        );
        git::materialize_working_tree(&theme_root, &mounted_theme_dir).unwrap();
        let mut state = crate::theme_live::LiveTheme::new(
            LocalThemeServeSync {
                owned_root: temp.path().join("build/repo"),
                theme_source_root: theme_root.clone(),
                mounted_theme_dir: mounted_theme_dir.clone(),
            },
            Default::default(),
        )
        .unwrap();
        write_file(&theme_root, "templates/index.html", "dirty local theme\n");
        state.synchronize().unwrap();

        assert_eq!(
            std::fs::read_to_string(mounted_theme_dir.join("templates/index.html")).unwrap(),
            "dirty local theme\n"
        );
    }

    #[test]
    fn local_theme_index_events_trigger_rescan() {
        let index_path = PathBuf::from("/workspace/theme/.git/index");
        let index_event = Event::new(EventKind::Any).add_path(index_path.clone());
        let lock_event =
            Event::new(EventKind::Any).add_path(PathBuf::from("/workspace/theme/.git/index.lock"));
        let unrelated_event =
            Event::new(EventKind::Any).add_path(PathBuf::from("/workspace/theme/.git/config"));

        assert!(event_has_theme_index_path(&index_path, &index_event));
        assert!(event_has_theme_index_path(&index_path, &lock_event));
        assert!(!event_has_theme_index_path(&index_path, &unrelated_event));
    }

    #[test]
    fn local_serve_syncs_theme_and_dirty_active_repo() {
        let temp = TempDir::new().unwrap();
        let plan = only_plan(temp.path());

        let sync_config = serve_sync_config(
            SourceMaterialization::Dirty,
            &temp.path().join("Core"),
            &temp.path().join(".local-build/Core/repo"),
            Some(plan),
            Some(fake_theme_sync(temp.path())),
        );

        assert!(sync_config.active_repo.is_some());
        assert!(sync_config
            .active_repo
            .as_ref()
            .unwrap()
            .only_plan
            .is_some());
        assert!(sync_config.local_theme.is_some());
    }

    #[test]
    fn clean_local_serve_keeps_theme_sync_but_disables_active_repo_dirty_sync() {
        let temp = TempDir::new().unwrap();

        let sync_config = serve_sync_config(
            SourceMaterialization::Clean,
            &temp.path().join("Core"),
            &temp.path().join(".local-build/Core/repo"),
            None,
            Some(fake_theme_sync(temp.path())),
        );

        assert!(sync_config.active_repo.is_none());
        assert!(sync_config.local_theme.is_some());
    }

    #[test]
    fn only_dirty_sync_does_not_reintroduce_unselected_markdown_or_assets() {
        let (_temp, source, build, plan) = dirty_sync_fixture();
        let commit = git::head_commit_id(&Repository::open(&source).unwrap()).unwrap();
        let mut live = crate::live::LiveSource::new(
            &source,
            &build,
            Some(&plan),
            git::SourceGeneration {
                source_head: commit,
                materialized_head: commit,
            },
            Default::default(),
        )
        .unwrap();
        write_file(
            &source,
            "content/00678.md",
            &proposal_markdown(678, "", "Dirty unselected."),
        );
        write_file(
            &source,
            "content/00678/assets/diagram.png",
            "dirty unselected image\n",
        );
        live.synchronize().unwrap();

        assert!(!build.join("content/00678.md").exists());
        assert!(!build.join("content/00678/assets/diagram.png").exists());
    }

    #[test]
    fn only_dirty_sync_copies_selected_assets_without_markdown_preprocessing() {
        let (_temp, source, build, plan) = dirty_sync_fixture();
        let commit = git::head_commit_id(&Repository::open(&source).unwrap()).unwrap();
        let mut live = crate::live::LiveSource::new(
            &source,
            &build,
            Some(&plan),
            git::SourceGeneration {
                source_head: commit,
                materialized_head: commit,
            },
            Default::default(),
        )
        .unwrap();
        write_file(
            &source,
            "content/00555/assets/diagram.png",
            "dirty selected image\n",
        );
        live.synchronize().unwrap();

        assert_eq!(
            std::fs::read_to_string(build.join("content/00555/assets/diagram.png")).unwrap(),
            "dirty selected image\n"
        );
    }

    #[test]
    fn only_dirty_sync_preprocesses_selected_and_retained_markdown_with_plan() {
        let (_temp, source, build, plan) = dirty_sync_fixture();
        let commit = git::head_commit_id(&Repository::open(&source).unwrap()).unwrap();
        let mut live = crate::live::LiveSource::new(
            &source,
            &build,
            Some(&plan),
            git::SourceGeneration {
                source_head: commit,
                materialized_head: commit,
            },
            Default::default(),
        )
        .unwrap();
        write_file(
            &source,
            "content/00555.md",
            &proposal_markdown(555, "requires: 678\n", "Dirty [EIP-678](/00678.md)."),
        );
        write_file(
            &source,
            "content/_index.md",
            "---\ntitle: Home\n---\nDirty [EIP-678](/00678.md).\n",
        );
        live.synchronize().unwrap();

        let selected = std::fs::read_to_string(build.join("content/00555.md")).unwrap();
        let index_body = rendered_body(&build.join("content/_index.md"));
        assert!(selected.contains("https://eips.ethereum.org/EIPS/eip-678"));
        assert!(index_body.contains("https://eips.ethereum.org/EIPS/eip-678"));
    }

    #[test]
    fn only_dirty_sync_propagates_selected_proposal_deletion() {
        let (_temp, source, build, plan) = dirty_sync_fixture();
        let commit = git::head_commit_id(&Repository::open(&source).unwrap()).unwrap();
        let mut live = crate::live::LiveSource::new(
            &source,
            &build,
            Some(&plan),
            git::SourceGeneration {
                source_head: commit,
                materialized_head: commit,
            },
            Default::default(),
        )
        .unwrap();
        std::fs::remove_file(source.join("content/00555.md")).unwrap();
        live.synchronize().unwrap();

        assert!(!build.join("content/00555.md").exists());
    }
}

#[cfg(test)]
mod live_generation_regressions {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn markdown(n: u32, body: &str) -> String {
        format!("---\neip: {n}\ntitle: Proposal {n}\n---\n{body}\n")
    }
    fn commit(repo: &Repository) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let id = index.write_tree().unwrap();
        let tree = repo.find_tree(id).unwrap();
        let sig = Signature::now("test", "test@example.invalid").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "test",
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    }
    struct Fixture {
        _temp: tempfile::TempDir,
        source: PathBuf,
        build: PathBuf,
        repo: Repository,
        generation: git::SourceGeneration,
        plan: Option<OnlyRenderPlan>,
    }
    impl Fixture {
        fn new(targeted: bool) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            let build = temp.path().join("build/repo");
            fs::create_dir(&source).unwrap();
            let repo = Repository::init(&source).unwrap();
            write(&source, "content/00001.md", &markdown(1, "ORIGINAL"));
            write(&source, "content/00002.md", &markdown(2, "OTHER"));
            let head = commit(&repo);
            Repository::clone(source.to_str().unwrap(), &build).unwrap();
            let plan = targeted.then(|| {
                OnlyRenderPlan::build(
                    &build.join("content"),
                    [crate::proposal::ProposalNumber::from_u32(1).unwrap()]
                        .into_iter()
                        .collect(),
                )
                .unwrap()
            });
            crate::markdown::preprocess(&build.join("content"), plan.as_ref()).unwrap();
            if let Some(plan) = &plan {
                plan.prune_content(&build.join("content")).unwrap();
            }
            Self {
                _temp: temp,
                source,
                build,
                repo,
                generation: git::SourceGeneration {
                    source_head: head,
                    materialized_head: head,
                },
                plan,
            }
        }
        fn live(&self) -> LiveSource {
            LiveSource::new(
                &self.source,
                &self.build,
                self.plan.as_ref(),
                self.generation,
                Default::default(),
            )
            .unwrap()
        }
        fn stage(&self, path: &str) {
            let mut index = self.repo.index().unwrap();
            index.add_path(Path::new(path)).unwrap();
            index.write().unwrap();
        }
    }

    #[test]
    fn staged_new_file_appears_and_unstaging_it_removes_it_without_source_mutation() {
        let f = Fixture::new(false);
        let mut live = f.live();
        let head = f.repo.head().unwrap().target();
        write(&f.source, "content/00003.md", &markdown(3, "NEW"));
        assert_eq!(live.synchronize().unwrap(), 0);
        assert!(!f.build.join("content/00003.md").exists());
        f.stage("content/00003.md");
        let index_before = fs::read(git::index_path(&f.source).unwrap()).unwrap();
        live.synchronize().unwrap();
        assert!(fs::read_to_string(f.build.join("content/00003.md"))
            .unwrap()
            .starts_with("+++"));
        assert_eq!(
            fs::read(git::index_path(&f.source).unwrap()).unwrap(),
            index_before
        );
        let mut index = f.repo.index().unwrap();
        index.remove_path(Path::new("content/00003.md")).unwrap();
        index.write().unwrap();
        live.synchronize().unwrap();
        assert!(!f.build.join("content/00003.md").exists());
        assert!(f.source.join("content/00003.md").exists());
        assert_eq!(f.repo.head().unwrap().target(), head);
    }
    #[test]
    fn clean_head_transition_is_compared_to_the_served_generation() {
        let f = Fixture::new(false);
        let mut live = f.live();
        write(
            &f.source,
            "content/00001.md",
            &markdown(1, "NEW CLEAN HEAD"),
        );
        commit(&f.repo);
        assert!(git::working_tree_paths(&f.source).unwrap().is_empty());
        live.synchronize().unwrap();
        assert!(fs::read_to_string(f.build.join("content/00001.md"))
            .unwrap()
            .contains("NEW CLEAN HEAD"));
    }
    #[test]
    fn startup_reconciliation_catches_a_change_after_preparation() {
        let f = Fixture::new(false);
        write(
            &f.source,
            "content/00001.md",
            &markdown(1, "BETWEEN PREPARATION AND WATCH"),
        );
        let _live = f.live();
        assert!(fs::read_to_string(f.build.join("content/00001.md"))
            .unwrap()
            .contains("BETWEEN PREPARATION AND WATCH"));
    }
    #[test]
    fn targeted_inventory_refreshes_and_invalid_updates_retain_last_good_input_until_recovery() {
        let f = Fixture::new(true);
        let mut live = f.live();
        write(&f.source, "content/00001/assets/new.txt", "NEW ASSET");
        f.stage("content/00001/assets/new.txt");
        write(
            &f.source,
            "content/00001.md",
            &markdown(1, "[asset](00001/assets/new.txt)"),
        );
        live.synchronize().unwrap();
        assert!(f.build.join("content/00001/assets/new.txt").exists());
        let good = fs::read(f.build.join("content/00001.md")).unwrap();
        write(
            &f.source,
            "content/00001.md",
            &markdown(1, "[missing](00001/assets/not-there.txt)"),
        );
        for _ in 0..2 {
            assert!(
                live.synchronize().is_err(),
                "an invalid raw candidate must not become successful on retry"
            );
            assert_eq!(fs::read(f.build.join("content/00001.md")).unwrap(), good);
        }
        write(
            &f.source,
            "content/00001.md",
            &markdown(1, "RECOVERED [asset](00001/assets/new.txt)"),
        );
        live.synchronize().unwrap();
        assert!(fs::read_to_string(f.build.join("content/00001.md"))
            .unwrap()
            .contains("RECOVERED"));
        assert!(!f.build.join("content/00002.md").exists());
    }
    #[test]
    fn access_events_do_not_cause_feedback_and_index_events_are_observed() {
        let f = Fixture::new(false);
        let paths = GitWatchPaths::load(&f.source).unwrap();
        let active = ActiveRepoServeSync {
            source_root: f.source.clone(),
            build_repo_path: f.build.clone(),
            only_plan: None,
            source_generation: Some(f.generation),
        };
        let index = Event::new(notify::EventKind::Any).add_path(paths.index.clone());
        assert!(paths.has_event(&active.source_root, active.build_repo_path.parent(), &index));
        let read = Event::new(notify::EventKind::Access(notify::event::AccessKind::Read))
            .add_path(f.source.join("content/00001.md"));
        assert!(!paths.has_event(&active.source_root, active.build_repo_path.parent(), &read));
        let closed = Event::new(notify::EventKind::Access(notify::event::AccessKind::Close(
            notify::event::AccessMode::Write,
        )))
        .add_path(f.source.join("content/00001.md"));
        assert!(paths.has_event(
            &active.source_root,
            active.build_repo_path.parent(),
            &closed
        ));
    }
}
