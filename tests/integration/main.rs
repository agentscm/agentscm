//! The integration tests, built as one binary so the crate links once.
//!
//! Every file in this directory is a module declared here. Shared helpers are declared once
//! below and imported with `use crate::<helper>;`. Only the modules declared without
//! `#[cfg(unix)]` (the Windows set) compile on Windows.

// Shared helpers.
#[cfg(unix)]
#[path = "common/fake_docker.rs"]
mod fake_docker;
#[path = "../support/fake_github.rs"]
mod fake_github;
#[path = "../support/fake_gitlab.rs"]
mod fake_gitlab;
#[path = "common/hub_pg.rs"]
mod hub_pg;
#[cfg(unix)]
#[path = "common/team_pg.rs"]
mod team_pg;
#[cfg(unix)]
#[path = "common/workspace_hub.rs"]
mod workspace_hub;

// Test modules.
#[cfg(unix)]
mod agent_stop_cli;
#[cfg(unix)]
mod agent_stop_install;
#[cfg(unix)]
mod agent_task_cli;
#[cfg(unix)]
mod attached_worktree;
#[cfg(unix)]
mod attached_worktree_gitlab;
#[cfg(unix)]
mod authority_context;
#[cfg(unix)]
mod authority_integration;
#[cfg(unix)]
mod authority_read_only;
#[cfg(unix)]
mod autopsy_cli;
#[cfg(unix)]
mod backend;
#[cfg(unix)]
mod backend_s3;
#[cfg(unix)]
mod change_capture;
#[cfg(unix)]
mod checks;
mod cli;
#[cfg(unix)]
mod client_completion;
#[cfg(unix)]
mod client_status;
#[cfg(unix)]
mod collisions_http;
#[cfg(unix)]
mod command_checks;
mod context_contract;
#[cfg(unix)]
mod continuation;
#[cfg(unix)]
mod continuity;
mod continuity_remote;
#[cfg(unix)]
mod demo_cli;
#[cfg(unix)]
mod distribution;
#[cfg(unix)]
mod entry_kinds;
#[cfg(unix)]
mod evidence;
#[cfg(unix)]
mod evidence_conformance;
#[cfg(unix)]
mod execution;
#[cfg(unix)]
mod execution_canaries;
mod fake_github_smoke;
mod fake_gitlab_smoke;
#[cfg(unix)]
mod front_door_http;
#[cfg(unix)]
mod git_collisions;
#[cfg(unix)]
mod git_export;
#[cfg(unix)]
mod git_reconcile;
#[cfg(unix)]
mod git_remote;
#[cfg(unix)]
mod git_snapshot;
#[cfg(unix)]
mod git_snapshot_cli;
#[cfg(unix)]
mod git_snapshot_kinds;
#[cfg(unix)]
mod gitlab_publication_client;
#[cfg(unix)]
mod guided_cli;
#[cfg(unix)]
mod guided_service;
#[cfg(unix)]
mod history;
mod hook_cli;
mod hook_end_to_end;
#[cfg(unix)]
mod hosted_laptop_workflow;
#[cfg(unix)]
mod http;
#[cfg(unix)]
mod hub_catchup;
#[cfg(unix)]
mod hub_catchup_preflight;
#[cfg(unix)]
mod hub_catchup_startup;
#[cfg(unix)]
mod hub_change_record;
#[cfg(unix)]
mod hub_change_record_http;
#[cfg(unix)]
mod hub_change_record_migration;
#[cfg(unix)]
mod hub_coordination;
#[cfg(unix)]
mod hub_devices;
#[cfg(unix)]
mod hub_external_runs;
#[cfg(unix)]
mod hub_forge_check;
mod hub_gitlab;
#[cfg(unix)]
mod hub_gitlab_forge_check;
#[cfg(unix)]
mod hub_gitlab_publish;
#[cfg(unix)]
mod hub_handoffs;
mod hub_hooks;
#[cfg(unix)]
mod hub_http;
#[cfg(unix)]
mod hub_insights_http;
#[cfg(unix)]
mod hub_insights_store;
mod hub_intents;
mod hub_login;
mod hub_mcp;
#[cfg(unix)]
mod hub_merge_queue;
#[cfg(unix)]
mod hub_narrative;
#[cfg(unix)]
mod hub_promises;
#[cfg(unix)]
mod hub_publication_store;
#[cfg(unix)]
mod hub_publish;
#[cfg(unix)]
mod hub_read_only;
mod hub_sessions;
#[cfg(unix)]
mod hub_skill_workflow;
#[cfg(unix)]
mod hub_skills;
#[cfg(unix)]
mod hub_standup_http;
#[cfg(unix)]
mod hub_store_conformance;
#[cfg(unix)]
mod hub_sync;
#[cfg(unix)]
mod hub_ui;
#[cfg(unix)]
mod hub_work;
#[cfg(unix)]
mod hub_work_auto_compaction;
#[cfg(unix)]
mod hub_workflow;
#[cfg(unix)]
mod hub_workspace;
#[cfg(unix)]
mod hub_workspace_execution;
mod init_repo;
#[cfg(unix)]
mod intent_cli;
#[cfg(unix)]
mod intent_review_host;
#[cfg(unix)]
mod issue_workflow;
#[cfg(unix)]
mod issue_workflow_recovery;
#[cfg(unix)]
mod license;
#[cfg(unix)]
mod local_read_http;
#[cfg(unix)]
mod local_recovery;
#[cfg(unix)]
mod local_recovery_git_reconcile;
#[cfg(unix)]
mod local_recovery_issue_config_cli;
#[cfg(unix)]
mod local_recovery_issue_workflow;
#[cfg(unix)]
mod local_validation;
#[cfg(unix)]
mod local_validation_cli;
#[cfg(unix)]
mod local_validation_fetch;
#[cfg(unix)]
mod local_validation_invalidate;
#[cfg(unix)]
mod local_validation_reconcile;
#[cfg(unix)]
mod merge;
#[cfg(unix)]
mod merge_cli;
#[cfg(unix)]
mod operating_policy;
#[cfg(unix)]
mod pipeline_rules;
#[cfg(unix)]
mod policy_attempts;
#[cfg(unix)]
mod policy_evaluation;
#[cfg(unix)]
mod policy_issuers;
#[cfg(unix)]
mod productive_actions;
#[cfg(unix)]
mod promise_agent_interfaces;
#[cfg(unix)]
mod qualification_oracle;
#[cfg(unix)]
mod quickstart;
#[cfg(unix)]
mod repo_explorer_evidence;
#[cfg(unix)]
mod repo_explorer_git;
#[cfg(unix)]
mod reviewer;
#[cfg(unix)]
mod reviewer_claude_code;
#[cfg(unix)]
mod setup_cli;
#[cfg(unix)]
mod skill_cli;
#[cfg(unix)]
mod skill_workflow;
#[cfg(unix)]
mod skill_workflow_http;
#[cfg(unix)]
mod skill_workflow_library;
#[cfg(unix)]
mod slack_setup_cli;
#[cfg(unix)]
mod snapshot_blackbox;
#[cfg(unix)]
mod snapshot_roundtrip;
#[cfg(unix)]
mod source_profile;
#[cfg(unix)]
mod source_reader;
#[cfg(unix)]
mod store_engine;
#[cfg(unix)]
mod sync;
#[cfg(unix)]
mod task_workspace;
#[cfg(unix)]
mod task_workspace_flow;
#[cfg(unix)]
mod team_auth;
#[cfg(unix)]
mod team_cli;
#[cfg(unix)]
mod team_http;
#[cfg(unix)]
mod team_publication;
#[cfg(unix)]
mod team_retirement;
#[cfg(unix)]
mod team_source;
#[cfg(unix)]
mod team_source_profile;
#[cfg(unix)]
mod telemetry;
#[cfg(unix)]
mod work_cli;
#[cfg(unix)]
mod workflow_cli;
#[cfg(unix)]
mod workflow_http;
#[cfg(unix)]
mod workspace;
#[cfg(unix)]
mod workspace_client;
#[cfg(unix)]
mod workspace_execution;
#[cfg(unix)]
mod workspace_snapshots;
#[cfg(unix)]
mod workspace_timeline_service;

/// A test file that is not declared above would silently never run.
#[test]
fn every_test_file_is_declared_in_main() {
    let source = include_str!("main.rs");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let stem = path.file_stem().unwrap().to_str().unwrap().to_owned();
            if stem != "main" && !source.lines().any(|l| l == format!("mod {stem};")) {
                missing.push(stem);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "declare in tests/integration/main.rs: {missing:?}"
    );
}
