//! Live hostile-boundary canaries for the OCI command collector.
//!
//! Each test runs a fixed hostile script through `OciBackend::collect` on the explicitly selected
//! local Docker runtime and image. A behavioural canary script exits 0 only when each hostile
//! action failed with the expected error; the test then expects `Success`. Evidence (outcome,
//! exit code, runtime identity, output) is printed so CI logs retain it. Passing qualifies only
//! this runtime, image and execution profile and these controls, not arbitrary hostile code.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use agentscm::checks::CheckSubject;
use agentscm::context::ChangeId;
use agentscm::execution::{
    Cancellation, CommandRecipe, ExecutionLimits, ExecutionOutcome, ExecutionRecord,
    ExecutionRequest, OciBackend, Snapshot, SyscallPolicy, environment_digest,
};
use agentscm::model::FileChange;
use agentscm::store::Engine;
use tempfile::TempDir;

const DIGEST_2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const DIGEST_3: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const OWNER_LABEL: &str = "io.agentscm.execution";
const IMAGE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const MIB: u64 = 1024 * 1024;

/// Shell prelude. `blocked PATTERN CMD...` fails the script unless CMD fails AND its output
/// matches the extended regex PATTERN (the specific errno text), so a missing tool or a typo
/// cannot count as "blocked". `need` requires the busybox applets a canary depends on.
/// `mopts MOUNTPOINT` prints the per-mount options from mountinfo; `mopt MOUNTPOINT OPT`
/// requires one option.
const PRELUDE: &str = r#"set -eu
B=/bin/busybox
blocked() {
  pat="$1"; shift
  if out="$("$@" 2>&1)"; then echo "NOT BLOCKED: $*"; exit 1; fi
  echo "$out" | $B grep -Eq "$pat" || { echo "WRONG FAILURE for $*: $out (want $pat)"; exit 1; }
  echo "blocked: $* -> $out"
}
need() { for a in "$@"; do $B --list | $B grep -qx "$a" || { echo "missing applet $a"; exit 90; }; done; }
mopts() { while read -r _ _ _ _ mp opts _; do if [ "$mp" = "$1" ]; then echo "$opts"; fi; done < /proc/self/mountinfo; }
mopt() { case ",$(mopts "$1")," in *",$2,"*) echo "mount $1 has $2";; *) echo "mount $1 lacks $2: $(mopts "$1")"; exit 1;; esac; }
"#;

// Mount-level protection and mode bits both deny writes, and which errno wins depends on the
// kernel path (Docker Desktop reports EROFS; on Linux the snapshot's 0555 modes can win).
const DENIED: &str = "Read-only file system|Permission denied";

const NETWORK: &str = r#"need wget nslookup ip grep
blocked 'Network is unreachable' $B wget -T 2 -q -O /dev/null http://1.1.1.1/
blocked 'Network is unreachable' $B nslookup -timeout=2 example.com 1.1.1.1
# Default resolver: the name must simply not resolve or connect.
blocked 'bad address|unreachable|can.t connect' $B wget -T 2 -q -O /dev/null http://example.com/
# Only loopback routes may exist (IPv6 loopback can appear); there must be no default or other route.
test -z "$($B ip route | $B grep -v ' dev lo' || true)"
test -z "$($B ip -6 route | $B grep -v ' dev lo' || true)"
# Docker Desktop's kernel lists DOWN tunnel devices (tunl0, sit0, ...), so test UP state, not presence.
$B ip -o link
up="$($B ip -o link | $B grep -v '^[0-9]*: lo[:@]' | $B grep -E '<([^>]*,)?UP(,[^>]*)?>' || true)"
test -z "$up" || { echo "interface up: $up"; exit 1; }
"#;

const FILESYSTEM: &str = r#"need touch dd rm cat mount grep
mopt / ro
mopt /source ro
mopt /scratch nosuid
mopt /scratch nodev
case ",$(mopts /scratch)," in *,noexec,*) echo "scratch is noexec";; *) echo "scratch allows exec";; esac
for p in /source/new /source/canary.txt /new /etc/new /usr/new /bin/new; do
  blocked "@DENIED@" $B touch "$p"
