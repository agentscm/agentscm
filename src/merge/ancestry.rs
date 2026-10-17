use std::collections::{BTreeMap, BTreeSet};

use crate::model::Revision;
use crate::store::{SourceRead, validate_id};
use crate::{Error, Result};

const MAX_ANCESTRY_REVISIONS: usize = 100_000;
const MAX_ANCESTRY_EDGES: usize = MAX_ANCESTRY_REVISIONS * 8;
const MAX_ANCESTRY_WORK: usize = MAX_ANCESTRY_EDGES * 8 + MAX_ANCESTRY_REVISIONS * 2 + 2;

const REACHABLE_FROM_LEFT: u8 = 1;
const REACHABLE_FROM_RIGHT: u8 = 2;
const REACHABLE_FROM_BOTH: u8 = REACHABLE_FROM_LEFT | REACHABLE_FROM_RIGHT;

pub(crate) struct Ancestry {
    pub bases: Vec<String>,
}

struct Node {
    revision: Revision,
    reachable_from: u8,
}

pub(crate) fn inspect<R: SourceRead + ?Sized>(
    source: &R,
    left: &str,
    right: &str,
) -> Result<Ancestry> {
    validate_id(left)?;
    validate_id(right)?;

    let mut pending = BTreeMap::new();
    pending.insert(left.to_owned(), REACHABLE_FROM_LEFT);
    *pending.entry(right.to_owned()).or_insert(0) |= REACHABLE_FROM_RIGHT;

    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    let mut edge_count = 0usize;
    let mut work = 0usize;
    while let Some((id, reachable_from)) = pending.pop_first() {
        charge_work(&mut work, 1)?;
        let (new_reachability, parents) = match nodes.get_mut(&id) {
            Some(node) => {
                let new_reachability = reachable_from & !node.reachable_from;
                node.reachable_from |= new_reachability;
                (new_reachability, node.revision.parents.clone())
            }
            None => {
                if nodes.len() == MAX_ANCESTRY_REVISIONS {
                    return Err(Error::Limit(format!(
                        "ancestry exceeds {MAX_ANCESTRY_REVISIONS} revisions"
                    )));
                }
                // Every identifier is validated before it reaches the source
                // boundary, including identifiers returned by test adapters.
                validate_id(&id)?;
                let revision = source.load_revision(&id)?;
                edge_count = edge_count
                    .checked_add(revision.parents.len())
                    .ok_or_else(|| Error::Limit("ancestry edge count overflow".into()))?;
                if edge_count > MAX_ANCESTRY_EDGES {
                    return Err(Error::Limit(format!(
                        "ancestry exceeds {MAX_ANCESTRY_EDGES} parent edges"
                    )));
                }
                for parent in &revision.parents {
                    validate_id(parent)?;
                }
                let parents = revision.parents.clone();
                nodes.insert(
                    id,
                    Node {
                        revision,
                        reachable_from,
                    },
                );
                (reachable_from, parents)
            }
        };
        if new_reachability == 0 {
            continue;
        }
        charge_work(&mut work, parents.len())?;
        for parent in parents {
            *pending.entry(parent).or_insert(0) |= new_reachability;
        }
    }

    reject_cycles(&nodes, &mut work)?;

    charge_work(&mut work, nodes.len())?;
    let common: BTreeSet<String> = nodes
        .iter()
        .filter(|(_, node)| node.reachable_from == REACHABLE_FROM_BOTH)
        .map(|(id, _)| id.clone())
        .collect();
    let mut dominated = BTreeSet::new();
    for id in &common {
        charge_work(&mut work, 1)?;
        let node = nodes
            .get(id)
            .expect("common revision was loaded during traversal");
        charge_work(&mut work, node.revision.parents.len())?;
        for parent in &node.revision.parents {
            if common.contains(parent) {
                dominated.insert(parent.clone());
            }
        }
    }
    // The common set is parent-closed. Therefore every common ancestor
    // dominated by another common node is either that node's direct parent or
    // has a common child on the path and is marked through that child.
    let bases = common.difference(&dominated).cloned().collect();

    Ok(Ancestry { bases })
}

