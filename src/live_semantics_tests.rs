//! Focused accepted-generation tests. The process/HTTP acceptance is separate.
use super::*;
use git2::IndexAddOption;

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    live: PathBuf,
    generation: git::SourceGeneration,
    plan: Option<OnlyRenderPlan>,
}

fn proposal(number: u32, title: &str, body: &str) -> String {
    format!("---\neip: {number}\ntitle: {title}\nstatus: Draft\ntype: Standards Track\ncategory: Core\nauthor: Initial Author\ncreated: 2020-01-01\n---\n\n{body}\n")
}

fn meta(member: u32) -> String {
    format!("---\neip: 7773\ntitle: Upgrade\nstatus: Draft\ntype: Meta\ncreated: 2025-01-01\n---\n\n### Included EIPs\n\n* [EIP-{member}](./{member:05}.md)\n")
}

impl Fixture {
    fn new(targeted: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let live = temp.path().join("live");
        std::fs::create_dir_all(source.join("content")).unwrap();
        fixture::complete_sources(&source);
        std::fs::write(
            source.join("content/00001.md"),
            proposal(1, "One", "Original"),
        )
        .unwrap();
        std::fs::write(
            source.join("content/00002.md"),
            proposal(2, "Two", "Unrelated"),
        )
        .unwrap();
        std::fs::write(source.join("content/07773.md"), meta(1)).unwrap();
        let repo = Repository::init(&source).unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let head = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        Repository::clone(source.to_str().unwrap(), &live).unwrap();
        let plan = targeted.then(|| {
            OnlyRenderPlan::build(
                &live.join("content"),
                BTreeSet::from([ProposalNumber::from_u32(1).unwrap()]),
            )
            .unwrap()
        });
        fixture::prepare(&live, plan.as_ref());
        Self {
            _temp: temp,
            source,
            live,
            generation: git::SourceGeneration {
                source_head: head,
                materialized_head: head,
            },
            plan,
        }
    }

    fn state(&self) -> LiveSource {
        LiveSource::new(
            &self.source,
            &self.live,
            self.plan.as_ref(),
            self.generation,
            Default::default(),
            "EIPs",
        )
        .unwrap()
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.source.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn stage(&self, relative: &str) {
        let repo = Repository::open(&self.source).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(relative)).unwrap();
        index.write().unwrap();
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.live.join(relative)).unwrap()
    }
    fn stamp(&self, relative: &str) -> (Vec<u8>, std::time::SystemTime) {
        let path = self.live.join(relative);
        (
            std::fs::read(&path).unwrap(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        )
    }
}

#[cfg(windows)]
#[test]
fn private_live_clone_enables_long_paths_without_changing_source_config() {
    let fixture = Fixture::new(false);
    let source_config = std::fs::read(fixture.source.join(".git/config")).unwrap();
    let live_config = std::fs::read(fixture.live.join(".git/config")).unwrap();
    let state = fixture.state();
    let staged = Repository::open(&state.stage).unwrap();
    assert!(staged.config().unwrap().get_bool("core.longpaths").unwrap());
    assert_eq!(
        std::fs::read(fixture.source.join(".git/config")).unwrap(),
        source_config
    );
    assert_eq!(
        std::fs::read(fixture.live.join(".git/config")).unwrap(),
        live_config
    );
}

#[test]
fn body_only_is_one_write_without_aggregate_or_unrelated_churn() {
    let fixture = Fixture::new(false);
    let mut state = fixture.state();
    let other = fixture.stamp("content/00002.md");
    let metadata = fixture.stamp(DERIVED_PATHS[0]);
    let hardforks = fixture.stamp(DERIVED_PATHS[1]);
    assert_eq!(state.synchronize().unwrap(), 0);
    fixture.write("content/00001.md", &proposal(1, "One", "Body changed"));
    assert_eq!(state.synchronize().unwrap(), 1);
    assert!(fixture.read("content/00001.md").contains("Body changed"));
    assert_eq!(fixture.stamp("content/00002.md"), other);
    assert_eq!(fixture.stamp(DERIVED_PATHS[0]), metadata);
    assert_eq!(fixture.stamp(DERIVED_PATHS[1]), hardforks);
    assert_eq!(state.synchronize().unwrap(), 0);
}

