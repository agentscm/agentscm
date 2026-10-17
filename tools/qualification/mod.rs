//! Independent raw-reference/set comparison; native contextual checks stay incomplete.

pub mod index_oracle;
pub mod native_oracle;

use index_oracle::{BuildLimits, BuiltIndex, OracleError};
use native_oracle::{
    NativeObservation, ObjectId, ObservationError, ObservationLimits, SourceProfile,
};
use std::collections::BTreeMap;
use std::mem::size_of;

#[derive(Debug, Eq, PartialEq)]
pub enum ComparisonError {
    Native(ObservationError),
    Index(OracleError),
    ProfileMismatch,
    RootsMismatch,
    DescriptorRootMismatch,
    ObjectCountMismatch,
    SourceBytesMismatch,
    ComparisonBufferLimit,
}

/// Set agreement retains the incomplete native observation; it grants no admission.
#[derive(Debug)]
pub struct IncompleteNativeSetAgreement {
    pub native: NativeObservation,
    pub descriptor_index: BuiltIndex,
    pub revision_index: BuiltIndex,
}

pub fn compare_advertised_source(
    objects: &BTreeMap<ObjectId, Vec<u8>>,
    roots: &[ObjectId],
    advertised_bytes: &[u8],
    native_limits: ObservationLimits,
    index_limits: BuildLimits,
) -> Result<IncompleteNativeSetAgreement, ComparisonError> {
    let advertised =
        index_oracle::decode_portable_identity(advertised_bytes).map_err(ComparisonError::Index)?;
    let mut native = native_oracle::observe_native_closure(objects, roots, native_limits)
        .map_err(ComparisonError::Native)?;
    let profile = match native.source_profile {
        SourceProfile::Profile1 => 1,
        SourceProfile::Profile2 => 2,
        SourceProfile::Profile3 => 3,
    };
    if advertised.profile != profile {
        return Err(ComparisonError::ProfileMismatch);
    }
    if advertised.native_roots != native.requested_roots {
        return Err(ComparisonError::RootsMismatch);
    }

    // These are comparison buffers, separately bounded from each builder's logical budget.
    let mut descriptors =
        comparison_buffer::<(ObjectId, u64)>(native.reachable.len(), index_limits)?;
    descriptors.extend(
        native
            .reachable
            .iter()
            .map(|item| (item.id, item.encoded_len)),
    );
    let descriptor_index = index_oracle::build_descriptor_index(&descriptors, index_limits)
        .map_err(ComparisonError::Index)?;
    drop(descriptors);
    if advertised.descriptor_root_hash != descriptor_index.root.hash {
        return Err(ComparisonError::DescriptorRootMismatch);
    }
    if advertised.object_count != descriptor_index.root.count {
        return Err(ComparisonError::ObjectCountMismatch);
    }
    if Some(advertised.encoded_source_bytes) != descriptor_index.root.size_total {
        return Err(ComparisonError::SourceBytesMismatch);
    }
    let mut revision_ids = comparison_buffer::<ObjectId>(native.revisions.len(), index_limits)?;
    revision_ids.extend(native.revisions.keys().copied());
    let revision_index = index_oracle::build_revision_index(&revision_ids, index_limits)
        .map_err(ComparisonError::Index)?;
    let native_oracle::ObservationCompleteness::Incomplete { pending_checks } =
        &mut native.completeness;
    pending_checks
        .retain(|check| *check != native_oracle::PendingOracleWork::PortableIndexComparison);
    Ok(IncompleteNativeSetAgreement {
        native,
        descriptor_index,
        revision_index,
    })
}

