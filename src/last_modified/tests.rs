use super::*;
use git2::{build::TreeUpdateBuilder, FileMode, Signature, Time};
use std::time::Instant;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    repo: Repository,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let repo = Repository::init(&root).unwrap();
        Self {
            _temp: temp,
            repo,
            root,
        }
    }

    fn commit(&self, files: &[(&str, &str, FileMode)], parents: &[Oid], seconds: i64) -> Oid {
        let empty = self.repo.treebuilder(None).unwrap().write().unwrap();
        let empty = self.repo.find_tree(empty).unwrap();
        let mut updates = TreeUpdateBuilder::new();
        for (path, body, mode) in files {
            let p = self.root.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            updates.upsert(path, self.repo.blob(body.as_bytes()).unwrap(), *mode);
        }
        let tree = updates.create_updated(&self.repo, &empty).unwrap();
        let tree = self.repo.find_tree(tree).unwrap();
        let parents = parents
            .iter()
            .map(|id| self.repo.find_commit(*id).unwrap())
            .collect::<Vec<_>>();
        let sig = Signature::new(
            "history test",
            "history@example.test",
            &Time::new(seconds, 330),
        )
        .unwrap();
        let id = self
            .repo
            .commit(
                None,
                &sig,
                &sig,
                "history fixture",
                &tree,
                &parents.iter().collect::<Vec<_>>(),
            )
            .unwrap();
        self.repo.set_head_detached(id).unwrap();
        id
    }

    fn check(&self, paths: &[&str]) -> Vec<Option<i64>> {
        compare(&self.root, paths.iter().map(PathBuf::from).collect())
    }
}

