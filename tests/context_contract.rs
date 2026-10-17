use agentscm::Error;
use agentscm::context::{
    BaseState, ChangeId, ContextReadiness, DependencyClaim, DependencyState, InputIdentity,
    MAX_REVIEW_CONTEXT_BYTES, REVIEW_CONTEXT_VERSION, RequiredEvidence, ReviewContext,
    SUPPORTED_CAPABILITIES,
};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

fn digest(value: &str) -> InputIdentity {
    InputIdentity::Digest {
        sha256: value.to_owned(),
    }
}

fn minimal_context() -> ReviewContext {
    ReviewContext {
        version: REVIEW_CONTEXT_VERSION,
        change: ChangeId {
            repository: "repo-1".to_owned(),
            id: "change-1".to_owned(),
        },
        change_version: 1,
        revision: A.to_owned(),
        base: None,
        target: "main".to_owned(),
        policy_epoch: 0,
        intent: "fix".to_owned(),
        acceptance_criteria: vec!["passes".to_owned()],
        scope: vec!["src/lib.rs".to_owned()],
        exclusions: Vec::new(),
        omissions: Vec::new(),
        dependencies: Vec::new(),
        recipe: digest(B),
        toolchain: digest(C),
        environment: digest(D),
        required_evidence: Vec::new(),
        required_capabilities: vec!["agentscm.review-context/v1".to_owned()],
    }
}

#[test]
fn minimal_canonical_vector_is_fixed() {
    let expected = concat!(
        "{\"version\":1,\"change\":{\"repository\":\"repo-1\",\"id\":\"change-1\"},",
        "\"change_version\":1,\"revision\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
        "\"base\":null,\"target\":\"main\",\"policy_epoch\":0,\"intent\":\"fix\",",
        "\"acceptance_criteria\":[\"passes\"],\"scope\":[\"src/lib.rs\"],\"exclusions\":[],",
        "\"omissions\":[],\"dependencies\":[],\"recipe\":{\"state\":\"digest\",",
        "\"sha256\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"},",
        "\"toolchain\":{\"state\":\"digest\",\"sha256\":\"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\"},",
        "\"environment\":{\"state\":\"digest\",\"sha256\":\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\"},",
        "\"required_evidence\":[],\"required_capabilities\":[\"agentscm.review-context/v1\"]}"
    );
    let context = minimal_context();

    assert_eq!(context.canonical_bytes().unwrap(), expected.as_bytes());
    assert_eq!(expected.len(), 722);
    assert_eq!(
        context.digest().unwrap(),
        "42292d3a9794a1ac1fdb220d195ac552fbc3042c6f09e9f838735607c8a2b46b"
    );
    assert_eq!(
        ReviewContext::from_slice(expected.as_bytes()).unwrap(),
        context
    );
    assert_eq!(
        context.readiness(SUPPORTED_CAPABILITIES),
        ContextReadiness::Complete
    );
    assert_eq!(
        serde_json::to_value(ContextReadiness::Complete).unwrap(),
        serde_json::json!({"status": "complete"})
    );
}

