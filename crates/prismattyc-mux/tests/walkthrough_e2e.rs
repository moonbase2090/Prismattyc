//! Catalog walk with scripted facts (PT-198).

use std::time::UNIX_EPOCH;

use prismattyc_mux::walkthrough::{
    bundled_catalog, detect_step, load_progress, reset_progress, resume_index, save_progress,
    scripted_fact, Cursor, DetectOutcome,
};

#[test]
fn scripted_facts_finish_the_catalog_and_progress_round_trips() {
    let catalog = bundled_catalog().expect("catalog");
    let mut cursor = Cursor::resume(catalog.clone(), None, None).expect("start");
    loop {
        let step = cursor.current_step().expect("step");
        let fact = scripted_fact(&step.expect);
        assert_eq!(
            detect_step(&step.expect, &fact),
            DetectOutcome::Advance,
            "step {}",
            step.id
        );
        if !cursor.complete() {
            break;
        }
    }
    let saved = cursor.snapshot(UNIX_EPOCH).expect("progress");
    assert!(resume_index(&catalog, &saved).is_none());
    let dir = std::env::temp_dir().join(format!(
        "pt-198-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("walkthrough.json");
    save_progress(&path, &saved).unwrap();
    let loaded = load_progress(&path).expect("load");
    assert_eq!(loaded, saved);
    assert!(resume_index(&catalog, &loaded).is_none());
    reset_progress(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