fn oracle(path: &Path) -> Option<i64> {
    let output = Command::new("git")
        .current_dir(path.parent().unwrap())
        .args(["log", "-1", "--pretty=format:%ct", "--"])
        .arg(path.file_name().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    if text.trim().is_empty() {
        None
    } else {
        Some(text.trim().parse().unwrap())
    }
}

fn compare(root: &Path, paths: Vec<PathBuf>) -> Vec<Option<i64>> {
    let paths = paths.into_iter().map(|p| root.join(p)).collect::<Vec<_>>();
    let batch = collect(&paths[0], &paths).unwrap();
    let mut history = History::new(paths.iter().cloned());
    paths
        .iter()
        .map(|p| {
            let actual = history.timestamp(p).unwrap();
            assert_eq!(actual, oracle(p), "{}", p.display());
            assert_eq!(batch[p], actual, "forced batch: {}", p.display());
            actual
        })
        .collect()
}

const A: &str = "content/00001.md";
const B: &str = "content/00002.md";
const T: i64 = 1_700_000_000;

#[test]
fn linear_unrelated_and_uncommitted_changes_match_git() {
    let f = Fixture::new();
    let a = f.commit(
        &[(A, "one", FileMode::Blob), (B, "two", FileMode::Blob)],
        &[],
        T,
    );
    let b = f.commit(
        &[(A, "changed", FileMode::Blob), (B, "two", FileMode::Blob)],
        &[a],
        T + 10,
    );
    f.commit(
        &[
            (A, "changed", FileMode::Blob),
            (B, "two", FileMode::Blob),
            ("README", "unrelated", FileMode::Blob),
        ],
        &[b],
        T + 20,
    );
    std::fs::write(
        f.root.join(A),
        "uncommitted content is not a history commit",
    )
    .unwrap();
    assert_eq!(
        f.check(&[A, B, "content/never.md"]),
        vec![Some(T + 10), Some(T), None]
    );
}

#[test]
fn merge_follows_first_equal_parent_not_latest_date_or_broad_log() {
    let f = Fixture::new();
    let a = f.commit(
        &[(A, "base", FileMode::Blob), (B, "base", FileMode::Blob)],
        &[],
        T,
    );
    let left = f.commit(
        &[(A, "left", FileMode::Blob), (B, "base", FileMode::Blob)],
        &[a],
        T + 10,
    );
    let right = f.commit(
        &[(A, "right", FileMode::Blob), (B, "base", FileMode::Blob)],
        &[a],
        T + 20,
    );
    f.commit(
        &[(A, "left", FileMode::Blob), (B, "base", FileMode::Blob)],
        &[left, right],
        T + 30,
    );
    assert_eq!(f.check(&[A, B]), vec![Some(T + 10), Some(T)]);
    f.commit(
        &[(A, "right", FileMode::Blob), (B, "base", FileMode::Blob)],
        &[left, right],
        T + 40,
    );
    assert_eq!(f.check(&[A, B]), vec![Some(T + 20), Some(T)]);
    let equal_right = f.commit(
        &[(A, "left", FileMode::Blob), (B, "base", FileMode::Blob)],
        &[a],
        T + 50,
    );
    f.commit(
        &[(A, "left", FileMode::Blob), (B, "base", FileMode::Blob)],
        &[left, equal_right],
        T + 60,
    );
    assert_eq!(f.check(&[A, B]), vec![Some(T + 10), Some(T)]);
}

#[test]
fn merge_resolution_and_unrelated_sibling_histories_match_git() {
    let f = Fixture::new();
    let a = f.commit(&[(A, "active", FileMode::Blob)], &[], T);
    let b = f.commit(&[(B, "sibling", FileMode::Blob)], &[], T + 10);
    let merged = f.commit(
        &[
            (A, "active", FileMode::Blob),
            (B, "sibling", FileMode::Blob),
        ],
        &[a, b],
        T + 20,
    );
    assert_eq!(f.check(&[A, B]), vec![Some(T), Some(T + 10)]);
    f.commit(
        &[
            (A, "resolution", FileMode::Blob),
            (B, "sibling", FileMode::Blob),
        ],
        &[merged, b],
        T + 30,
    );
    assert_eq!(f.check(&[A, B]), vec![Some(T + 30), Some(T + 10)]);
}

#[test]
fn backdated_and_same_second_commits_use_ancestry_not_max_timestamp() {
    let f = Fixture::new();
    let a = f.commit(&[(A, "one", FileMode::Blob)], &[], T + 100);
    let b = f.commit(&[(A, "two", FileMode::Blob)], &[a], T);
    f.commit(
        &[(A, "two", FileMode::Blob), (B, "three", FileMode::Blob)],
        &[b],
        T,
    );
    assert_eq!(f.check(&[A, B]), vec![Some(T), Some(T)]);
}

#[test]
fn rename_deletion_readdition_and_mode_changes_match_git() {
    let f = Fixture::new();
    let a = f.commit(&[(A, "same", FileMode::Blob)], &[], T);
    let b = f.commit(&[(A, "same", FileMode::BlobExecutable)], &[a], T + 10);
    assert_eq!(f.check(&[A]), vec![Some(T + 10)]);
    let bundle = "content/00001/index.md";
    let c = f.commit(&[(bundle, "same", FileMode::BlobExecutable)], &[b], T + 20);
    assert_eq!(f.check(&[A, bundle]), vec![Some(T + 20), Some(T + 20)]);
    let d = f.commit(&[], &[c], T + 30);
    assert_eq!(f.check(&[bundle]), vec![Some(T + 30)]);
    f.commit(&[(bundle, "same", FileMode::Blob)], &[d], T + 40);
    assert_eq!(f.check(&[bundle]), vec![Some(T + 40)]);
}

#[test]
fn new_batch_observes_new_materialized_head() {
    let f = Fixture::new();
    let a = f.commit(&[(A, "committed", FileMode::Blob)], &[], T);
    assert_eq!(f.check(&[A]), vec![Some(T)]);
    f.commit(&[(A, "dirty materialized", FileMode::Blob)], &[a], T + 10);
    assert_eq!(f.check(&[A]), vec![Some(T + 10)]);
}

#[test]
fn shallow_boundary_matches_git_even_when_older_objects_are_available() {
    let f = Fixture::new();
    let a = f.commit(&[(A, "old", FileMode::Blob)], &[], T);
    let b = f.commit(
        &[(A, "old", FileMode::Blob), (B, "new", FileMode::Blob)],
        &[a],
        T + 10,
    );
    std::fs::write(f.repo.path().join("shallow"), format!("{b}\n")).unwrap();
    assert_eq!(f.check(&[A, B]), vec![Some(T + 10), Some(T + 10)]);
}

#[test]
fn git_replacement_history_matches_git() {
    let f = Fixture::new();
    let a = f.commit(&[(A, "original", FileMode::Blob)], &[], T);
    let replacement = f.commit(&[(A, "replacement", FileMode::Blob)], &[], T + 10);
    f.repo.set_head_detached(a).unwrap();
    f.repo
        .reference(&format!("refs/replace/{a}"), replacement, true, "fixture")
        .unwrap();
    assert_eq!(f.check(&[A]), vec![Some(T + 10)]);
}

#[test]
fn empty_batch_does_not_open_git() {
    let _history = History::new(Vec::new());
}

#[test]
#[ignore = "explicit real-corpus benchmark requires PHASE7_CORPUS and PHASE7_REPORT"]
fn real_corpus_equivalence_and_benchmark() {
    let root = PathBuf::from(std::env::var_os("PHASE7_CORPUS").expect("PHASE7_CORPUS"));
    let report = PathBuf::from(std::env::var_os("PHASE7_REPORT").expect("PHASE7_REPORT"));
    let mut paths = std::fs::read_dir(root.join("content"))
        .unwrap()
        .map(|entry| {
            let p = entry.unwrap().path();
            if p.is_dir() {
                p.join("index.md")
            } else {
                p
            }
        })
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|e| e == "md")
                && p.file_name().unwrap() != "_index.md"
        })
        .collect::<Vec<_>>();
    paths.sort();
    let run_before = || paths.iter().map(|p| oracle(p)).collect::<Vec<_>>();
    let run_after = || {
        let mut h = History::new(paths.iter().cloned());
        paths
            .iter()
            .map(|p| h.timestamp(p).unwrap())
            .collect::<Vec<_>>()
    };
    let expected = run_before();
    assert_eq!(run_after(), expected);
    let mut before = Vec::new();
    let mut after = Vec::new();
    for i in 0..5 {
        for optimized in if i % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let start = Instant::now();
            let actual = if optimized { run_after() } else { run_before() };
            let elapsed = start.elapsed().as_secs_f64();
            assert_eq!(actual, expected);
            if optimized {
                after.push(elapsed);
            } else {
                before.push(elapsed);
            }
        }
    }
    let mut subset_measurements = Vec::new();
    for size in [1, 8, 64, 128] {
        let subset = &paths[..size];
        let expected = subset.iter().map(|p| oracle(p)).collect::<Vec<_>>();
        let mut old = Vec::new();
        let mut new = Vec::new();
        for i in 0..6 {
            for optimized in if i % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let start = Instant::now();
                let actual = if optimized {
                    let mut h = History::new(subset.iter().cloned());
                    subset
                        .iter()
                        .map(|p| h.timestamp(p).unwrap())
                        .collect::<Vec<_>>()
                } else {
                    subset.iter().map(|p| oracle(p)).collect::<Vec<_>>()
                };
                let elapsed = start.elapsed().as_secs_f64();
                assert_eq!(actual, expected);
                if i > 0 {
                    if optimized {
                        new.push(elapsed);
                    } else {
                        old.push(elapsed);
                    }
                }
            }
        }
        subset_measurements
            .push(serde_json::json!({"paths": size, "before_seconds": old, "after_seconds": new}));
    }
    let timestamps = paths
        .iter()
        .zip(expected)
        .map(|(path, t)| {
            (
                path.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                t,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let data = serde_json::json!({"status":"passed", "corpus":root, "head":Repository::open(&root).unwrap().head().unwrap().target().unwrap().to_string(), "paths":paths.len(), "warmups":1, "repetitions":5, "conditions":"same process and corpus; alternating before/after order; warm cache; oracle and production batch both timed", "before_seconds":before, "after_seconds":after, "before_git_processes":paths.len(), "after_git_processes":2, "timestamps":timestamps, "subsets":subset_measurements});
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report)
        .unwrap();
    use std::io::Write;
    writeln!(output, "{}", serde_json::to_string_pretty(&data).unwrap()).unwrap();
    println!("{} paths: before {before:?}; after {after:?}", paths.len());
}

#[test]
fn subtree_equal_second_parent_does_not_override_per_file_first_parent() {
    let f = Fixture::new();
    let root = f.commit(
        &[(A, "base", FileMode::Blob), (B, "base", FileMode::Blob)],
        &[],
        T,
    );
    let first = f.commit(
        &[(A, "first", FileMode::Blob), (B, "equal", FileMode::Blob)],
        &[root],
        T + 10,
    );
    let second = f.commit(
        &[(A, "second", FileMode::Blob), (B, "equal", FileMode::Blob)],
        &[root],
        T + 20,
    );
    f.commit(
        &[(A, "second", FileMode::Blob), (B, "equal", FileMode::Blob)],
        &[first, second],
        T + 30,
    );
    assert_eq!(f.check(&[A, B]), vec![Some(T + 20), Some(T + 10)]);
}

#[test]
fn unrelated_merge_with_skipped_parents_retains_path_history() {
    let f = Fixture::new();
    let root = f.commit(&[(A, "base", FileMode::Blob)], &[], T);
    let first = f.commit(
        &[
            (A, "base", FileMode::Blob),
            ("outside", "first", FileMode::Blob),
        ],
        &[root],
        T + 10,
    );
    let second = f.commit(
        &[
            (A, "base", FileMode::Blob),
            ("outside", "second", FileMode::Blob),
        ],
        &[root],
        T + 20,
    );
    f.commit(
        &[
            (A, "base", FileMode::Blob),
            ("outside", "resolution", FileMode::Blob),
        ],
        &[first, second],
        T + 30,
    );
    assert_eq!(f.check(&[A, "content/never.md"]), vec![Some(T), None]);
}

#[test]
fn deterministic_branch_and_merge_graph_matches_git_for_every_path() {
    let f = Fixture::new();
    let names = [
        "content/00001.md",
        "content/00002.md",
        "content/00003/index.md",
        "content/00004.md",
    ];
    let mut ids = Vec::new();
    let mut states: Vec<[u8; 4]> = Vec::new();
    let mut seed = 13_u64;
    for i in 0..48 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let first = if i == 0 { 0 } else { seed as usize % i };
        let mut state = if i == 0 { [1; 4] } else { states[first] };
        state[(seed >> 12) as usize % 4] = ((seed >> 24) % 4) as u8;
        let mut parents = if i == 0 { vec![] } else { vec![ids[first]] };
        if i > 2 && i % 3 == 0 {
            let second = (seed >> 32) as usize % i;
            if second != first {
                parents.push(ids[second]);
            }
        }
        let bodies = state
            .iter()
            .map(|n| format!("value-{n}"))
            .collect::<Vec<_>>();
        let files = names
            .iter()
            .enumerate()
            .filter(|(j, _)| state[*j] != 0)
            .map(|(j, name)| (*name, bodies[j].as_str(), FileMode::Blob))
            .collect::<Vec<_>>();
        ids.push(f.commit(&files, &parents, T + ((seed >> 40) % 100) as i64));
        states.push(state);
        // Ensure the old CLI can run from each parent even for absent paths.
        for name in &names {
            std::fs::create_dir_all(f.root.join(name).parent().unwrap()).unwrap();
        }
        f.check(&names);
    }
}