done
# /tmp is 1777: only the read-only root can stop this write.
blocked 'Read-only file system' $B touch /tmp/new
blocked "@DENIED@" $B rm /source/canary.txt
blocked "@DENIED@" sh -c 'echo x > /source/canary.txt'
blocked "@DENIED@" sh -c 'echo x >> /etc/passwd'
blocked 'ermission denied|not permitted' $B mount -o remount,rw /
test "$($B cat /source/canary.txt)" = immutable
echo ok > /scratch/ok
# 4 MiB scratch: writing 8 MiB must hit ENOSPC.
blocked 'No space left on device' $B dd if=/dev/zero of=/scratch/big bs=1M count=8
test "$($B cat /source/canary.txt)" = immutable
"#;

const PRIVILEGES: &str = r#"need id env find timeout mount chroot unshare cut sort touch
test "$($B id -u)" = 65532
test "$($B id -g)" = 65532
for f in CapInh CapPrm CapEff CapBnd CapAmb; do
  v="$($B grep "^$f:" /proc/self/status | $B cut -f2)"
  test "$v" = 0000000000000000 || { echo "$f=$v"; exit 1; }
done
$B grep -q '^NoNewPrivs:[[:space:]]1$' /proc/self/status
$B grep -q '^Seccomp:[[:space:]]2$' /proc/self/status
# Environment is an exact allowlist with the profile's values: nothing inherited from the host or CLI.
for k in $($B env | $B cut -d= -f1); do
  case "$k" in PATH|HOME|TMPDIR|HOSTNAME|PWD|SHLVL) ;; *) echo "unexpected env $k"; exit 1;; esac
done
test "$HOME" = /scratch
test "$TMPDIR" = /scratch
test "$PATH" = "@PATH@"
# Search for the daemon socket, state databases and authority directories (bounded). Decoys are the
# positive control: the search must find exactly them and nothing else.
$B touch /scratch/decoy.sqlite /scratch/docker.sock /scratch/.agentscm
rc=0
found="$($B timeout 8 $B find / -path /proc -prune -o -path /sys -prune -o \( -name docker.sock -o -name '*.sqlite' -o -name .agentscm \) -print 2>/dev/null | $B sort)" || rc=$?
test "$rc" != 143 || { echo "find timed out"; exit 1; }
want="$(printf '/scratch/.agentscm\n/scratch/decoy.sqlite\n/scratch/docker.sock')"
test "$found" = "$want" || { echo "found: $found"; exit 1; }
# Privilege escalation attempts.
$B mkdir /scratch/x
blocked 'ermission denied|not permitted' $B mount -t tmpfs none /scratch/x
blocked 'Operation not permitted' $B chroot / $B true
blocked 'Operation not permitted' $B unshare -U $B true
blocked 'Operation not permitted' $B unshare -m $B true
# File descriptor and core limits (nofile=128, core=0).
test "$(ulimit -n)" = 128
test "$(ulimit -c)" = 0
if msg="$( ( i=3; while [ $i -lt 200 ]; do eval "exec $i</source/canary.txt" || exit 7; i=$((i+1)); done ) 2>&1 )"; then
  echo "opened 200 descriptors"; exit 1
fi
echo "$msg" | $B grep -q 'Too many open files' || { echo "wrong fd failure: $msg"; exit 1; }
echo "descriptor limit enforced: $msg"
"#;

const PROCESSES: &str = r#"need sleep
i=0
# Fork failures are expected and ignored; each spawn runs in its own subshell so one cannot end the script.
while [ $i -lt 64 ]; do ( $B sleep 5 & ) 2>/dev/null || true; i=$((i+1)); done
# Counted with shell globbing so the check itself needs no fork.
set -- /proc/[0-9]*
echo "processes: $#"
test "$#" -le 16
test "$#" -ge 14
# A private cgroup namespace is part of the profile, so the limit must be readable.
test -r /sys/fs/cgroup/pids.max || { echo "cgroup pids.max not visible"; exit 1; }
read -r max < /sys/fs/cgroup/pids.max
echo "pids.max: $max"
test "$max" = 16
"#;

