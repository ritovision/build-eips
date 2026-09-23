//! Opt-in measurement entry point, also injected verbatim into the isolated
//! C6.3 baseline by the Phase 7 comparison harness. It invokes the real Markdown
//! pipeline without alternate timestamp logic. Never compiled into production.

#[test]
#[ignore = "explicit byte-equivalence benchmark requires PHASE7_PREPROCESS_ROOT"]
fn preprocess_real_corpus() {
    let root = std::path::PathBuf::from(
        std::env::var_os("PHASE7_PREPROCESS_ROOT").expect("PHASE7_PREPROCESS_ROOT"),
    );
    let start = std::time::Instant::now();
    crate::markdown::preprocess(&root.join("content"), None).unwrap();
    println!(
        "PHASE7_PREPROCESS_SECONDS={:.9}",
        start.elapsed().as_secs_f64()
    );
}