#[test]
fn configured_rename_following_remains_gits_own_query() {
    let f = Fixture::new();
    let a = f.commit(&[(A, "unchanged", FileMode::Blob)], &[], T);
    let bundle = "content/00001/index.md";
    f.commit(&[(bundle, "unchanged", FileMode::Blob)], &[a], T + 10);
    f.repo
        .config()
        .unwrap()
        .set_bool("log.follow", true)
        .unwrap();
    let path = f.root.join(bundle);
    let mut history = History::new(vec![path.clone(); 128]);
    assert!(configured_history(&path).unwrap());
    assert_eq!(history.timestamp(&path).unwrap(), oracle(&path));
}

#[test]
fn signature_output_configuration_is_not_silently_disabled() {
    let f = Fixture::new();
    f.commit(&[(A, "unsigned", FileMode::Blob)], &[], T);
    f.repo
        .config()
        .unwrap()
        .set_bool("log.showSignature", true)
        .unwrap();
    let path = f.root.join(A);
    assert!(configured_history(&path).unwrap());
    let mut history = History::new(vec![path.clone(); 128]);
    assert_eq!(history.timestamp(&path).unwrap(), oracle(&path));
}

#[test]
fn empty_first_parent_is_not_pruned_when_an_absent_path_has_sibling_history() {
    let f = Fixture::new();
    let empty = f.commit(&[], &[], T);
    let sibling = f.commit(
        &[
            (A, "sibling", FileMode::Blob),
            (B, "sibling", FileMode::Blob),
        ],
        &[],
        T + 10,
    );
    f.commit(&[(B, "sibling", FileMode::Blob)], &[empty, sibling], T + 20);
    // A still exists in the working directory, but is absent from the merge.
    // The first equal parent has NO history for A, so the caller must retain
    // its filesystem fallback instead of treating this as a sibling deletion.
    assert_eq!(f.check(&[A, B]), vec![None, Some(T + 10)]);
    let path = f.root.join(A);
    let mut history = History::new(vec![path.clone(); 128]);
    assert_eq!(history.timestamp(&path).unwrap(), None);
}

#[test]
fn empty_root_batch_preserves_no_history_for_all_uncommitted_paths() {
    let f = Fixture::new();
    f.commit(&[], &[], T);
    std::fs::create_dir_all(f.root.join("content")).unwrap();
    std::fs::write(f.root.join(A), "uncommitted").unwrap();
    assert_eq!(f.check(&[A, B]), vec![None, None]);
}