#[test]
fn metadata_and_author_created_fields_advance_one_complete_semantic_generation() {
    let fixture = Fixture::new(false);
    let mut state = fixture.state();
    let other = fixture.stamp("content/00002.md");
    let changed = proposal(1, "Fresh title", "Original")
        .replace("status: Draft", "status: Review")
        .replace("Initial Author", "New Author (@fresh)")
        .replace("2020-01-01", "2026-09-23");
    fixture.write("content/00001.md", &changed);
    assert_eq!(state.synchronize().unwrap(), 3);
    let record = &state.accepted_catalog.records()["eip-1"];
    assert_eq!(record.title, "Fresh title");
    assert_eq!(record.status, "Review");
    assert_eq!(record.authors[0].name, "New Author");
    assert_eq!(record.created.as_ref().unwrap().to_string(), "2026-09-23");
    assert!(fixture.read(DERIVED_PATHS[0]).contains("Fresh title"));
    assert!(fixture.read(DERIVED_PATHS[1]).contains("Fresh title"));
    assert_eq!(fixture.stamp("content/00002.md"), other);
}

#[test]
fn targeted_membership_and_marker_changes_refresh_selected_badges_without_leaking_sources() {
    let fixture = Fixture::new(true);
    let mut state = fixture.state();
    assert!(fixture.read("content/00001.md").contains("Glamsterdam"));
    fixture.write("content/07773.md", &meta(2));
    assert_eq!(state.synchronize().unwrap(), 2);
    assert!(!fixture
        .read("content/00001.md")
        .contains("network_upgrades"));
    assert!(!fixture.live.join("content/00002.md").exists());
    assert!(!fixture.live.join("content/07773.md").exists());
    fixture.write(
        "content/07773.md",
        &meta(1).replace("type: Meta\n", "type: Meta\nnetwork-upgrade: Fresh Fork\n"),
    );
    state.synchronize().unwrap();
    assert!(fixture.read("content/00001.md").contains("Fresh Fork"));
    assert!(fixture.read(DERIVED_PATHS[1]).contains("Fresh Fork"));
    assert_eq!(state.synchronize().unwrap(), 0);
}

#[test]
fn startup_gap_reconciles_against_initialized_catalog_index_and_projections() {
    let fixture = Fixture::new(true);
    let other = fixture.stamp(DERIVED_PATHS[0]);
    fixture.write(
        "content/07773.md",
        &meta(1).replace(
            "type: Meta\n",
            "type: Meta\nnetwork-upgrade: Startup Fork\n",
        ),
    );
    let mut state = fixture.state();
    assert!(fixture.read("content/00001.md").contains("Startup Fork"));
    assert!(fixture.read(DERIVED_PATHS[1]).contains("Startup Fork"));
    assert_eq!(fixture.stamp(DERIVED_PATHS[0]), other);
    assert_eq!(state.synchronize().unwrap(), 0);
}

#[test]
fn rejected_semantics_preserve_accepted_input_and_recover() {
    let fixture = Fixture::new(false);
    let mut state = fixture.state();
    let original = fixture.stamp("content/00001.md");
    let metadata = fixture.stamp(DERIVED_PATHS[0]);
    let accepted = state.last_good_tree;
    fixture.write(
        "content/00001.md",
        &proposal(1, "One", "Invalid").replace("status: Draft\n", ""),
    );
    assert!(state.synchronize().is_err());
    assert_eq!(state.last_good_tree, accepted);
    assert_eq!(fixture.stamp("content/00001.md"), original);
    assert_eq!(fixture.stamp(DERIVED_PATHS[0]), metadata);
    fixture.write("content/00001.md", &proposal(1, "Recovered", "Valid"));
    state.synchronize().unwrap();
    assert_eq!(state.accepted_catalog.records()["eip-1"].title, "Recovered");
}

#[test]
fn add_delete_reconsiders_links_without_republishing_existing_pages() {
    let fixture = Fixture::new(false);
    let mut state = fixture.state();
    fixture.write("content/00001.md", &proposal(1, "One", "Already dirty"));
    state.synchronize().unwrap();
    let one = fixture.stamp("content/00001.md");
    let two = fixture.stamp("content/00002.md");
    // A later synthetic generation must not change a previously dirty page's
    // history-derived updated field just because links are reconsidered.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fixture.write("content/00003.md", &proposal(3, "Three", "Added"));
    fixture.stage("content/00003.md");
    assert_eq!(state.synchronize().unwrap(), 2);
    assert_eq!(fixture.stamp("content/00001.md"), one);
    assert_eq!(fixture.stamp("content/00002.md"), two);
    std::fs::remove_file(fixture.source.join("content/00003.md")).unwrap();
    assert_eq!(state.synchronize().unwrap(), 2);
    assert!(!fixture.live.join("content/00003.md").exists());
    assert_eq!(fixture.stamp("content/00001.md"), one);
    assert_eq!(fixture.stamp("content/00002.md"), two);
}