#[test]
fn full_vector_sorts_every_set_like_collection() {
    let mut context = minimal_context();
    context.change_version = 2;
    context.base = Some(BaseState {
        revision: E.to_owned(),
        target_version: 7,
    });
    context.policy_epoch = 3;
    context.acceptance_criteria = vec!["z criterion".to_owned(), "a criterion".to_owned()];
    context.scope = vec!["src/z.rs".to_owned(), "src/a.rs".to_owned()];
    context.exclusions = vec!["vendor/z".to_owned(), "vendor/a".to_owned()];
    context.omissions = vec!["generated/z".to_owned(), "generated/a".to_owned()];
    context.dependencies = vec![
        DependencyClaim {
            identity: "z-dependency".to_owned(),
            state: DependencyState::Observed {
                revision: Some(D.to_owned()),
                evidence_digest: E.to_owned(),
            },
        },
        DependencyClaim {
            identity: "a-dependency".to_owned(),
            state: DependencyState::Resolved {
                revision: B.to_owned(),
            },
        },
    ];
    context.required_evidence = vec![
        RequiredEvidence {
            identity: "z-check".to_owned(),
            digest: digest(E),
        },
        RequiredEvidence {
            identity: "a-check".to_owned(),
            digest: digest(A),
        },
    ];
    context.required_capabilities = vec![
        "z-capability/v1".to_owned(),
        "agentscm.review-context/v1".to_owned(),
    ];

    let expected = concat!(
        "{\"version\":1,\"change\":{\"repository\":\"repo-1\",\"id\":\"change-1\"},",
        "\"change_version\":2,\"revision\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
        "\"base\":{\"revision\":\"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\",\"target_version\":7},",
        "\"target\":\"main\",\"policy_epoch\":3,\"intent\":\"fix\",",
        "\"acceptance_criteria\":[\"a criterion\",\"z criterion\"],\"scope\":[\"src/a.rs\",\"src/z.rs\"],",
        "\"exclusions\":[\"vendor/a\",\"vendor/z\"],\"omissions\":[\"generated/a\",\"generated/z\"],\"dependencies\":[",
        "{\"identity\":\"a-dependency\",\"state\":{\"state\":\"resolved\",\"revision\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"}},",
        "{\"identity\":\"z-dependency\",\"state\":{\"state\":\"observed\",\"revision\":\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\",",
        "\"evidence_digest\":\"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\"}}],",
        "\"recipe\":{\"state\":\"digest\",\"sha256\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"},",
        "\"toolchain\":{\"state\":\"digest\",\"sha256\":\"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\"},",
        "\"environment\":{\"state\":\"digest\",\"sha256\":\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\"},",
        "\"required_evidence\":[{\"identity\":\"a-check\",\"digest\":{\"state\":\"digest\",\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}},",
        "{\"identity\":\"z-check\",\"digest\":{\"state\":\"digest\",\"sha256\":\"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\"}}],",
        "\"required_capabilities\":[\"agentscm.review-context/v1\",\"z-capability/v1\"]}"
    );

    assert_eq!(context.canonical_bytes().unwrap(), expected.as_bytes());
    assert_eq!(
        context.digest().unwrap(),
        "faf7a298927db5c003583a8c434b971d6e1c73f1ece69c0c39172882c2a8c6f2"
    );
    let parsed = ReviewContext::from_slice(&serde_json::to_vec(&context).unwrap()).unwrap();
    assert_eq!(parsed.canonical_bytes().unwrap(), expected.as_bytes());
    assert_eq!(parsed.acceptance_criteria[0], "a criterion");

    context.acceptance_criteria.reverse();
    context.scope.reverse();
    context.exclusions.reverse();
    context.omissions.reverse();
    context.dependencies.reverse();
    context.required_evidence.reverse();
    context.required_capabilities.reverse();
    assert_eq!(context.canonical_bytes().unwrap(), expected.as_bytes());
}

#[test]
fn duplicate_values_and_identities_are_rejected() {
    let mut context = minimal_context();
    context.scope.push("src/lib.rs".to_owned());
    assert!(matches!(context.validate(), Err(Error::Invalid(_))));

    let mut context = minimal_context();
    context.dependencies = vec![
        DependencyClaim {
            identity: "shared".to_owned(),
            state: DependencyState::Declared {},
        },
        DependencyClaim {
            identity: "shared".to_owned(),
            state: DependencyState::Unknown {},
        },
    ];
    assert!(matches!(context.validate(), Err(Error::Invalid(_))));
}

#[test]
fn incomplete_inputs_omissions_and_capabilities_are_explicit() {
    let mut context = minimal_context();
    context.recipe = InputIdentity::Unknown {};
    context.environment = InputIdentity::Unavailable {
        reason: "runner missing".to_owned(),
    };
    context.omissions = vec!["generated source".to_owned()];
    context.dependencies = vec![DependencyClaim {
        identity: "dep".to_owned(),
        state: DependencyState::Unknown {},
    }];
    context.required_evidence = vec![RequiredEvidence {
        identity: "tests".to_owned(),
        digest: InputIdentity::Unavailable {
            reason: "not run".to_owned(),
        },
    }];
    context
        .required_capabilities
        .push("future-capability/v1".to_owned());

    context.validate().unwrap();
    let ContextReadiness::Incomplete { reasons } = context.readiness(SUPPORTED_CAPABILITIES) else {
        panic!("context should be incomplete");
    };
    assert_eq!(reasons.len(), 6);
    assert!(reasons.iter().any(|reason| reason == "recipe is unknown"));
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("declared omission"))
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("future-capability"))
    );
}

