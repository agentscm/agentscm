use super::*;
use agentscm::git_export::remote::network::trust_digest;
use agentscm::git_export::remote::{Access, Endpoint, HttpsTrust, SshAccess};
use serde_json::json;
use std::process::Stdio;

#[path = "../support/git_https.rs"]
mod https_fixture;
use https_fixture::HttpsFixture;

#[test]
fn https_authenticated_native_export_fetch_and_hook_rejection() {
    if isolated_test("network_tests::https_authenticated_native_export_fetch_and_hook_rejection") {
        return;
    }
    let fixture = HttpsFixture::new(false);
    let (root, engine, mut authority, path, mut mirror) = setup();
    mirror
        .remote_bind_network(fixture.endpoint(), "main")
        .unwrap();
    let access = fixture.access();
    assert_eq!(mirror.remote_observe_with(&access).unwrap(), None);
    let one = publish(
        &engine,
        &mut authority,
        vec![change("file", Some(b"HTTPS bytes"))],
    );
    let first = mirror
        .export(&engine, &authority, &request(&one, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    let operation = Request::Publish {
        operation_id: "https-one".into(),
        event_sequence: one.sequence,
        expected_oid: None,
    };
    let receipt = mirror.remote_run_with(&operation, &access).unwrap();
    assert_eq!(receipt.state, State::Published);
    assert_eq!(
        run(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        first
    );
    assert_eq!(
        mirror.remote_run_with(&operation, &access).unwrap(),
        receipt
    );
    let receiving = Mirror::initialize(
        &git(),
        &root.path().join("incoming.git"),
        &authority.repository_id().unwrap(),
        "incoming",
    )
    .unwrap();
    receiving
        .remote_bind_network(fixture.endpoint(), "main")
        .unwrap();
    let incoming = receiving
        .remote_run_with(
            &Request::Fetch {
                operation_id: "https-fetch".into(),
                expected_oid: first.clone(),
            },
            &access,
        )
        .unwrap();
    assert_eq!(incoming.state, State::Fetched);
    assert_eq!(incoming.commit, first);
    let checkout = root.path().join("network-checkout");
    fs::create_dir(&checkout).unwrap();
    run(&checkout, &["init", "--template=", "--object-format=sha1"]);
    run(
        &checkout,
        &[
            "fetch",
            "--no-tags",
            root.path().join("incoming.git").to_str().unwrap(),
            incoming.local_ref.as_deref().unwrap(),
        ],
    );
    run(&checkout, &["checkout", "--detach", &first]);
    let capture_request = SnapshotRequest {
        git: git(),
        repository: checkout,
        revision: first.clone(),
        exclude: vec![],
    };
    let report = git_snapshot::preflight(&capture_request).unwrap();
    assert!(report.eligible);
    let captured = git_snapshot::capture(
        &engine,
        &authority.repository_id().unwrap(),
        &capture_request,
        report,
    )
    .unwrap();
    assert_eq!(
        engine.read_file(&captured.revision, "file").unwrap(),
        b"HTTPS bytes"
    );
    assert_eq!(authority.target("main").unwrap(), Some(one.after.clone()));
    let two = publish(&engine, &mut authority, vec![change("file", Some(b"next"))]);
    mirror
        .export(&engine, &authority, &request(&two, Some(first.clone())))
        .unwrap();
    let hooks = fixture.remote.join("hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let rejected = mirror
        .remote_run_with(
            &Request::Publish {
                operation_id: "https-rejected".into(),
                event_sequence: two.sequence,
                expected_oid: Some(first.clone()),
            },
            &access,
        )
        .unwrap();
    assert_eq!(rejected.state, State::Unknown);
    assert_eq!(
        run(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        first
    );
    assert_eq!(
        mirror
            .remote_recover_with("https-rejected", &access)
            .unwrap()
            .state,
        State::ObservedExpected
    );
    let secret: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture.credential).unwrap()).unwrap();
    let bytes = fs::read(path.join("agentscm-remote.sqlite")).unwrap();
    assert!(
        !bytes
            .windows(secret["password"].as_str().unwrap().len())
            .any(|part| part == secret["password"].as_str().unwrap().as_bytes())
    );
}

#[test]
fn https_lost_push_reply_recovers_without_replaying_post() {
    if isolated_test("network_tests::https_lost_push_reply_recovers_without_replaying_post") {
        return;
    }
    let fixture = HttpsFixture::new(false);
    let (_root, engine, mut authority, _, mut mirror) = setup();
    mirror
        .remote_bind_network(fixture.endpoint(), "main")
        .unwrap();
    let access = fixture.access();
    let one = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let oid = mirror
        .export(&engine, &authority, &request(&one, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    fixture.option("lose_push_reply", json!(true));
    let operation = Request::Publish {
        operation_id: "lost".into(),
        event_sequence: one.sequence,
        expected_oid: None,
    };
    assert_eq!(
        mirror.remote_run_with(&operation, &access).unwrap().state,
        State::Unknown
    );
    assert_eq!(run(&fixture.remote, &["rev-parse", "refs/heads/main"]), oid);
    let count = || {
        fixture
            .requests()
            .iter()
            .filter(|r| {
                r["method"] == "POST" && r["path"].as_str().unwrap().ends_with("git-receive-pack")
            })
            .count()
    };
    let before = count();
    assert_eq!(before, 1);
    assert_eq!(
        mirror.remote_run_with(&operation, &access).unwrap().state,
        State::Unknown
    );
    assert_eq!(
        mirror.remote_recover_with("lost", &access).unwrap().state,
        State::ObservedDesired
    );
    assert_eq!(count(), before);
}

#[test]
fn https_refuses_wrong_auth_tls_hostname_redirect_and_changed_trust() {
    if isolated_test(
        "network_tests::https_refuses_wrong_auth_tls_hostname_redirect_and_changed_trust",
    ) {
        return;
    }
    let fixture = HttpsFixture::new(false);
    let (root, _, _, _, mirror) = setup();
    mirror
        .remote_bind_network(fixture.endpoint(), "main")
        .unwrap();
    let wrong = root.path().join("wrong.json");
    fs::write(
        &wrong,
        b"{\"username\":\"synthetic\",\"password\":\"wrong\"}",
    )
    .unwrap();
    fs::set_permissions(&wrong, fs::Permissions::from_mode(0o600)).unwrap();
    let mut access = fixture.access();
    access.credential_file = Some(wrong);
    assert!(mirror.remote_observe_with(&access).is_err());
    let access = fixture.access();
    fixture.option(
        "redirect",
        json!(
            fixture
                .url
                .replace("127.0.0.1", "localhost")
                .replace("fixture.git", "redirect-target")
        ),
    );
    assert!(mirror.remote_observe_with(&access).is_err());
    assert!(
        fixture
            .requests()
            .iter()
            .all(|r| !r["path"].as_str().unwrap().contains("redirect-target"))
    );
    fixture.option("redirect", serde_json::Value::Null);
    let mut changed = fixture.access();
    changed.ca_file = None;
    assert!(mirror.remote_observe_with(&changed).is_err());
    let (_root2, _, authority2, _, mirror2) = setup();
    let _keep_authority = authority2;
    mirror2
        .remote_bind_network(
            Endpoint::Https {
                url: fixture.url.clone(),
                trust: HttpsTrust::System,
            },
            "main",
        )
        .unwrap();
    let mut system = fixture.access();
    system.ca_file = None;
    assert!(mirror2.remote_observe_with(&system).is_err());
    let bad = HttpsFixture::new(true);
    let (_root3, _, _, _, mirror3) = setup();
    mirror3.remote_bind_network(bad.endpoint(), "main").unwrap();
    assert!(mirror3.remote_observe_with(&bad.access()).is_err());
    assert!(bad.requests().is_empty());
}

#[test]
fn endpoint_and_credential_file_profiles_fail_closed() {
    if isolated_test("network_tests::endpoint_and_credential_file_profiles_fail_closed") {
        return;
    }
    for url in [
        "http://example.invalid/repo.git",
        "https://user:password@example.invalid/repo.git",
        "https://example.invalid/repo.git?token=x",
        "https://example.invalid/repo.git#x",
        "https://example.invalid/a%2fb",
        "file:///repo.git",
    ] {
        assert!(
            Endpoint::Https {
                url: url.into(),
                trust: HttpsTrust::System
            }
            .validate()
            .is_err()
        );
    }
    let fixture = HttpsFixture::new(false);
    let (_root, _, _, _, mirror) = setup();
    mirror
        .remote_bind_network(fixture.endpoint(), "main")
        .unwrap();
    let access = fixture.access();
    fs::set_permissions(&fixture.credential, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(mirror.remote_observe_with(&access).is_err());
    assert!(fixture.requests().is_empty());
}

#[test]
fn ssh_explicit_identity_trust_and_git_protocol_profile() {
    if isolated_test("network_tests::ssh_explicit_identity_trust_and_git_protocol_profile") {
        return;
    }
    let (root, engine, mut authority, _, mut mirror) = setup();
    let remote = bare(root.path());
    let identity = root.path().join("identity");
    let host = root.path().join("host");
    for path in [&identity, &host] {
        assert!(
            Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "synthetic", "-f"])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }
    let known = root.path().join("known_hosts");
    let public = fs::read_to_string(host.with_extension("pub")).unwrap();
    fs::write(&known, format!("[synthetic.invalid]:2222 {public}")).unwrap();
    let config = root.path().join("ssh-fixture.json");
    let log = root.path().join("ssh-calls");
    fs::write(&config,serde_json::to_vec(&json!({"identity":identity,"known":known,"known_expected":fs::read_to_string(&known).unwrap(),"rotate_marker":root.path().join("rotated"),"git":git(),"remote":remote,"log":log,"home":std::env::var("HOME").unwrap(),"global":std::env::var("GIT_CONFIG_GLOBAL").unwrap(),"system":std::env::var("GIT_CONFIG_SYSTEM").unwrap()})).unwrap()).unwrap();
    let executable = root.path().join("ssh-fixture");
    let source = format!(
        r#"#!/usr/bin/python3
import json,os,shlex,sys
c=json.load(open({config:?}))
a=sys.argv[1:]
assert a[a.index('-F')+1]=='/dev/null'
assert a[a.index('-i')+1]==c['identity']
options=[a[i+1] for i,v in enumerate(a[:-1]) if v=='-o']
for required in ['BatchMode=yes','IdentitiesOnly=yes','IdentityAgent=none','CertificateFile=/dev/null','PKCS11Provider=none','SecurityKeyProvider=none','StrictHostKeyChecking=yes','UpdateHostKeys=no','PasswordAuthentication=no','ProxyCommand=none','ProxyJump=none']:
 assert required in options
selected=shlex.split(next(v.split('=',1)[1] for v in options if v.startswith('UserKnownHostsFile=')))[0]
assert selected != c['known']
# This occurs after the controller checked/snapshotted trust, before the SSH
# client's simulated host verification reads it.
if not os.path.exists(c['rotate_marker']):
 with open(c['known']+'.new','w') as changed: changed.write('rotated trust\n')
 os.replace(c['known']+'.new',c['known'])
 with open(c['rotate_marker'],'w') as marker: marker.write('rotated')
assert open(selected).read()==c['known_expected']
assert os.stat(selected).st_mode & 0o777 == 0o600
assert os.stat(os.path.dirname(selected)).st_mode & 0o777 == 0o700
assert 'SSH_AUTH_SOCK' not in os.environ
assert a[-2]=='synthetic@synthetic.invalid'
service,path=shlex.split(a[-1])
assert service in ['git-upload-pack','git-receive-pack'] and path=='/team/repo.git'
with open(c['log'],'a') as output: output.write(service+'\t'+selected+'\n')
env={{'PATH':'/usr/bin:/bin','HOME':c['home'],'GIT_CONFIG_GLOBAL':c['global'],'GIT_CONFIG_SYSTEM':c['system']}}
os.execve(c['git'],[c['git'],service[4:],c['remote']],env)
"#,
        config = config.to_str().unwrap()
    );
    fs::write(&executable, source).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = Endpoint::Ssh {
        url: "ssh://synthetic@synthetic.invalid:2222/team/repo.git".into(),
        known_hosts_sha256: trust_digest(&known).unwrap(),
    };
    mirror
        .remote_bind_network(endpoint.clone(), "main")
        .unwrap();
    let mut access = Access {
        credential_file: None,
        credential_helper: None,
        ca_file: None,
        ssh: Some(SshAccess {
            executable,
            identity_file: identity.clone(),
            known_hosts_file: known.clone(),
        }),
    };
    for name in ["identity%h", "identity${HOME}"] {
        let ambiguous = root.path().join(name);
        fs::copy(&identity, &ambiguous).unwrap();
        access.ssh.as_mut().unwrap().identity_file = ambiguous;
        assert!(
            mirror
                .remote_observe_with(&access)
                .unwrap_err()
                .to_string()
                .contains("expansion syntax")
        );
        assert!(!log.exists());
    }
    access.ssh.as_mut().unwrap().identity_file = identity.clone();
    for name in ["known%h", "known${HOME}"] {
        let ambiguous = root.path().join(name);
        fs::copy(&known, &ambiguous).unwrap();
        access.ssh.as_mut().unwrap().known_hosts_file = ambiguous;
        assert!(
            mirror
                .remote_observe_with(&access)
                .unwrap_err()
                .to_string()
                .contains("expansion syntax")
        );
        assert!(!log.exists());
    }
    access.ssh.as_mut().unwrap().known_hosts_file = known.clone();
    let observed = mirror.remote_observe_with(&access);
    assert!(observed.is_ok(), "synthetic SSH protocol fixture failed");
    assert_eq!(observed.unwrap(), None);
    assert_eq!(fs::read_to_string(&known).unwrap(), "rotated trust\n");
    fs::write(&known, format!("[synthetic.invalid]:2222 {public}")).unwrap();
    for call in fs::read_to_string(&log).unwrap().lines() {
        let snapshot = Path::new(call.split_once('\t').unwrap().1);
        assert!(!snapshot.exists());
        assert!(!snapshot.parent().unwrap().exists());
    }
    let event = publish(
        &engine,
        &mut authority,
        vec![change("file", Some(b"SSH profile"))],
    );
    let oid = mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    assert_eq!(
        mirror
            .remote_run_with(
                &Request::Publish {
                    operation_id: "ssh-one".into(),
                    event_sequence: event.sequence,
                    expected_oid: None
                },
                &access
            )
            .unwrap()
            .state,
        State::Published
    );
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), oid);
    let receiving = Mirror::initialize(
        &git(),
        &root.path().join("ssh-incoming.git"),
        &authority.repository_id().unwrap(),
        "incoming",
    )
    .unwrap();
    receiving.remote_bind_network(endpoint, "main").unwrap();
    assert_eq!(
        receiving
            .remote_run_with(
                &Request::Fetch {
                    operation_id: "ssh-fetch".into(),
                    expected_oid: oid
                },
                &access
            )
            .unwrap()
            .state,
        State::Fetched
    );
    let before = fs::read(&log).unwrap();
    fs::write(&known, b"changed host trust\n").unwrap();
    assert!(mirror.remote_observe_with(&access).is_err());
    assert_eq!(fs::read(&log).unwrap(), before);
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(mirror.remote_observe_with(&access).is_err());
}

#[test]
fn credential_helper_refuses_foreign_scope_without_secret_output() {
    if isolated_test("network_tests::credential_helper_refuses_foreign_scope_without_secret_output")
    {
        return;
    }
    use std::io::Write;
    let root = TempDir::new().unwrap();
    let file = root.path().join("credential.json");
    let mut random = [0u8; 24];
    getrandom::fill(&mut random).unwrap();
    let secret = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fs::write(
        &file,
        serde_json::to_vec(&json!({"username":"synthetic","password":secret})).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    for input in [
        "protocol=https\nhost=foreign.invalid\npath=team/repo.git\n\n",
        "protocol=https\nhost=example.invalid\npath=other/repo.git\n\n",
        "protocol=https\nhost=example.invalid\nhost=example.invalid\npath=team/repo.git\n\n",
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["git-remote-credential", "get"])
            .env_clear()
            .env("AGENTSCM_REMOTE_CREDENTIAL_FILE", &file)
            .env(
                "AGENTSCM_REMOTE_CREDENTIAL_URL",
                "https://example.invalid/team/repo.git",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(&secret));
    }
}

#[test]
fn network_hidden_advertisement_never_becomes_an_absence_claim() {
    if isolated_test("network_tests::network_hidden_advertisement_never_becomes_an_absence_claim") {
        return;
    }
    let fixture = HttpsFixture::new(false);
    let (_root, engine, mut authority, _, mut mirror) = setup();
    mirror
        .remote_bind_network(fixture.endpoint(), "main")
        .unwrap();
    let access = fixture.access();
    let one = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let oid = mirror
        .export(&engine, &authority, &request(&one, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    assert_eq!(
        mirror
            .remote_run_with(
                &Request::Publish {
                    operation_id: "first".into(),
                    event_sequence: one.sequence,
                    expected_oid: None
                },
                &access
            )
            .unwrap()
            .state,
        State::Published
    );
    let two = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    mirror
        .export(&engine, &authority, &request(&two, Some(oid.clone())))
        .unwrap();
    run(
        &fixture.remote,
        &["config", "uploadpack.hideRefs", "refs/heads/main"],
    );
    let fetch = Request::Fetch {
        operation_id: "hidden-fetch".into(),
        expected_oid: oid.clone(),
    };
    assert_eq!(
        mirror.remote_run_with(&fetch, &access).unwrap().state,
        State::Unknown
    );
    assert_eq!(
        mirror
            .remote_recover_with("hidden-fetch", &access)
            .unwrap()
            .state,
        State::ObservedUnadvertised
    );
    let attempted = mirror
        .remote_run_with(
            &Request::Publish {
                operation_id: "create-guard".into(),
                event_sequence: two.sequence,
                expected_oid: None,
            },
            &access,
        )
        .unwrap();
    assert_eq!(attempted.state, State::Unknown);
    assert_eq!(run(&fixture.remote, &["rev-parse", "refs/heads/main"]), oid);
}

#[test]
fn local_profile_rejects_network_only_recovery_state() {
    if isolated_test("network_tests::local_profile_rejects_network_only_recovery_state") {
        return;
    }
    let (root, _, _, path, mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let request = Request::Fetch {
        operation_id: "uncertain".into(),
        expected_oid: "1".repeat(40),
    };
    let mut record = mirror.remote_run(&request).unwrap();
    assert_eq!(record.state, State::Unknown);
    record.state = State::ObservedUnadvertised;
    let db = rusqlite::Connection::open(path.join("agentscm-remote.sqlite")).unwrap();
    db.execute(
        "UPDATE operations SET record=?1 WHERE id='uncertain'",
        [serde_json::to_vec(&record).unwrap()],
    )
    .unwrap();
    assert!(mirror.remote_lookup("uncertain").is_err());
    assert!(mirror.remote_recover("uncertain").is_err());
    assert!(mirror.remote_run(&request).is_err());
    assert!(
        mirror
            .remote_run(&Request::Fetch {
                operation_id: "new".into(),
                expected_oid: "1".repeat(40),
            })
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM operations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(mirror.remote_observe().unwrap(), None);
}

#[test]
fn ssh_bind_rejects_expansion_paths_before_creating_a_journal() {
    if isolated_test("network_tests::ssh_bind_rejects_expansion_paths_before_creating_a_journal") {
        return;
    }
    let (root, _, _, mirror_path, _) = setup();
    // These need not exist: name refusal must precede reading trust/identity.
    for (key, known) in [
        ("key", "known%h"),
        ("key%h", "known"),
        ("key", "known${HOME}"),
        ("key${HOME}", "known"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--data-dir")
            .arg(root.path().join("data"))
            .arg("git-remote")
            .arg("--mirror")
            .arg(&mirror_path)
            .arg("--ssh-identity")
            .arg(root.path().join(key))
            .arg("--known-hosts")
            .arg(root.path().join(known))
            .args([
                "bind-network",
                "--url",
                "ssh://synthetic@synthetic.invalid/team/repo.git",
                "--branch",
                "main",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("expansion syntax"));
        assert!(!mirror_path.join("agentscm-remote.sqlite").exists());
    }
}

#[test]
fn https_uses_digest_verified_trust_snapshot_during_original_rotation() {
    if isolated_test(
        "network_tests::https_uses_digest_verified_trust_snapshot_during_original_rotation",
    ) {
        return;
    }
    let fixture = HttpsFixture::new(false);
    let (root, _, authority, _, _) = setup();
    let config = root.path().join("git-wrapper.json");
    let log = root.path().join("trust-paths");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "git":git(), "ca":fixture.ca, "digest":trust_digest(&fixture.ca).unwrap(), "log":log,
        }))
        .unwrap(),
    )
    .unwrap();
    let wrapper = root.path().join("selected-git");
    fs::write(
        &wrapper,
        format!(
            r#"#!/usr/bin/python3
import hashlib,json,os,sys
c=json.load(open({config:?}))
selected=next((a.split('=',1)[1] for a in sys.argv[1:] if a.startswith('http.sslCAInfo=')),None)
if selected is not None:
 assert selected!=c['ca']
 assert hashlib.sha256(open(selected,'rb').read()).hexdigest()==c['digest']
 assert os.stat(selected).st_mode & 0o777 == 0o600
 assert os.stat(os.path.dirname(selected)).st_mode & 0o777 == 0o700
 with open(c['ca']+'.new','w') as changed: changed.write('rotated invalid CA\n')
 os.replace(c['ca']+'.new',c['ca'])
 with open(c['log'],'a') as output: output.write(selected+'\n')
os.execve(c['git'],[c['git']]+sys.argv[1:],dict(os.environ))
"#,
            config = config.to_str().unwrap()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let mirror = Mirror::initialize(
        &wrapper,
        &root.path().join("snapshot-mirror.git"),
        &authority.repository_id().unwrap(),
        "main",
    )
    .unwrap();
    mirror
        .remote_bind_network(fixture.endpoint(), "main")
        .unwrap();
    assert_eq!(mirror.remote_observe_with(&fixture.access()).unwrap(), None);
    assert_eq!(
        fs::read_to_string(&fixture.ca).unwrap(),
        "rotated invalid CA\n"
    );
    let calls = fs::read_to_string(&log).unwrap();
    assert_eq!(calls.lines().count(), 1);
    for call in calls.lines() {
        let snapshot = Path::new(call);
        assert!(!snapshot.exists());
        assert!(!snapshot.parent().unwrap().exists());
    }
    assert!(mirror.remote_observe_with(&fixture.access()).is_err());
    assert_eq!(fs::read_to_string(&log).unwrap(), calls);
}