fn comparison_buffer<T>(len: usize, limits: BuildLimits) -> Result<Vec<T>, ComparisonError> {
    let bytes = len
        .checked_mul(size_of::<T>())
        .ok_or(ComparisonError::ComparisonBufferLimit)?;
    if len as u64 > limits.max_entries || bytes as u64 > limits.max_entry_buffer_bytes {
        return Err(ComparisonError::ComparisonBufferLimit);
    }
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| ComparisonError::ComparisonBufferLimit)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn insert(objects: &mut BTreeMap<ObjectId, Vec<u8>>, bytes: Vec<u8>) -> ObjectId {
        let id = Sha256::digest(&bytes).into();
        objects.insert(id, bytes);
        id
    }

    fn empty_native() -> (BTreeMap<ObjectId, Vec<u8>>, ObjectId) {
        let mut objects = BTreeMap::new();
        let mut tree = b"AGSCM-T2\0".to_vec();
        for namespace in 1..=3 {
            let mut branch = b"AGSCM-N1\0".to_vec();
            branch.extend_from_slice(&[namespace, 0, 0]);
            tree.extend_from_slice(&insert(&mut objects, branch));
        }
        let tree_id = insert(&mut objects, tree);
        let mut revision = b"AGSCM-R1\0".to_vec();
        revision.extend_from_slice(&1u32.to_be_bytes());
        revision.push(0);
        revision.extend_from_slice(&tree_id);
        let root = insert(&mut objects, revision);
        (objects, root)
    }

    fn identity(root: ObjectId, index: &BuiltIndex) -> Vec<u8> {
        let mut bytes = b"AGSCM-PC1\0".to_vec();
        bytes.push(1);
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&root);
        bytes.extend_from_slice(&index.root.hash);
        bytes.extend_from_slice(&index.root.count.to_be_bytes());
        bytes.extend_from_slice(&index.root.size_total.unwrap().to_be_bytes());
        bytes
    }

    #[test]
    fn profile3_identity_matches_only_a_closure_with_entry_kinds() {
        let (mut objects, _) = empty_native();
        let blob = insert(&mut objects, {
            let mut bytes = b"AGSCM-B1\0".to_vec();
            bytes.extend_from_slice(&1u64.to_be_bytes());
            bytes.push(b'x');
            bytes
        });
        let path = b"run.sh";
        let mut exact_leaf = b"AGSCM-L2\0".to_vec();
        exact_leaf.push(1);
        exact_leaf.extend_from_slice(&Sha256::digest(path));
        exact_leaf.extend_from_slice(&(path.len() as u16).to_be_bytes());
        exact_leaf.extend_from_slice(path);
        exact_leaf.extend_from_slice(&blob);
        exact_leaf.push(1);
        let exact_leaf = insert(&mut objects, exact_leaf);
        let mut portable_leaf = b"AGSCM-L1\0".to_vec();
        portable_leaf.push(2);
        portable_leaf.extend_from_slice(&Sha256::digest(path));
        portable_leaf.extend_from_slice(&(path.len() as u16).to_be_bytes());
        portable_leaf.extend_from_slice(path);
        portable_leaf.extend_from_slice(&exact_leaf);
        let portable_leaf = insert(&mut objects, portable_leaf);
        let mut roots = Vec::new();
        for (namespace, child) in [(1u8, Some(exact_leaf)), (2, Some(portable_leaf)), (3, None)] {
            let mut branch = b"AGSCM-N1\0".to_vec();
            branch.extend_from_slice(&[namespace, 0, u8::from(child.is_some())]);
            if let Some(child) = child {
                branch.push(0);
                branch.extend_from_slice(&child);
            }
            roots.push(insert(&mut objects, branch));
        }
        let mut tree = b"AGSCM-T2\0".to_vec();
        for root in roots {
            tree.extend_from_slice(&root);
        }
        let tree = insert(&mut objects, tree);
        let mut revision = b"AGSCM-R1\0".to_vec();
        revision.extend_from_slice(&1u32.to_be_bytes());
        revision.push(0);
        revision.extend_from_slice(&tree);
        let root = insert(&mut objects, revision);

        // Keep only the objects reachable from the new root.
        let reachable =
            native_oracle::observe_native_closure(&objects, &[root], ObservationLimits::default())
                .unwrap();
        assert_eq!(reachable.source_profile, SourceProfile::Profile3);
        let descriptors: Vec<_> = reachable
            .reachable
            .iter()
            .map(|item| (item.id, item.encoded_len))
            .collect();
        let index =
            index_oracle::build_descriptor_index(&descriptors, BuildLimits::default()).unwrap();
        let mut advertised = identity(root, &index);
        advertised[10] = 3;
        compare_advertised_source(
            &objects,
            &[root],
            &advertised,
            ObservationLimits::default(),
            BuildLimits::default(),
        )
        .unwrap();
        advertised[10] = 2;
        assert!(matches!(
            compare_advertised_source(
                &objects,
                &[root],
                &advertised,
                ObservationLimits::default(),
                BuildLimits::default(),
            ),
            Err(ComparisonError::ProfileMismatch)
        ));
    }

    #[test]
    fn matching_set_retains_incomplete_native_verdict() {
        let (objects, root) = empty_native();
        let descriptors: Vec<_> = objects
            .iter()
            .map(|(id, bytes)| (*id, bytes.len() as u64))
            .collect();
        let index =
            index_oracle::build_descriptor_index(&descriptors, BuildLimits::default()).unwrap();
        let observed = compare_advertised_source(
            &objects,
            &[root],
            &identity(root, &index),
            ObservationLimits::default(),
            BuildLimits::default(),
        )
        .unwrap();
        assert_eq!(observed.descriptor_index.root.count, 5);
        assert_eq!(observed.descriptor_index.root.size_total, Some(187));
        assert_eq!(observed.revision_index.root.count, 1);
        let native_oracle::ObservationCompleteness::Incomplete { pending_checks } =
            observed.native.completeness;
        assert_eq!(
            pending_checks,
            vec![
                native_oracle::PendingOracleWork::NativeWireOwnerConfirmation,
                native_oracle::PendingOracleWork::ContextualTriePlacement,
                native_oracle::PendingOracleWork::CrossIndexEquality,
                native_oracle::PendingOracleWork::DerivedPrefixCounts,
            ]
        );
    }

    #[test]
    fn merkle_consistent_revision_only_set_fails_raw_closure_comparison() {
        let (objects, root) = empty_native();
        let index = index_oracle::build_descriptor_index(
            &[(root, objects[&root].len() as u64)],
            BuildLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            compare_advertised_source(
                &objects,
                &[root],
                &identity(root, &index),
                ObservationLimits::default(),
                BuildLimits::default(),
            ),
            Err(ComparisonError::DescriptorRootMismatch)
        ));
    }

    #[test]
    fn advertised_fields_and_decode_failures_reject_with_exact_errors() {
        let (objects, root) = empty_native();
        let descriptors: Vec<_> = objects
            .iter()
            .map(|(id, bytes)| (*id, bytes.len() as u64))
            .collect();
        let index =
            index_oracle::build_descriptor_index(&descriptors, BuildLimits::default()).unwrap();
        let original = identity(root, &index);
        let mut cases = Vec::new();
        for (offset, error) in [
            (10, ComparisonError::ProfileMismatch),
            (13, ComparisonError::RootsMismatch),
            (45, ComparisonError::DescriptorRootMismatch),
            (84, ComparisonError::ObjectCountMismatch),
            (92, ComparisonError::SourceBytesMismatch),
        ] {
            let mut changed = original.clone();
            if offset == 10 {
                changed[offset] = 2;
            } else {
                changed[offset] ^= 1;
            }
            cases.push((changed, error));
        }
        cases.push((vec![], ComparisonError::Index(OracleError::Truncated)));
        for (bytes, expected) in cases {
            assert_eq!(
                compare_advertised_source(
                    &objects,
                    &[root],
                    &bytes,
                    ObservationLimits::default(),
                    BuildLimits::default(),
                )
                .err(),
                Some(expected)
            );
        }
        let tree: ObjectId = objects[&root][14..46].try_into().unwrap();
        let mut missing = objects.clone();
        missing.remove(&tree);
        assert_eq!(
            compare_advertised_source(
                &missing,
                &[root],
                &original,
                ObservationLimits::default(),
                BuildLimits::default(),
            )
            .err(),
            Some(ComparisonError::Native(ObservationError::MissingObject {
                id: tree
            }))
        );
        let limits = BuildLimits {
            max_entries: 0,
            ..BuildLimits::default()
        };
        assert_eq!(
            compare_advertised_source(
                &objects,
                &[root],
                &original,
                ObservationLimits::default(),
                limits,
            )
            .err(),
            Some(ComparisonError::ComparisonBufferLimit)
        );
    }
}