#[test]
fn size_and_sql_integer_limits_fail_closed() {
    let oversized = vec![b' '; MAX_REVIEW_CONTEXT_BYTES + 1];
    assert!(matches!(
        ReviewContext::from_slice(&oversized),
        Err(Error::Limit(_))
    ));

    let mut context = minimal_context();
    context.policy_epoch = i64::MAX as u64 + 1;
    assert!(matches!(context.validate(), Err(Error::Limit(_))));

    let mut context = minimal_context();
    context.change_version = i64::MAX as u64 + 1;
    assert!(matches!(context.validate(), Err(Error::Limit(_))));

    let mut context = minimal_context();
    context.base = Some(BaseState {
        revision: E.to_owned(),
        target_version: i64::MAX as u64 + 1,
    });
    assert!(matches!(context.validate(), Err(Error::Limit(_))));

    let mut context = minimal_context();
    context.intent = "x".repeat(8 * 1024 + 1);
    assert!(matches!(context.validate(), Err(Error::Limit(_))));

    let mut context = minimal_context();
    context.acceptance_criteria = (0..129).map(|index| format!("criterion-{index}")).collect();
    assert!(matches!(context.validate(), Err(Error::Limit(_))));

    let mut context = minimal_context();
    context.scope = (0..64)
        .map(|index| format!("{index:03}{}", "x".repeat(1021)))
        .collect();
    assert!(matches!(context.canonical_bytes(), Err(Error::Limit(_))));
}

#[test]
fn zero_versions_and_noncanonical_digests_are_rejected() {
    let mut context = minimal_context();
    context.change_version = 0;
    assert!(matches!(context.validate(), Err(Error::Invalid(_))));

    let mut context = minimal_context();
    context.base = Some(BaseState {
        revision: E.to_owned(),
        target_version: 0,
    });
    assert!(matches!(context.validate(), Err(Error::Invalid(_))));

    let mut context = minimal_context();
    context.revision = A.to_uppercase();
    assert!(matches!(context.validate(), Err(Error::Invalid(_))));
}

#[test]
fn unknown_json_fields_are_rejected() {
    let mut bytes = minimal_context().canonical_bytes().unwrap();
    bytes.pop();
    bytes.extend_from_slice(b",\"surprise\":true}");
    assert!(matches!(
        ReviewContext::from_slice(&bytes),
        Err(Error::Json(_))
    ));
}

#[test]
fn nested_unknown_fields_are_rejected_for_every_state_variant() {
    use serde_json::json;
    for state in [
        json!({"state": "digest", "sha256": B}),
        json!({"state": "unknown"}),
        json!({"state": "unavailable", "reason": "missing"}),
    ] {
        let mut value = serde_json::to_value(minimal_context()).unwrap();
        value["recipe"] = state.clone();
        let parsed = ReviewContext::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&parsed.recipe).unwrap(), state);
        value["recipe"]["surprise"] = json!(true);
        assert!(
            matches!(
                ReviewContext::from_slice(&serde_json::to_vec(&value).unwrap()),
                Err(Error::Json(_))
            ),
            "accepted unknown input field: {value}"
        );
    }
    let mut evidence = serde_json::to_value(minimal_context()).unwrap();
    evidence["required_evidence"] = json!([{"identity": "check", "digest": {"state": "unknown"}}]);
    let parsed = ReviewContext::from_slice(&serde_json::to_vec(&evidence).unwrap()).unwrap();
    assert!(matches!(
        parsed.readiness(SUPPORTED_CAPABILITIES),
        ContextReadiness::Incomplete { .. }
    ));
    evidence["required_evidence"][0]["digest"]["surprise"] = json!(true);
    assert!(matches!(
        ReviewContext::from_slice(&serde_json::to_vec(&evidence).unwrap()),
        Err(Error::Json(_))
    ));
    for state in [
        json!({"state": "declared"}),
        json!({"state": "resolved", "revision": A}),
        json!({"state": "observed", "revision": null, "evidence_digest": B}),
        json!({"state": "unknown"}),
        json!({"state": "unavailable", "reason": "missing"}),
    ] {
        let mut value = serde_json::to_value(minimal_context()).unwrap();
        value["dependencies"] = json!([{"identity": "dep", "state": state}]);
        let parsed = ReviewContext::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed.dependencies[0].state).unwrap(),
            state
        );
        value["dependencies"][0]["state"]["surprise"] = json!(true);
        assert!(
            matches!(
                ReviewContext::from_slice(&serde_json::to_vec(&value).unwrap()),
                Err(Error::Json(_))
            ),
            "accepted unknown dependency field: {value}"
        );
    }
}