fn reject_cycles(nodes: &BTreeMap<String, Node>, work: &mut usize) -> Result<()> {
    charge_work(work, nodes.len())?;
    let mut child_counts: BTreeMap<String, usize> =
        nodes.keys().map(|id| (id.clone(), 0)).collect();
    for node in nodes.values() {
        charge_work(work, node.revision.parents.len())?;
        for parent in &node.revision.parents {
            let count = child_counts
                .get_mut(parent)
                .ok_or_else(|| Error::Invalid("ancestry closure is incomplete".into()))?;
            *count = count
                .checked_add(1)
                .ok_or_else(|| Error::Limit("ancestry child count overflow".into()))?;
        }
    }

    charge_work(work, child_counts.len())?;
    let mut tips: BTreeSet<String> = child_counts
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut removed = 0usize;
    while let Some(id) = tips.pop_first() {
        charge_work(work, 1)?;
        removed += 1;
        let node = nodes
            .get(&id)
            .expect("cycle frontier contains only loaded revisions");
        charge_work(work, node.revision.parents.len())?;
        for parent in &node.revision.parents {
            let count = child_counts
                .get_mut(parent)
                .expect("complete ancestry closure contains every parent");
            *count = count
                .checked_sub(1)
                .ok_or_else(|| Error::Invalid("ancestry child count underflow".into()))?;
            if *count == 0 {
                tips.insert(parent.clone());
            }
        }
    }
    if removed != nodes.len() {
        return Err(Error::Invalid("revision ancestry contains a cycle".into()));
    }
    Ok(())
}

