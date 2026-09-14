use super::*;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("pmux-template-{}", crate::new_space_id().unwrap())))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn template(cwd: &Path) -> TeamTemplate {
    serde_json::from_value(serde_json::json!({
        "version": 1,
        "definition": {
            "version": 2, "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "saved_at_unix": 0,
            "sessions": [{"name":"old", "agent":"old", "windows":[{
                "title":"work", "cols":80, "rows":24,
                "root":{"kind":"leaf", "cwd":cwd, "program":"/bin/sh", "command":"echo ready"}
            }]}],
            "tabs":[{"title":"work", "sessions":["old"]}], "focused_session":"old"
        },
        "metadata": {"roles":{"old":"reviewer", "obsolete":"builder"},
                     "links":{"docs":"https://example.com/docs"}}
    }))
    .unwrap()
}

#[test]
fn saved_templates_round_trip_list_in_order_and_refuse_overwrites() {
    let scratch = Scratch::new();
    assert!(list(&scratch.0).unwrap().is_empty());
    let original = template(&std::env::temp_dir());
    save(&scratch.0, "zulu", &original).unwrap();
    let alpha = save(&scratch.0, "alpha", &original).unwrap();
    std::fs::write(directory(&scratch.0).join("ignore.txt"), "not a template").unwrap();
    assert_eq!(list(&scratch.0).unwrap(), ["alpha", "zulu"]);
    assert_eq!(load(&scratch.0, "alpha").unwrap(), original);
    let bytes = std::fs::read(&alpha).unwrap();
    let changed = TeamTemplate {
        version: 99,
        ..original
    };
    assert!(save(&scratch.0, "alpha", &changed).is_err());
    assert_eq!(std::fs::read(alpha).unwrap(), bytes);
    assert!(save(&scratch.0, "../escape", &changed).is_err());
    assert!(load(&scratch.0, "missing").is_err());
}

#[test]
fn invalid_versions_oversized_definitions_and_broken_json_are_rejected() {
    let scratch = Scratch::new();
    let mut value = template(&std::env::temp_dir());
    value.version = 99;
    save(&scratch.0, "future", &value).unwrap();
    assert!(load(&scratch.0, "future")
        .unwrap_err()
        .to_string()
        .contains("unsupported"));
    value.version = 1;
    value.definition.sessions = vec![value.definition.sessions[0].clone(); 65];
    save(&scratch.0, "oversized", &value).unwrap();
    assert!(load(&scratch.0, "oversized")
        .unwrap_err()
        .to_string()
        .contains("64 sessions"));
    std::fs::write(directory(&scratch.0).join("broken.json"), "{").unwrap();
    assert!(load(&scratch.0, "broken").is_err());
}

#[test]
fn preview_renames_all_references_without_editing_the_template() {
    let source = template(&std::env::temp_dir());
    let before = source.clone();
    let result = preview(&source, "Review Team", &BTreeSet::new()).unwrap();
    assert_eq!(source, before);
    assert_eq!(result.definition.id, None);
    assert_eq!(result.definition.sessions[0].name, "reviewteam-1");
    assert_eq!(
        result.definition.sessions[0].agent.as_deref(),
        Some("reviewteam-1")
    );
    assert_eq!(result.definition.tabs[0].sessions, ["reviewteam-1"]);
    assert_eq!(
        result.definition.focused_session.as_deref(),
        Some("reviewteam-1")
    );
    assert_eq!(
        result.metadata.roles,
        BTreeMap::from([("reviewteam-1".into(), "reviewer".into())])
    );
    assert_eq!(result.metadata.links, before.metadata.links);
    assert!(result.conflicts.is_empty());
    assert_eq!(result.launches.len(), 1);
    assert_eq!(result.launches[0].command.as_deref(), Some("echo ready"));
    let text = result.text();
    assert!(text.contains("Create Review Team with 1 independent sessions"));
    assert!(text.contains("No sessions created or commands executed"));
}

#[test]
fn preview_reports_unavailable_launches_and_refuses_dangling_references() {
    let scratch = Scratch::new();
    let mut source = template(&scratch.0);
    let SavedNode::Leaf { command, .. } = &mut source.definition.sessions[0].windows[0].root else {
        unreachable!()
    };
    *command = Some("echo first\necho second".into());
    let result = preview(&source, "Review", &BTreeSet::from(["review-1".into()])).unwrap();
    assert_eq!(result.conflicts.len(), 3);
    assert!(result
        .conflicts
        .iter()
        .any(|c| c.contains("already exists")));
    assert!(result.conflicts.iter().any(|c| c.contains("unavailable")));
    assert!(result.conflicts.iter().any(|c| c.contains("line break")));
    assert!(
        !scratch.0.exists(),
        "preview must not create the launch directory"
    );
    let mut broken = source.clone();
    broken
        .definition
        .sessions
        .push(broken.definition.sessions[0].clone());
    assert!(preview(&broken, "Review", &BTreeSet::new()).is_err());
    let mut broken = source.clone();
    broken.definition.tabs[0].sessions = vec!["missing".into()];
    assert!(preview(&broken, "Review", &BTreeSet::new()).is_err());
    source.definition.focused_session = Some("missing".into());
    assert!(preview(&source, "Review", &BTreeSet::new()).is_err());
}
