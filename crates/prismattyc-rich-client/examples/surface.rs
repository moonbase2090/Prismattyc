//! Minimal protocol 0.3 app: public client + protocol only, with fallback.

use std::io::{self, IsTerminal, Write};

use prismattyc_protocol::{
    encode_collection_snapshot, encode_semantic_snapshot, encode_status_snapshot,
    encode_workspace_drop, encode_workspace_snapshot, CollectionItem, SemanticDocument,
    SemanticRole, SemanticSpan, StatusItem, StatusTone, StatusVisual, TreeNode, TreeNodeKind,
    WorkspaceRows, WorkspaceSnapshot,
};
use prismattyc_rich_client::{
    await_surface_session, encode_surface_query, enter_raw_stdin, RichGrant, Session,
    NEGOTIATE_TIMEOUT,
};

const SURFACE_ROWS: WorkspaceRows = WorkspaceRows {
    min: 5,
    preferred: 5,
    max: 5,
};

fn workspace(grant: &mut RichGrant) -> WorkspaceSnapshot {
    grant
        .workspace_snapshot(
            SURFACE_ROWS,
            vec![
                TreeNode {
                    id: 1,
                    parent: 0,
                    kind: TreeNodeKind::Column,
                    min: 1,
                    preferred: 1,
                    fill: 1,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: String::new(),
                },
                TreeNode {
                    id: 2,
                    parent: 1,
                    kind: TreeNodeKind::Text,
                    min: 1,
                    preferred: 1,
                    fill: 0,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: "Ready: 1 task".into(),
                },
            ],
        )
        .expect("workspace capability was checked")
}

fn encode_surface(grant: &mut RichGrant) -> Result<Vec<u8>, prismattyc_protocol::DecodeError> {
    let mut wire = encode_workspace_snapshot(&workspace(grant))?;

    if let Some(status) = grant.status_snapshot(vec![StatusItem {
        node_id: 2,
        tone: StatusTone::Success,
        visual: StatusVisual::Badge,
    }]) {
        wire.extend_from_slice(&encode_status_snapshot(&status)?);
    }
    if let Some(collection) = grant.collection_snapshot(
        "tasks",
        vec![CollectionItem {
            id: 1,
            replaceable: true,
            text: "task ready".into(),
        }],
    ) {
        wire.extend_from_slice(&encode_collection_snapshot(&collection)?);
    }
    if grant.has_semantics() {
        wire.extend_from_slice(&encode_semantic_snapshot(&SemanticDocument {
            surface_generation: grant.generation,
            document_id: "main".into(),
            rev: 1,
            text: "Ready: 1 task".into(),
            spans: vec![SemanticSpan {
                start: 0,
                end: 5,
                role: SemanticRole::Status,
            }],
            selection: None,
        })?);
    }
    Ok(wire)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _raw = if io::stdin().is_terminal() {
        enter_raw_stdin()
    } else {
        None
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(&encode_surface_query()?)?;
    stdout.flush()?;

    let Session::Rich(mut grant) = await_surface_session(NEGOTIATE_TIMEOUT) else {
        writeln!(stdout, "Ready (classic): 1 task")?;
        return Ok(());
    };
    if !grant.has_workspace() {
        writeln!(stdout, "Ready (classic): 1 task")?;
        return Ok(());
    }

    stdout.write_all(&encode_surface(&mut grant)?)?;
    stdout.flush()?;

    // A real app runs its input/state loop here and validates every host APC
    // with RichGrant::decode_event_mut before mutating state.
    stdout.write_all(&encode_workspace_drop(grant.generation)?)?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_protocol::{CapabilityReply, ControlMessage};
    use prismattyc_rich_client::{session_from_message, surface_query};

    #[test]
    fn granted_surface_is_runtime_valid() {
        let reply = CapabilityReply::for_surface_query(surface_query()).unwrap();
        let Session::Rich(mut grant) =
            session_from_message(Some(ControlMessage::CapabilityReply(reply)))
        else {
            panic!("expected rich grant");
        };
        let wire = encode_surface(&mut grant).expect("example surface must encode");
        assert!(!wire.is_empty());
        assert_eq!(grant.scene_rev, 1);
    }
}