const BOUNDARIES: &str = r#"need cat ls grep head wc hostname date mknod unshare
# PID namespace: this shell is PID 1 and sees almost nothing else.
test "$$" = 1
set -- /proc/[0-9]*
echo "processes: $#"
test "$#" -le 3
# Every mount is read-only or one expected writable mount (Docker's /proc masks bind /dev/null).
echo "mount table (mountpoint root options):"
bad=""
while read -r _ _ _ root mp opts _; do
  echo "  $mp $root $opts"
  case ",$opts," in *,ro,*) continue;; esac
  case "$mp:$root" in
    /scratch:*|/dev:*|/dev/pts:*|/dev/mqueue:*|/proc:*|/proc/*:/null) ;;
    *) bad="$bad $mp";;
  esac
done < /proc/self/mountinfo
test -z "$bad" || { echo "unexpected writable mounts:$bad"; exit 1; }
mopt / ro
mopt /source ro
mopt /sys ro
# Devices: private /dev without shared memory, block devices or kernel interfaces.
$B ls -la /dev
test ! -e /dev/shm
test ! -e /dev/mem
test ! -e /dev/kmsg
test ! -e /dev/fuse
test -z "$($B ls -l /dev | $B grep '^b' || true)"
blocked 'Operation not permitted' $B mknod /scratch/n c 1 3
# Cgroup limits as the kernel sees them.
test "$($B cat /sys/fs/cgroup/memory.max)" = @MEMORY@
test "$($B cat /sys/fs/cgroup/memory.swap.max)" = 0
test "$($B cat /sys/fs/cgroup/cpu.max)" = "@CPU@ 100000"
test "$($B cat /sys/fs/cgroup/pids.max)" = @PIDS@
test "$($B cat /proc/self/cgroup)" = "0::/"
# Capability-gated host changes.
blocked 'Operation not permitted' $B hostname x
# busybox date reports EPERM but still exits 0, so check the message and that the clock is unchanged.
out="$($B date -s 2000-01-01 2>&1 || true)"
echo "$out" | $B grep -q 'Operation not permitted' || { echo "wrong date failure: $out"; exit 1; }
test "$($B date +%Y)" != 2000
echo "blocked: date -s -> $out"
for ns in n p i u; do blocked 'Operation not permitted' $B unshare -$ns $B true; done
# /proc masks are defence in depth behind uid 65532.
test "$($B head -c 16 /proc/kcore 2>/dev/null | $B wc -c)" = 0
blocked "@DENIED@" sh -c 'echo 1 > /proc/sysrq-trigger'
blocked "@DENIED@" sh -c 'echo x > /proc/sys/kernel/hostname'
"#;

fn limits() -> ExecutionLimits {
    ExecutionLimits {
        timeout_ms: 10_000,
        max_output_bytes: 16 * 1024,
        memory_bytes: 64 * MIB,
        scratch_bytes: 4 * MIB,
        pids: 16,
        cpu_millis: 1_000,
    }
}

/// Prelude plus body with the profile's values substituted.
fn script(body: &str) -> String {
    let limits = limits();
    format!("{PRELUDE}{body}")
        .replace("@PATH@", IMAGE_PATH)
        .replace("@DENIED@", DENIED)
        .replace("@MEMORY@", &limits.memory_bytes.to_string())
        .replace("@CPU@", &(u64::from(limits.cpu_millis) * 100).to_string())
        .replace("@PIDS@", &limits.pids.to_string())
}

/// Runs `/bin/sh -c script` under the standard profile adjusted by `tune`, prints the evidence and
/// checks that no container for this request remains and cleanup is confirmed.
fn run(name: &str, script: &str, tune: impl FnOnce(&mut ExecutionLimits)) -> ExecutionRecord {
    let executable = required_env("AGENTSCM_DOCKER");
    let socket = required_env("AGENTSCM_DOCKER_SOCKET");
    let image = std::env::var("AGENTSCM_OCI_IMAGE")
        .expect("set AGENTSCM_OCI_IMAGE to sha256:<64 lowercase hex>");
    let temporary = TempDir::new().expect("temporary fixture");
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
        .expect("private snapshot parent");
    let config = temporary.path().join("config");
    fs::create_dir(&config).expect("config directory");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).expect("private config");
    let backend = OciBackend::new(&executable, &socket, &config).expect("real backend");
    let engine = Engine::open(temporary.path().join("store")).expect("engine");
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "canary.txt".into(),
                content: Some(b"immutable\n".to_vec()),
            }],
        )
        .expect("revision");
    let mut request = request(
        &revision,
        image,
        vec!["/bin/sh".into(), "-c".into(), script.into()],
    );
    tune(&mut request.recipe.limits);
    request
        .recipe
        .environment
        .insert("PATH".into(), IMAGE_PATH.into());
    request.recipe.environment_sha256 =
        environment_digest(&request.recipe.environment).expect("environment digest");
    request.subject.environment_sha256 = request.recipe.environment_sha256.clone();
    request.subject.recipe_sha256 = request.recipe.digest().expect("recipe digest");
    let snapshot = Snapshot::materialize(
        &engine,
        &revision,
        &temporary.path().join("snapshot"),
        32,
        MIB,
    )
    .expect("snapshot");

    let started = Instant::now();
    let record = backend
        .collect(&request, &snapshot, &Cancellation::default())
        .expect("real collection");
    let elapsed = started.elapsed();
    eprintln!(
        "canary {name}: outcome={:?} exit_code={:?} elapsed={elapsed:?} truncated={}\n  \
         runtime_identity={:?}\n  detail={}\n  stdout={}\n  stderr={}",
        record.outcome,
        record.exit_code,
        record.output_truncated,
        record.runtime_identity,
        record.detail,
        evidence(&record.stdout, record.output_truncated),
        evidence(&record.stderr, record.output_truncated),
    );
    // Scan first: a leftover container must be reported (and removed) before any other assert.
    assert_no_leftover_container(&executable, &socket, &config, &record.request_sha256);
    assert!(record.cleanup_confirmed, "{}", brief(&record));
    assert!(record.cleanup_handle.is_none(), "{}", brief(&record));
    record
}

/// Lossy text; a truncated flood is reported by size only so it does not drown the log.
fn evidence(bytes: &[u8], truncated: bool) -> String {
    if truncated {
        return format!("[{} bytes, truncated flood omitted]", bytes.len());
    }
    let text = String::from_utf8_lossy(bytes);
    match text.char_indices().nth(8192) {
        Some((end, _)) => format!("{}... [{} bytes total]", &text[..end], bytes.len()),
        None => text.into_owned(),
    }
}

fn docker(executable: &Path, socket: &Path, config: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("DOCKER_CONFIG", config)
        .arg("--host")
        .arg(format!("unix://{}", socket.display()));
    command
}

fn assert_no_leftover_container(
    executable: &Path,
    socket: &Path,
    config: &Path,
    request_sha256: &str,
) {
    let output = docker(executable, socket, config)
        .args(["ps", "-a", "--no-trunc", "--filter"])
        .arg(format!("label={OWNER_LABEL}"))
        .args([
            "--format",
            &format!("{{{{.ID}}}} {{{{.Label \"{OWNER_LABEL}\"}}}}"),
        ])
        .output()
        .expect("docker ps");
    assert!(output.status.success(), "docker ps failed: {output:?}");
    let listing = String::from_utf8_lossy(&output.stdout);
    let leftover: Vec<_> = listing
        .lines()
        .filter(|line| {
            line.split(' ')
                .nth(1)
                .is_some_and(|v| v.starts_with(request_sha256))
        })
        .collect();
    if !leftover.is_empty() {
        for line in &leftover {
            let id = line.split(' ').next().unwrap_or_default();
            // Best effort so a failing canary does not strand a container.
            let _ = docker(executable, socket, config)
                .args(["rm", "--force", id])
                .output();
        }
        panic!("container remains: {leftover:?}");
    }
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn network_egress_is_blocked() {
    let record = run("network_egress_is_blocked", &script(NETWORK), |_| {});
    assert_success(&record);
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn filesystem_is_read_only_except_bounded_scratch() {
    let record = run(
        "filesystem_is_read_only_except_bounded_scratch",
        &script(FILESYSTEM),
        |_| {},
    );
    assert_success(&record);
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn no_privileges_secrets_or_host_sockets() {
    let record = run(
        "no_privileges_secrets_or_host_sockets",
        &script(PRIVILEGES),
        |_| {},
    );
    assert_success(&record);
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn process_count_is_bounded() {
    let record = run("process_count_is_bounded", &script(PROCESSES), |_| {});
    assert_success(&record);
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn devices_mounts_namespaces_and_cgroups() {
    let record = run(
        "devices_mounts_namespaces_and_cgroups",
        &script(BOUNDARIES),
        |_| {},
    );
    assert_success(&record);
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn memory_exhaustion_is_killed_not_success() {
    // `tail` buffers the newline-free stream, so 256 MiB must be held against a 64 MiB limit.
    let script = "/bin/busybox head -c 268435456 /dev/zero | /bin/busybox tail > /dev/null";
    let record = run("memory_exhaustion_is_killed_not_success", script, |l| {
        l.memory_bytes = 64 * MIB;
    });
    // The memory limit kills the pipeline's last stage with SIGKILL, so the shell exits 137. That is
    // incomplete whether or not Docker's asynchronous OOMKilled flag was set.
    assert_eq!(
        record.outcome,
        ExecutionOutcome::Incomplete,
        "{}",
        brief(&record)
    );
    assert_eq!(record.exit_code, Some(137), "{}", brief(&record));
    assert!(
        String::from_utf8_lossy(&record.stderr).contains("Killed"),
        "{}",
        brief(&record)
    );
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn output_flood_stops_at_limit() {
    let record = run("output_flood_stops_at_limit", "/bin/busybox yes", |l| {
        l.max_output_bytes = 4096;
    });
    assert_eq!(
        record.outcome,
        ExecutionOutcome::OutputLimit,
        "{}",
        brief(&record)
    );
    assert!(record.output_truncated, "{}", brief(&record));
    assert!(
        record.stdout.len() + record.stderr.len() <= 4096,
        "{}",
        brief(&record)
    );
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn timeout_kills_the_whole_process_tree() {
    let script = "/bin/busybox sleep 300 & /bin/busybox sleep 300 & /bin/busybox sleep 300";
    let started = Instant::now();
    let record = run("timeout_kills_the_whole_process_tree", script, |l| {
        l.timeout_ms = 2_000;
    });
    assert_eq!(
        record.outcome,
        ExecutionOutcome::TimedOut,
        "{}",
        brief(&record)
    );
    // `run` has already shown the container (and so every descendant) is gone.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{}",
        brief(&record)
    );
}

/// Debug of a record includes raw byte vectors, so failures show a readable summary instead.
fn brief(record: &ExecutionRecord) -> String {
    format!(
        "outcome={:?} exit_code={:?} detail={} stdout={} stderr={}",
        record.outcome,
        record.exit_code,
        record.detail,
        evidence(&record.stdout, record.output_truncated),
        evidence(&record.stderr, record.output_truncated),
    )
}

fn assert_success(record: &ExecutionRecord) {
    assert_eq!(
        record.outcome,
        ExecutionOutcome::Success,
        "{}",
        brief(record)
    );
    assert_eq!(record.exit_code, Some(0), "{}", brief(record));
}

fn request(revision: &str, image: String, argv: Vec<String>) -> ExecutionRequest {
    let environment = BTreeMap::new();
    let environment_sha256 = environment_digest(&environment).expect("environment digest");
    let recipe = CommandRecipe {
        version: 1,
        image,
        argv,
        environment,
        toolchain_sha256: DIGEST_2.into(),
        environment_sha256: environment_sha256.clone(),
        limits: limits(),
        syscall_policy: SyscallPolicy::DockerBuiltin,
        no_new_privileges: true,
    };
    let recipe_sha256 = recipe.digest().expect("recipe digest");
    ExecutionRequest {
        attempt_id: "execution-canary".into(),
        subject: CheckSubject {
            repository: "repository".into(),
            change: ChangeId {
                repository: "repository".into(),
                id: "change".into(),
            },
            change_version: 1,
            context_digest: DIGEST_3.into(),
            revision: revision.into(),
            recipe_sha256,
            toolchain_sha256: DIGEST_2.into(),
            environment_sha256,
        },
        run_generation: 1,
        recipe,
    }
}

fn required_env(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")));
    assert!(path.is_absolute(), "{name} must be absolute");
    path
}