fn charge_work(work: &mut usize, amount: usize) -> Result<()> {
    *work = work
        .checked_add(amount)
        .ok_or_else(|| Error::Limit("ancestry work count overflow".into()))?;
    if *work > MAX_ANCESTRY_WORK {
        return Err(Error::Limit(format!(
            "ancestry exceeds {MAX_ANCESTRY_WORK} work units"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use tempfile::TempDir;

    use super::*;
    use crate::model::{DiffResult, FileChange};
    use crate::store::{DiffPage, Engine, FilePage};

    fn put(path: &str, content: &[u8]) -> FileChange {
        FileChange {
            path: path.to_owned(),
            content: Some(content.to_vec()),
        }
    }

    fn fixture() -> (TempDir, Engine) {
        let directory = TempDir::new().expect("temporary directory");
        let engine = Engine::open(directory.path()).expect("engine");
        (directory, engine)
    }

    #[test]
    fn real_engine_finds_bases_for_equal_and_ancestor_heads() {
        let (_directory, engine) = fixture();
        let base = engine
            .checkpoint(None, &[put("base", b"base")])
            .expect("base");
        let child = engine
            .checkpoint(Some(&base), &[put("child", b"child")])
            .expect("child");

        let equal = inspect(&engine, &base, &base).expect("equal ancestry");
        assert_eq!(equal.bases, vec![base.clone()]);

        let forward = inspect(&engine, &base, &child).expect("forward ancestry");
        assert_eq!(forward.bases, vec![base.clone()]);

        let reverse = inspect(&engine, &child, &base).expect("reverse ancestry");
        assert_eq!(reverse.bases, vec![base]);
    }

    #[test]
    fn real_engine_finds_the_best_base_across_a_diamond() {
        let (_directory, engine) = fixture();
        let base = engine
            .checkpoint(None, &[put("base", b"base")])
            .expect("base");
        let left = engine
            .checkpoint(Some(&base), &[put("left", b"left")])
            .expect("left");
        let right = engine
            .checkpoint(Some(&base), &[put("right", b"right")])
            .expect("right");
        let join = engine
            .checkpoint_with_parents(&[left.clone(), right], &[])
            .expect("join");
        let side = engine
            .checkpoint(Some(&left), &[put("side", b"side")])
            .expect("side");

        let ancestry = inspect(&engine, &join, &side).expect("diamond ancestry");
        assert_eq!(ancestry.bases, vec![left]);
    }

    #[test]
    fn real_engine_returns_every_criss_cross_base_in_id_order() {
        let (_directory, engine) = fixture();
        let base = engine
            .checkpoint(None, &[put("base", b"base")])
            .expect("base");
        let first = engine
            .checkpoint(Some(&base), &[put("first", b"first")])
            .expect("first");
        let second = engine
            .checkpoint(Some(&base), &[put("second", b"second")])
            .expect("second");
        let left = engine
            .checkpoint_with_parents(&[first.clone(), second.clone()], &[])
            .expect("left merge");
        let right = engine
            .checkpoint_with_parents(&[second.clone(), first.clone()], &[])
            .expect("right merge");

        let ancestry = inspect(&engine, &left, &right).expect("criss-cross ancestry");
        let mut expected = vec![first, second];
        expected.sort();
        assert_eq!(ancestry.bases, expected);
    }

    #[test]
    fn real_engine_reports_unrelated_heads_without_an_invented_base() {
        let (_directory, engine) = fixture();
        let left = engine
            .checkpoint(None, &[put("root", b"left")])
            .expect("left root");
        let right = engine
            .checkpoint(None, &[put("root", b"right")])
            .expect("right root");

        let ancestry = inspect(&engine, &left, &right).expect("unrelated ancestry");
        assert!(ancestry.bases.is_empty());
    }

    #[test]
    fn real_engine_rejects_invalid_and_missing_ids() {
        let (_directory, engine) = fixture();
        let existing = engine
            .checkpoint(None, &[put("root", b"root")])
            .expect("existing root");

        assert!(matches!(
            inspect(&engine, "not-an-id", &existing),
            Err(Error::Invalid(_))
        ));
        let missing = "f".repeat(64);
        assert!(matches!(
            inspect(&engine, &existing, &missing),
            Err(error) if error.missing_object_id() == Some(missing.as_str())
        ));
    }

    struct CyclicSource {
        revisions: BTreeMap<String, Revision>,
        reads: RefCell<BTreeMap<String, usize>>,
    }

    impl SourceRead for CyclicSource {
        fn read_encoded(&self, _id: &str) -> Result<Vec<u8>> {
            unreachable!("ancestry reads decoded revisions only")
        }

        fn load_revision(&self, id: &str) -> Result<Revision> {
            *self.reads.borrow_mut().entry(id.to_owned()).or_insert(0) += 1;
            self.revisions
                .get(id)
                .cloned()
                .ok_or_else(|| Error::MissingObject(id.to_owned()))
        }

        fn read_file(&self, _revision: &str, _path: &str) -> Result<Vec<u8>> {
            unreachable!("ancestry does not read files")
        }

        fn list_files(
            &self,
            _revision_id: &str,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<FilePage> {
            unreachable!("ancestry does not list files")
        }

        fn diff(&self, _left: &str, _right: &str, _limit: usize) -> Result<DiffResult> {
            unreachable!("ancestry does not diff files")
        }

        fn diff_page(
            &self,
            _left_id: &str,
            _right_id: &str,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<DiffPage> {
            unreachable!("ancestry does not page diffs")
        }
    }

    #[test]
    fn controlled_mock_rejects_an_impossible_cryptographic_cycle() {
        let first = format!("{:064x}", 1);
        let second = format!("{:064x}", 2);
        let tree = format!("{:064x}", 3);
        let source = CyclicSource {
            revisions: BTreeMap::from([
                (
                    first.clone(),
                    Revision {
                        version: 1,
                        parents: vec![second.clone()],
                        tree: tree.clone(),
                    },
                ),
                (
                    second.clone(),
                    Revision {
                        version: 1,
                        parents: vec![first.clone()],
                        tree,
                    },
                ),
            ]),
            reads: RefCell::new(BTreeMap::new()),
        };

        assert!(matches!(
            inspect(&source, &first, &second),
            Err(Error::Invalid(message)) if message.contains("cycle")
        ));
        assert!(source.reads.borrow().values().all(|reads| *reads == 1));
    }
}
