use std::collections::BTreeSet;
use std::fs;

use agentscm::history::{HeadRelation, history_page, relation_page};
use agentscm::model::FileChange;
use agentscm::store::Engine;
use tempfile::TempDir;

fn relation(engine: &Engine, left: &str, right: &str) -> HeadRelation {
    let mut cursor = None;
    for _ in 0..100 {
        let page = relation_page(engine, left, right, cursor.as_deref(), 1).unwrap();
        cursor = page.next;
        if cursor.is_none() {
            return page.relation;
        }
    }
    panic!("relation did not finish");
}

#[test]
fn diamond_history_pages_emit_once_and_relation_uses_all_parents() {
    let temp = TempDir::new().unwrap();
    let engine = Engine::open(temp.path()).unwrap();
    let base = engine.checkpoint(None, &[]).unwrap();
    let left = engine.checkpoint(Some(&base), &[]).unwrap();
    let right = engine
        .checkpoint(
            Some(&base),
            &[FileChange {
                path: "right".into(),
                content: Some(vec![1]),
            }],
        )
        .unwrap();
    let merge = engine
        .checkpoint_with_parents(&[left.clone(), right.clone()], &[])
        .unwrap();
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = history_page(&engine, &merge, cursor.as_deref(), 1).unwrap();
        assert!(page.entries.len() <= 1);
        assert!(page.needed_objects.is_empty());
        ids.extend(page.entries.into_iter().map(|entry| entry.id));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 4);
    assert_eq!(
        ids.iter().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([base.clone(), left.clone(), right.clone(), merge.clone(),])
    );
    assert_eq!(ids[0], merge);
    assert_eq!(relation(&engine, &left, &right), HeadRelation::Diverged);
    assert_eq!(
        relation(&engine, &right, &merge),
        HeadRelation::LeftAncestor
    );
    assert_eq!(
        relation(&engine, &merge, &left),
        HeadRelation::RightAncestor
    );
    assert_eq!(relation(&engine, &base, &base), HeadRelation::Equal);
}

#[test]
fn missing_parent_is_incomplete_and_resume_finishes_after_retrieval() {
    let temp = TempDir::new().unwrap();
    let engine = Engine::open(temp.path()).unwrap();
    let base = engine.checkpoint(None, &[]).unwrap();
    let tip = engine.checkpoint(Some(&base), &[]).unwrap();
    let path = temp
        .path()
        .join("objects")
        .join(&base[..2])
        .join(&base[2..]);
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let page = history_page(&engine, &tip, None, 100).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.needed_objects, vec![base.clone()]);
    let incomplete = relation_page(&engine, &base, &tip, None, 100).unwrap();
    assert_eq!(incomplete.relation, HeadRelation::Incomplete);
    assert_eq!(incomplete.needed_objects, vec![base.clone()]);
    fs::write(path, bytes).unwrap();
    let resumed = history_page(&engine, &tip, page.next.as_deref(), 100).unwrap();
    assert_eq!(resumed.entries[0].id, base);
    assert!(resumed.next.is_none());
    let resolved = relation_page(&engine, &base, &tip, incomplete.next.as_deref(), 100).unwrap();
    assert_eq!(resolved.relation, HeadRelation::LeftAncestor);
    assert!(resolved.next.is_none());
}

#[test]
fn history_rejects_wrong_root_corruption_and_invalid_bounds() {
    let temp = TempDir::new().unwrap();
    let engine = Engine::open(temp.path()).unwrap();
    let base = engine.checkpoint(None, &[]).unwrap();
    let tip = engine.checkpoint(Some(&base), &[]).unwrap();
    let page = history_page(&engine, &tip, None, 1).unwrap();
    let cursor = page.next.unwrap();
    assert!(history_page(&engine, &base, Some(&cursor), 1).is_err());
    assert!(history_page(&engine, &tip, Some(&(cursor + "x")), 1).is_err());
    assert!(history_page(&engine, &tip, None, 0).is_err());
    assert!(history_page(&engine, &tip, None, 1_001).is_err());
    assert!(relation_page(&engine, &tip, &base, Some("bad"), 1).is_err());
}

#[test]
fn queued_shared_parent_is_prioritized_in_single_and_resumed_walks() {
    let temp = TempDir::new().unwrap();
    let engine = Engine::open(temp.path()).unwrap();
    let b = engine.checkpoint(None, &[]).unwrap();
    let c = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "c".into(),
                content: Some(vec![1]),
            }],
        )
        .unwrap();
    let a = engine
        .checkpoint_with_parents(&[b.clone(), c.clone()], &[])
        .unwrap();
    let m = engine
        .checkpoint_with_parents(&[a.clone(), b.clone()], &[])
        .unwrap();
    for limit in [1, 100] {
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = history_page(&engine, &m, cursor.as_deref(), limit).unwrap();
            ids.extend(page.entries.into_iter().map(|entry| entry.id));
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(ids, vec![m.clone(), a.clone(), b.clone(), c.clone()]);
    }
}
