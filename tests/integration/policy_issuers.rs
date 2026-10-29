use agentscm::Error;
use agentscm::policy::{
    CommonControl, ISSUER_REGISTRY_VERSION, IndependenceAssessment, IssuerKeyRecord, IssuerRecord,
    IssuerRegistrySnapshot, KeyTrustStatus, MAX_CHECK_KINDS, MAX_CONTROL_ROOTS,
    MAX_ISSUER_REGISTRY_BYTES, MAX_ISSUERS, MAX_KEYS_PER_ISSUER, issuer_key_id,
};

const CHECK_KIND: &str = "model-review/v1";

fn public_key(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn key(byte: u8, not_before_ms: u64, not_after_ms: u64) -> IssuerKeyRecord {
    let public_key_hex = public_key(byte);
    IssuerKeyRecord {
        key_id: issuer_key_id(&public_key_hex).unwrap(),
        public_key_hex,
        not_before_ms,
        not_after_ms,
        revoked_at_ms: None,
    }
}

fn issuer(id: &str, domain: &str, roots: &[&str], key_byte: u8) -> IssuerRecord {
    IssuerRecord {
        issuer_id: id.to_owned(),
        trust_domain: domain.to_owned(),
        common_control: CommonControl::Known {
            roots: roots.iter().map(|root| (*root).to_owned()).collect(),
        },
        allowed_check_kinds: vec![CHECK_KIND.to_owned()],
        keys: vec![key(key_byte, 100, 1_000)],
    }
}

fn registry(issuers: Vec<IssuerRecord>) -> IssuerRegistrySnapshot {
    IssuerRegistrySnapshot {
        version: ISSUER_REGISTRY_VERSION,
        repository: "repo-1".to_owned(),
        epoch: 7,
        issuers,
    }
}

fn selected(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

fn assert_groups(assessment: &IndependenceAssessment, expected: &[&[&str]]) {
    let expected: Vec<Vec<String>> = expected
        .iter()
        .map(|group| group.iter().map(|id| (*id).to_owned()).collect())
        .collect();
    assert_eq!(assessment.groups, expected);
    assert_eq!(assessment.independent_count, expected.len());
}

#[test]
fn distinct_domains_and_roots_count_independently() {
    let registry = registry(vec![
        issuer("issuer-a", "domain-a", &["owner-a"], 1),
        issuer("issuer-b", "domain-b", &["owner-b"], 2),
    ]);

    let assessment = registry
        .assess_independence(&selected(&["issuer-b", "issuer-a"]), CHECK_KIND)
        .unwrap();

    assert_groups(&assessment, &[&["issuer-a"], &["issuer-b"]]);
    assert!(assessment.unknown_issuer_ids.is_empty());
    assert!(assessment.unknown_control_issuer_ids.is_empty());
    assert!(assessment.disallowed_check_kind_issuer_ids.is_empty());
}

#[test]
fn shared_key_material_is_rejected_across_issuer_aliases() {
    let duplicate_key_registry = registry(vec![
        issuer("issuer-a", "domain-a", &["owner-a"], 1),
        issuer("issuer-alias", "domain-b", &["owner-b"], 1),
    ]);

    assert!(matches!(
        duplicate_key_registry.validate(),
        Err(Error::Invalid(_))
    ));

    let registry = registry(vec![
        issuer("issuer-a", "domain-a", &["owner-a"], 1),
        issuer("issuer-a", "domain-b", &["owner-b"], 2),
    ]);
    assert!(matches!(registry.validate(), Err(Error::Invalid(_))));
}

#[test]
fn same_administrator_with_different_names_is_one_group() {
    let registry = registry(vec![
        issuer("issuer-a", "shared-domain", &["owner-a"], 1),
        issuer("issuer-b", "shared-domain", &["owner-b"], 2),
    ]);

    let assessment = registry
        .assess_independence(&selected(&["issuer-a", "issuer-b"]), CHECK_KIND)
        .unwrap();

    assert_groups(&assessment, &[&["issuer-a", "issuer-b"]]);
}

#[test]
fn duplicate_selected_eligible_issuer_counts_once() {
    let registry = registry(vec![issuer("issuer-a", "domain-a", &["owner-a"], 1)]);

    let assessment = registry
        .assess_independence(&selected(&["issuer-a", "issuer-a"]), CHECK_KIND)
        .unwrap();

    assert_groups(&assessment, &[&["issuer-a"]]);
    assert_eq!(assessment.duplicate_issuer_ids, selected(&["issuer-a"]));
}

#[test]
fn unselected_intermediary_connects_transitive_control() {
    let registry = registry(vec![
        issuer("issuer-a", "domain-a", &["owner-a"], 1),
        issuer("issuer-bridge", "domain-bridge", &["owner-a", "owner-c"], 2),
        issuer("issuer-c", "domain-c", &["owner-c"], 3),
    ]);

    let assessment = registry
        .assess_independence(&selected(&["issuer-a", "issuer-c"]), CHECK_KIND)
        .unwrap();

    assert_groups(&assessment, &[&["issuer-a", "issuer-c"]]);
}

#[test]
fn unknown_control_duplicates_unknown_issuers_and_disallowed_kinds_are_explicit() {
    let mut unknown_control = issuer("issuer-unknown-control", "domain-u", &[], 1);
    unknown_control.common_control = CommonControl::Unknown {};
    let mut disallowed = issuer("issuer-disallowed", "domain-d", &["owner-d"], 2);
    disallowed.allowed_check_kinds = vec!["source-paths/v1".to_owned()];
    let registry = registry(vec![unknown_control, disallowed]);

    let assessment = registry
        .assess_independence(
            &selected(&[
                "issuer-unknown-control",
                "issuer-unknown-control",
                "issuer-disallowed",
                "issuer-missing",
            ]),
            CHECK_KIND,
        )
        .unwrap();

    assert_groups(&assessment, &[]);
    assert_eq!(
        assessment.unknown_control_issuer_ids,
        selected(&["issuer-unknown-control"])
    );
    assert_eq!(
        assessment.disallowed_check_kind_issuer_ids,
        selected(&["issuer-disallowed"])
    );
    assert_eq!(assessment.unknown_issuer_ids, selected(&["issuer-missing"]));
    assert_eq!(
        assessment.duplicate_issuer_ids,
        selected(&["issuer-unknown-control"])
    );
}

#[test]
fn canonical_digest_is_invariant_to_every_set_like_permutation() {
    let mut issuer_a = issuer("issuer-a", "domain-a", &["owner-z", "owner-a"], 1);
    issuer_a.allowed_check_kinds = vec!["z-check/v1".to_owned(), CHECK_KIND.to_owned()];
    issuer_a.keys = vec![key(4, 200, 900), key(1, 100, 1_000)];
    let issuer_b = issuer("issuer-b", "domain-b", &["owner-b"], 2);
    let first = registry(vec![issuer_b, issuer_a]);

    let mut second = first.clone();
    second.issuers.reverse();
    for issuer in &mut second.issuers {
        issuer.allowed_check_kinds.reverse();
        issuer.keys.reverse();
        if let CommonControl::Known { roots } = &mut issuer.common_control {
            roots.reverse();
        }
    }

    assert_eq!(
        first.canonical_bytes().unwrap(),
        second.canonical_bytes().unwrap()
    );
    assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    let parsed = IssuerRegistrySnapshot::from_slice(&serde_json::to_vec(&second).unwrap()).unwrap();
    assert_eq!(
        parsed.canonical_bytes().unwrap(),
        first.canonical_bytes().unwrap()
    );
}

#[test]
fn key_trust_enforces_current_revocation_and_time_boundaries() {
    let mut record = issuer("issuer-a", "domain-a", &["owner-a"], 1);
    record.keys[0].not_before_ms = 100;
    record.keys[0].not_after_ms = 200;
    record.keys[0].revoked_at_ms = Some(150);
    let key_id = record.keys[0].key_id.clone();
    let public_key_hex = record.keys[0].public_key_hex.clone();
    let revoked_registry = registry(vec![record]);

    let trusted = revoked_registry
        .key_trust("issuer-a", &key_id, 120, 149)
        .unwrap();
    assert_eq!(trusted.status, KeyTrustStatus::Trusted);
    assert_eq!(
        trusted.public_key_hex.as_deref(),
        Some(public_key_hex.as_str())
    );
    assert_eq!(
        revoked_registry
            .key_trust("issuer-a", &key_id, 100, 100)
            .unwrap()
            .status,
        KeyTrustStatus::Trusted
    );
    let installed_key = revoked_registry
        .key_record("issuer-a", &key_id)
        .unwrap()
        .unwrap();
    assert_eq!(installed_key.public_key_bytes().unwrap(), [1; 32]);
    assert_eq!(
        revoked_registry
            .key_trust("issuer-a", &key_id, 120, 150)
            .unwrap()
            .status,
        KeyTrustStatus::Revoked
    );
    assert_eq!(
        revoked_registry
            .key_trust("issuer-a", &key_id, 121, 120)
            .unwrap()
            .status,
        KeyTrustStatus::FutureIssued
    );
    assert_eq!(
        revoked_registry
            .key_trust("issuer-a", &key_id, 99, 149)
            .unwrap()
            .status,
        KeyTrustStatus::NotYetValid
    );

    let unrevoked = registry(vec![issuer("issuer-b", "domain-b", &["owner-b"], 2)]);
    let unrevoked_key = &unrevoked.issuers[0].keys[0].key_id;
    assert_eq!(
        unrevoked
            .key_trust("issuer-b", unrevoked_key, 999, 999)
            .unwrap()
            .status,
        KeyTrustStatus::Trusted
    );
    assert_eq!(
        unrevoked
            .key_trust("issuer-b", unrevoked_key, 999, 1_000)
            .unwrap()
            .status,
        KeyTrustStatus::Expired
    );
    let unknown_issuer = unrevoked
        .key_trust("missing", unrevoked_key, 120, 120)
        .unwrap();
    assert_eq!(unknown_issuer.status, KeyTrustStatus::UnknownIssuer);
    assert_eq!(unknown_issuer.public_key_hex, None);
    let unknown_key = unrevoked
        .key_trust("issuer-b", &"0".repeat(64), 120, 120)
        .unwrap();
    assert_eq!(unknown_key.status, KeyTrustStatus::UnknownKey);
    assert_eq!(unknown_key.public_key_hex, None);
}

#[test]
fn malformed_key_encodings_and_mismatched_derived_ids_are_rejected() {
    assert!(matches!(
        issuer_key_id(&"A".repeat(64)),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        issuer_key_id(&"00".repeat(31)),
        Err(Error::Invalid(_))
    ));

    let mut registry = registry(vec![issuer("issuer-a", "domain-a", &["owner-a"], 1)]);
    registry.issuers[0].keys[0].key_id = "0".repeat(64);
    assert!(matches!(registry.validate(), Err(Error::Invalid(_))));
}

#[test]
fn collection_caps_and_timestamp_bounds_fail_closed() {
    let mut too_many_issuers = registry(
        (0..=MAX_ISSUERS)
            .map(|index| IssuerRecord {
                issuer_id: format!("issuer-{index}"),
                trust_domain: format!("domain-{index}"),
                common_control: CommonControl::Known {
                    roots: vec![format!("owner-{index}")],
                },
                allowed_check_kinds: Vec::new(),
                keys: Vec::new(),
            })
            .collect(),
    );
    assert!(matches!(too_many_issuers.validate(), Err(Error::Limit(_))));
    too_many_issuers.issuers.truncate(1);

    let mut too_many_keys = issuer("issuer-a", "domain-a", &["owner-a"], 1);
    too_many_keys.keys = (0..=MAX_KEYS_PER_ISSUER)
        .map(|index| key(index as u8 + 1, 0, 1_000))
        .collect();
    assert!(matches!(
        registry(vec![too_many_keys]).validate(),
        Err(Error::Limit(_))
    ));

    let roots: Vec<String> = (0..=MAX_CONTROL_ROOTS)
        .map(|index| format!("owner-{index}"))
        .collect();
    let mut too_many_roots = issuer("issuer-a", "domain-a", &["owner-a"], 1);
    too_many_roots.common_control = CommonControl::Known { roots };
    assert!(matches!(
        registry(vec![too_many_roots]).validate(),
        Err(Error::Limit(_))
    ));

    let mut too_many_kinds = issuer("issuer-a", "domain-a", &["owner-a"], 1);
    too_many_kinds.allowed_check_kinds = (0..=MAX_CHECK_KINDS)
        .map(|index| format!("check-{index}/v1"))
        .collect();
    assert!(matches!(
        registry(vec![too_many_kinds]).validate(),
        Err(Error::Limit(_))
    ));

    let mut invalid_time = issuer("issuer-a", "domain-a", &["owner-a"], 1);
    invalid_time.keys[0].not_after_ms = invalid_time.keys[0].not_before_ms;
    assert!(matches!(
        registry(vec![invalid_time]).validate(),
        Err(Error::Invalid(_))
    ));

    let mut oversized_time = issuer("issuer-a", "domain-a", &["owner-a"], 1);
    oversized_time.keys[0].revoked_at_ms = Some(i64::MAX as u64 + 1);
    assert!(matches!(
        registry(vec![oversized_time]).validate(),
        Err(Error::Limit(_))
    ));
}

#[test]
fn duplicates_empty_known_control_and_canonical_size_fail_closed() {
    let mut duplicate_root = issuer("issuer-a", "domain-a", &["owner-a", "owner-a"], 1);
    assert!(matches!(
        registry(vec![duplicate_root.clone()]).validate(),
        Err(Error::Invalid(_))
    ));
    duplicate_root.common_control = CommonControl::Known { roots: Vec::new() };
    assert!(matches!(
        registry(vec![duplicate_root]).validate(),
        Err(Error::Invalid(_))
    ));

    let mut duplicate_kind = issuer("issuer-a", "domain-a", &["owner-a"], 1);
    duplicate_kind
        .allowed_check_kinds
        .push(CHECK_KIND.to_owned());
    assert!(matches!(
        registry(vec![duplicate_kind]).validate(),
        Err(Error::Invalid(_))
    ));

    let largest_identifier = "x".repeat(128);
    let large_registry = registry(
        (0..MAX_ISSUERS)
            .map(|index| IssuerRecord {
                issuer_id: format!("issuer-{index}"),
                trust_domain: format!("domain-{index}"),
                common_control: CommonControl::Known {
                    roots: (0..MAX_CONTROL_ROOTS)
                        .map(|root| format!("r{index}-{root}-{largest_identifier}"))
                        .map(|root| root[..128].to_owned())
                        .collect(),
                },
                allowed_check_kinds: (0..MAX_CHECK_KINDS)
                    .map(|kind| format!("k{index}-{kind}-{largest_identifier}"))
                    .map(|kind| kind[..128].to_owned())
                    .collect(),
                keys: Vec::new(),
            })
            .collect(),
    );
    large_registry.validate().unwrap();
    assert!(matches!(
        large_registry.canonical_bytes(),
        Err(Error::Limit(_))
    ));
}

#[test]
fn input_size_and_unknown_fields_are_rejected() {
    let oversized = vec![b' '; MAX_ISSUER_REGISTRY_BYTES + 1];
    assert!(matches!(
        IssuerRegistrySnapshot::from_slice(&oversized),
        Err(Error::Limit(_))
    ));

    let input = br#"{
        "version":1,
        "repository":"repo-1",
        "epoch":0,
        "issuers":[],
        "candidate_claimed_trusted":true
    }"#;
    assert!(matches!(
        IssuerRegistrySnapshot::from_slice(input),
        Err(Error::Json(_))
    ));
}