#[cfg(unix)]
#[test]
fn publication_failure_does_not_accept_semantics_and_reversion_repairs_partial_output() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(false);
    let mut state = fixture.state();
    let page = fixture.read("content/00001.md");
    let hardforks = fixture.read(DERIVED_PATHS[1]);
    let metadata = fixture.read(DERIVED_PATHS[0]);
    let accepted = state.last_good_tree;
    let parent = fixture.live.join("static/assets/data");
    let permissions = std::fs::metadata(&parent).unwrap().permissions();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    fixture.write("content/00001.md", &proposal(1, "Not accepted", "Partial"));
    let failed = state.synchronize();
    std::fs::set_permissions(&parent, permissions).unwrap();
    assert!(
        failed.is_err(),
        "fault fixture must fail to publish metadata"
    );
    assert!(state.needs_publication_retry());
    assert_eq!(state.last_good_tree, accepted);
    assert!(
        fixture.read("content/00001.md").contains("Not accepted"),
        "exercise a genuinely partial publication"
    );
    fixture.write("content/00001.md", &proposal(1, "One", "Original"));
    state.synchronize().unwrap();
    assert!(!state.needs_publication_retry());
    assert_eq!(state.last_good_tree, accepted);
    assert_eq!(fixture.read("content/00001.md"), page);
    assert_eq!(fixture.read(DERIVED_PATHS[1]), hardforks);
    assert_eq!(fixture.read(DERIVED_PATHS[0]), metadata);
}

#[cfg(unix)]
#[test]
fn partial_native_asset_publication_preserves_render_intent_through_source_reversion() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(false);
    let mut state = fixture.state();
    let accepted = state.last_good_tree;
    let config =
        crate::layout::theme_config_path(&crate::layout::mounted_theme_path(&fixture.live));
    let initial_config = std::fs::read(&config).unwrap();
    let asset = "content/phase5-asset.txt";
    let later = "static/phase5-later.txt";
    fixture.write(asset, "native render input");
    fixture.stage(asset);
    fixture.write(later, "later failing input");
    fixture.stage(later);
    let destination = fixture.live.join("static");
    let permissions = std::fs::metadata(&destination).unwrap().permissions();
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = state.synchronize();
    std::fs::set_permissions(&destination, permissions).unwrap();
    assert!(result.is_err());
    assert!(fixture.live.join(asset).is_file());
    assert!(!fixture.live.join(later).exists());
    assert!(state.render_pending);
    assert_eq!(state.last_good_tree, accepted);
    std::fs::remove_file(fixture.source.join(asset)).unwrap();
    std::fs::remove_file(fixture.source.join(later)).unwrap();
    state.synchronize().unwrap();
    assert!(!state.needs_publication_retry());
    assert_eq!(state.last_good_tree, accepted);
    assert!(!fixture.live.join(asset).exists());
    assert_ne!(std::fs::read(config).unwrap(), initial_config);
}

#[cfg(unix)]
#[test]
fn partial_static_output_invalidation_requires_a_render_even_when_source_reverts() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(false);
    let mut state = fixture.state();
    let a = "static/phase5-output-a/item.txt";
    let z = "static/phase5-output-z/item.txt";
    for path in [a, z] {
        fixture.write(path, "accepted");
        fixture.stage(path);
    }
    state.synchronize().unwrap();
    let output = crate::layout::serve_output_path(fixture.live.parent().unwrap());
    let output_a = output.join("phase5-output-a/item.txt");
    let output_z = output.join("phase5-output-z/item.txt");
    for path in [&output_a, &output_z] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "accepted").unwrap();
    }
    let config =
        crate::layout::theme_config_path(&crate::layout::mounted_theme_path(&fixture.live));
    let before = std::fs::read(&config).unwrap();
    let permissions = std::fs::metadata(output_z.parent().unwrap())
        .unwrap()
        .permissions();
    std::fs::set_permissions(
        output_z.parent().unwrap(),
        std::fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    for path in [a, z] {
        std::fs::remove_file(fixture.source.join(path)).unwrap();
    }
    let result = state.synchronize();
    std::fs::set_permissions(output_z.parent().unwrap(), permissions).unwrap();
    assert!(result.is_err());
    assert!(
        !output_a.exists(),
        "the earlier output removal must really have happened"
    );
    assert!(output_z.exists());
    assert!(state.needs_publication_retry());
    for path in [a, z] {
        fixture.write(path, "accepted");
    }
    state.synchronize().unwrap();
    assert!(!state.needs_publication_retry());
    assert_ne!(
        std::fs::read(config).unwrap(),
        before,
        "repaired accepted input must request restoration of partially invalidated output"
    );
}
