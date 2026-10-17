use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::process::{Capture, Stop, capture};
use super::{CleanupHandle, ExecutionOutcome, ExecutionRecord, ExecutionRequest, Snapshot};
use crate::{Error, Result};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
// A cold first create unpacks image layers, which can take longer than other control calls.
const CREATE_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_OUTPUT: u64 = 256 * 1024;
const SIGKILL_EXIT: i32 = 128 + 9;
const OWNER_LABEL: &str = "io.agentscm.execution";

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeProbe {
    pub available: bool,
    pub identity: Option<String>,
    pub detail: String,
}

/// Trusted local operator configuration. No environment/context/remote Docker lookup.
/// A responsive daemon and matching configuration do not qualify hostile isolation.
pub struct OciBackend {
    executable: PathBuf,
    endpoint: String,
    config_dir: PathBuf,
}

impl OciBackend {
    /// `config_dir` must be an existing empty private directory, outside source.
    /// Docker receives this instead of the operator's home/config and credential helpers.
    pub fn new(executable: &Path, socket: &Path, config_dir: &Path) -> Result<Self> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        if !executable.is_absolute() || !socket.is_absolute() || !config_dir.is_absolute() {
            return Err(Error::Invalid("OCI paths must be absolute".into()));
        }
        let executable = fs::canonicalize(executable)?;
        if !fs::metadata(&executable)?.is_file() {
            return Err(Error::Invalid(
                "Docker executable is not a regular file".into(),
            ));
        }
        let socket = fs::canonicalize(socket)?;
        if !fs::metadata(&socket)?.file_type().is_socket() {
            return Err(Error::Invalid(
                "Docker endpoint is not a local Unix socket".into(),
            ));
        }
        let config_dir = fs::canonicalize(config_dir)?;
        if !fs::metadata(&config_dir)?.is_dir()
            || fs::read_dir(&config_dir)?.next().is_some()
            || fs::metadata(&config_dir)?.permissions().mode() & 0o077 != 0
        {
            return Err(Error::Invalid(
                "Docker config directory must be empty and private".into(),
            ));
        }
        let socket = socket
            .to_str()
            .ok_or_else(|| Error::Invalid("Docker socket must be UTF-8".into()))?;
        Ok(Self {
            executable,
            endpoint: format!("unix://{socket}"),
            config_dir,
        })
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .env("HOME", &self.config_dir)
            .env("PATH", "/usr/bin:/bin");
        command
            .arg("--host")
            .arg(&self.endpoint)
            .arg("--config")
            .arg(&self.config_dir);
        command.args(args).current_dir(&self.config_dir);
        command
    }

    fn control(&self, args: &[String]) -> std::io::Result<Capture> {
        self.control_with(args, &Cancellation::default())
    }

    fn control_with(&self, args: &[String], cancel: &Cancellation) -> std::io::Result<Capture> {
        self.control_within(args, CONTROL_TIMEOUT, cancel)
    }

    fn control_within(
        &self,
        args: &[String],
        timeout: Duration,
        cancel: &Cancellation,
    ) -> std::io::Result<Capture> {
        capture(&mut self.command(args), timeout, CONTROL_OUTPUT, cancel)
    }

    fn object(&self, args: &[String]) -> std::result::Result<Value, String> {
        self.object_with(args, &Cancellation::default())
    }

    fn object_with(
        &self,
        args: &[String],
        cancel: &Cancellation,
    ) -> std::result::Result<Value, String> {
        let result = self
            .control_with(args, cancel)
            .map_err(|_| "runtime control could not start".to_owned())?;
        if result.stop.is_some() || result.code != Some(0) {
            return Err("runtime control failed, timed out or exceeded its output bound".into());
        }
        serde_json::from_slice(&result.stdout).map_err(|_| "runtime returned invalid JSON".into())
    }

    pub fn probe(&self) -> RuntimeProbe {
        self.probe_with(&Cancellation::default())
    }

    fn probe_with(&self, cancel: &Cancellation) -> RuntimeProbe {
        let result = self.object_with(&strings(&["info", "--format", "{{json .}}"]), cancel);
        match result.and_then(|info| {
            if info["OSType"] != "linux"
                || info["CgroupVersion"] != "2"
                || !info["Runtimes"]
                    .as_object()
                    .is_some_and(|runtimes| runtimes.contains_key("runc"))
                || [
                    "MemoryLimit",
                    "SwapLimit",
                    "CpuCfsPeriod",
                    "CpuCfsQuota",
                    "PidsLimit",
                ]
                .iter()
                .any(|field| info[*field] != true)
                || !info["SecurityOptions"].as_array().is_some_and(|options| {
                    options.iter().any(|v| {
                        v.as_str()
                            .is_some_and(|s| s == "name=seccomp,profile=builtin")
                    })
                })
            {
                return Err("requires a Linux daemon, cgroup v2 and seccomp support".into());
            }
            let version = info["ServerVersion"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .ok_or("runtime version unavailable")?;
            let runtime = self.object_with(
                &strings(&["version", "--format", "{{json .Server}}"]),
                cancel,
            )?;
            if runtime["Version"] != version
                || runtime["Os"] != "linux"
                || runtime["GitCommit"].as_str().is_none_or(str::is_empty)
            {
                return Err("runtime build identity unavailable or changed during probe".into());
            }
            let daemon_id = info["ID"].as_str().filter(|id| !id.is_empty() && id.len() <= 128).ok_or("daemon identity unavailable")?;
            let stable = json!({"daemon":daemon_id,"kernel":info["KernelVersion"],"arch":info["Architecture"],
                "cgroup_version":info["CgroupVersion"],"cgroup_driver":info["CgroupDriver"],
                "security":info["SecurityOptions"],"runtime":info["Runtimes"]["runc"],"server":runtime});
            Ok(format!("docker:{version}:runc:builtin:{}", hex(&serde_json::to_vec(&stable).expect("JSON value"))))
        }) {
            Ok(identity) => RuntimeProbe {
                available: true,
                identity: Some(identity),
                detail: "configuration observed; hostile runtime canaries remain separate".into(),
            },
            Err(detail) => RuntimeProbe {
                available: false,
                identity: None,
                detail,
            },
        }
    }

    /// Caller must claim a durable running generation first and CAS this record
    /// back against that exact generation. A late result grants no authority.
    pub fn collect(
        &self,
        request: &ExecutionRequest,
        snapshot: &Snapshot,
        cancel: &Cancellation,
    ) -> Result<ExecutionRecord> {
        self.collect_with(request, snapshot, cancel, &mut |_| Ok(()))
    }

    /// Like `collect`, but `persist` durably stores the cleanup handle before the container
    /// can exist and again once its ID is known. A failed persist starts nothing.
    pub fn collect_with(
        &self,
        request: &ExecutionRequest,
        snapshot: &Snapshot,
        cancel: &Cancellation,
        persist: &mut dyn FnMut(&CleanupHandle) -> Result<()>,
    ) -> Result<ExecutionRecord> {
        request.validate()?;
        if snapshot.revision() != request.subject.revision {
            return Err(Error::Conflict(
                "source snapshot does not match execution subject".into(),
            ));
        }
        let mut record = ExecutionRecord {
            version: 1,
            request: request.clone(),
            request_sha256: request.digest()?,
            outcome: ExecutionOutcome::Unavailable,
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            output_truncated: false,
            cleanup_confirmed: true,
            cleanup_handle: None,
            isolation_observed: false,
            runtime_identity: None,
            detail: String::new(),
        };
        if cancel.is_cancelled() {
            record.outcome = ExecutionOutcome::Cancelled;
            record.detail = "cancelled before runtime acquisition".into();
            return Ok(record);
        }
        let probe = self.probe_with(cancel);
        record.runtime_identity = probe.identity;
        if cancel.is_cancelled() {
            record.outcome = ExecutionOutcome::Cancelled;
            record.detail = "cancelled during runtime acquisition".into();
            return Ok(record);
        }
        if !probe.available {
            record.detail = probe.detail;
            return Ok(record);
        }
        let image = self.object_with(
            &strings(&[
                "image",
                "inspect",
                "--format",
                "{{json .}}",
                &request.recipe.image,
            ]),
            cancel,
        );
        if cancel.is_cancelled() {
            record.outcome = ExecutionOutcome::Cancelled;
            record.detail = "cancelled during image acquisition".into();
            return Ok(record);
        }
        if !image
            .as_ref()
            .is_ok_and(|image| image_allowed(image, request))
        {
            record.detail =
                "local pinned image missing or declares unsupported volumes/environment/platform"
                    .into();
            return Ok(record);
        }
        // OS entropy makes each physical container a unique incarnation, even for
        // identical attempts. Never target a reusable user-controlled container name.
        let mut nonce = [0u8; 16];
        fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        let name = format!("agentscm-{}", hex(&nonce));
        let owner = format!("{}:{}", record.request_sha256, hex(&nonce));
        record.cleanup_confirmed = false;
        record.cleanup_handle = Some(CleanupHandle {
            container_name: name.clone(),
            owner_label: owner.clone(),
            creation_confirmed: false,
            runtime_identity: record
                .runtime_identity
                .clone()
                .expect("available runtime identity"),
            container_id: None,
        });
        persist(record.cleanup_handle.as_ref().expect("created handle"))?;
        let create = self.control_within(
            &create_args(request, snapshot.path(), &name, &owner),
            CREATE_TIMEOUT,
            cancel,
        );
        if let Ok(result) = &create
            && result.stop.is_none()
            && result.code == Some(0)
            && let Ok(id) = std::str::from_utf8(&result.stdout)
            && valid_container_id(id.trim())
        {
            let handle = record.cleanup_handle.as_mut().expect("created handle");
            handle.container_id = Some(id.trim().to_owned());
            handle.creation_confirmed = true;
        }
        let created = create.as_ref().is_ok_and(|result| {
            result.stop.is_none() && result.code == Some(0) && result.stderr.is_empty()
        });
        // Reconcile even a lost create response, but never start after an uncertain response.
        let observed = self.inspect(&name);
        let owned = observed.as_ref().is_ok_and(|value| owns(value, &owner));
        let observed_id = observed.as_ref().ok().and_then(container_id);
        let handle = record.cleanup_handle.as_mut().expect("created handle");
        let same_incarnation = owned
            && observed_id
                .as_ref()
                .is_some_and(|id| handle.container_id.as_ref().is_none_or(|known| known == id));
        if same_incarnation {
            handle.container_id = observed_id;
            handle.creation_confirmed = true;
        }
        let id = handle.container_id.clone();
        let persisted = persist(handle).is_ok();
        if created
            && same_incarnation
            && persisted
            && observed
                .as_ref()
                .is_ok_and(|value| configuration_allowed(value, request, snapshot.path()))
            && !cancel.is_cancelled()
            && self.probe_with(cancel).identity == record.runtime_identity
            && !cancel.is_cancelled()
        {
            record.isolation_observed = true;
            let attached = capture(
                &mut self.command(&strings(&[
                    "container",
                    "start",
                    "--attach",
                    id.as_deref().expect("confirmed container ID"),
                ])),
                Duration::from_millis(request.recipe.limits.timeout_ms),
                request.recipe.limits.max_output_bytes,
                cancel,
            );
            match attached {
                Ok(result) => {
                    record.stdout = result.stdout;
                    record.stderr = result.stderr;
                    record.output_truncated = result.stop == Some(Stop::Output);
                    record.outcome = match result.stop {
                        Some(Stop::Timeout) => ExecutionOutcome::TimedOut,
                        Some(Stop::Cancelled) => ExecutionOutcome::Cancelled,
                        Some(Stop::Output) => ExecutionOutcome::OutputLimit,
                        Some(Stop::Io) => ExecutionOutcome::Incomplete,
                        None => self.completed_outcome(
                            id.as_deref().expect("confirmed container ID"),
                            &owner,
                            result.code,
                            &mut record,
                        ),
                    };
                }
                Err(_) => record.outcome = ExecutionOutcome::Incomplete,
            }
            if matches!(
                record.outcome,
                ExecutionOutcome::Success | ExecutionOutcome::Failed
            ) && self.probe_with(cancel).identity != record.runtime_identity
            {
                record.outcome = if cancel.is_cancelled() {
                    ExecutionOutcome::Cancelled
                } else {
                    ExecutionOutcome::Incomplete
                };
            }
            record.detail = "controller observed execution; admission and live isolation qualification are separate".into();
        } else {
            record.outcome = if cancel.is_cancelled() {
                ExecutionOutcome::Cancelled
            } else {
                ExecutionOutcome::Incomplete
            };
            record.detail = "container creation or effective configuration was not confirmed; candidate not started".into();
        }
        let handle = record.cleanup_handle.as_mut().expect("created handle");
        let known = handle.container_id.is_some();
        record.cleanup_confirmed = self.reconcile(request, handle)?;
        // A create that completed after its reply was lost is discovered by its owned name. Persist
        // the learned ID, as reconcile requires, and only then remove that incarnation by ID.
        if !record.cleanup_confirmed
            && !known
            && handle.container_id.is_some()
            && persist(handle).is_ok()
        {
            record.cleanup_confirmed = self.reconcile(request, handle)?;
        }
        if record.cleanup_confirmed {
            record.cleanup_handle = None;
        }
        if !record.cleanup_confirmed {
            // Best effort: the record still carries the latest handle.
            let _ = persist(record.cleanup_handle.as_ref().expect("unconfirmed handle"));
            record.outcome = ExecutionOutcome::Indeterminate;
            record.detail =
                format!("cleanup unconfirmed; reconcile owned container {name} before recovery");
        }
        Ok(record)
    }

    /// Reconcile only a handle retained by the trusted controller. The handle
    /// does not authenticate imported records. Unknown create + absence remains
    /// uncertain because an unanswered create may still complete later. If an ID
    /// is discovered, this updates the handle and returns false without removal.
    /// Persist that handle before calling again.
    pub fn reconcile(
        &self,
        request: &ExecutionRequest,
        handle: &mut CleanupHandle,
    ) -> Result<bool> {
        request.validate()?;
        handle.validate(request)?;
        if self.probe().identity.as_ref() != Some(&handle.runtime_identity) {
            return Ok(false);
        }
        let id = match handle.container_id.clone() {
            Some(id) => id,
            None => {
                let discovered = self
                    .inspect(&handle.container_name)
                    .ok()
                    .filter(|value| owns(value, &handle.owner_label))
                    .and_then(|value| container_id(&value));
                if let Some(id) = discovered
                    && self.probe().identity.as_ref() == Some(&handle.runtime_identity)
                {
                    handle.container_id = Some(id);
                    handle.creation_confirmed = true;
                }
                // The caller must persist a learned ID before the next call can
                // destroy that incarnation. Never discard a discovered identity
                // after an uncertain removal or controller crash.
                return Ok(false);
            }
        };
        let removed = self.remove_owned(&id, &handle.owner_label);
        Ok(removed && self.probe().identity.as_ref() == Some(&handle.runtime_identity))
    }

    fn inspect(&self, name: &str) -> std::result::Result<Value, String> {
        self.object(&strings(&[
            "container",
            "inspect",
            "--format",
            "{{json .}}",
            name,
        ]))
    }

    fn completed_outcome(
        &self,
        name: &str,
        owner: &str,
        attached_exit: Option<i32>,
        record: &mut ExecutionRecord,
    ) -> ExecutionOutcome {
        let Ok(value) = self.inspect(name) else {
            return ExecutionOutcome::Incomplete;
        };
        if !owns(&value, owner)
            || value["Id"] != name
            || value["State"]["Running"] != false
            || value["State"]["Status"] != "exited"
            || value["State"]["Error"] != ""
        {
            return ExecutionOutcome::Incomplete;
        }
        record.exit_code = value["State"]["ExitCode"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok());
        if record.exit_code != attached_exit || value["State"]["OOMKilled"] != false {
            return ExecutionOutcome::Incomplete;
        }
        match record.exit_code {
            Some(0) => ExecutionOutcome::Success,
            // SIGKILL (128 + 9): when the memory limit kills a child process, the shell exits 137
            // and Docker's asynchronous OOMKilled flag may still be false, so never call it a failure.
            Some(SIGKILL_EXIT) => ExecutionOutcome::Incomplete,
            Some(_) => ExecutionOutcome::Failed,
            None => ExecutionOutcome::Incomplete,
        }
    }

    fn remove_owned(&self, name: &str, owner: &str) -> bool {
        if !self
            .inspect(name)
            .as_ref()
            .is_ok_and(|value| owns(value, owner) && value["Id"] == name)
        {
            return self.confirm_absent(name);
        }
        let removed = self.control(&strings(&["container", "rm", "--force", "--volumes", name]));
        removed.is_ok_and(|result| result.code == Some(0) && result.stop.is_none())
            && self.confirm_absent(name)
    }

    fn confirm_absent(&self, name: &str) -> bool {
        // A daemon/transport failure is never interpreted as absence.
        self.control(&strings(&[
            "container",
            "inspect",
            "--format",
            "{{json .}}",
            name,
        ]))
        .is_ok_and(|result| {
            let stderr = String::from_utf8_lossy(&result.stderr);
            // Older CLIs say "Error:"; Docker 28 forwards the daemon's "Error response from daemon:".
            result.code != Some(0)
                && result.stop.is_none()
                && ["Error", "Error response from daemon"]
                    .iter()
                    .any(|prefix| stderr.trim() == format!("{prefix}: No such container: {name}"))
        })
    }
}

fn environment(request: &ExecutionRequest) -> BTreeMap<String, String> {
    let mut env = request.recipe.environment.clone();
    env.insert("HOME".into(), "/scratch".into());
    env.insert("TMPDIR".into(), "/scratch".into());
    env
}

fn image_allowed(value: &Value, request: &ExecutionRequest) -> bool {
    value["Id"] == request.recipe.image
        && value["Os"] == "linux"
        && (value["Config"]["Volumes"].is_null()
            || value["Config"]["Volumes"]
                .as_object()
                .is_some_and(|v| v.is_empty()))
        && parsed_env(&value["Config"]["Env"]).is_some_and(|env| {
            env.iter()
                .all(|(key, value)| environment(request).get(key) == Some(value))
        })
}

fn create_args(request: &ExecutionRequest, source: &Path, name: &str, owner: &str) -> Vec<String> {
    let limits = &request.recipe.limits;
    let mut args = strings(&[
        "container",
        "create",
        "--pull=never",
        "--attach",
        "stdout",
        "--attach",
        "stderr",
        "--name",
        name,
        "--label",
        &format!("{OWNER_LABEL}={owner}"),
        "--network",
        "none",
        "--user",
        "65532:65532",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges=true",
        "--security-opt",
        "seccomp=builtin",
        "--pids-limit",
        &limits.pids.to_string(),
        "--memory",
        &limits.memory_bytes.to_string(),
        "--memory-swap",
        &limits.memory_bytes.to_string(),
        "--cpu-period",
        "100000",
        "--cpu-quota",
        &(u64::from(limits.cpu_millis) * 100).to_string(),
        "--ipc",
        "none",
        "--cgroupns",
        "private",
        "--runtime",
        "runc",
        "--log-driver",
        "none",
        "--no-healthcheck",
        "--ulimit",
        "nofile=128:128",
        "--ulimit",
        "core=0:0",
        "--workdir",
        "/source",
        "--mount",
        &format!(
            "type=bind,src={},dst=/source,readonly,bind-recursive=disabled,bind-propagation=rprivate",
            source.display()
        ),
        "--tmpfs",
        &format!(
            "/scratch:rw,nosuid,nodev,size={},mode=1777",
            limits.scratch_bytes
        ),
        "--entrypoint",
        &request.recipe.argv[0],
    ]);
    for (key, value) in environment(request) {
        args.extend(strings(&["--env", &format!("{key}={value}")]));
    }
    args.push(request.recipe.image.clone());
    args.extend_from_slice(&request.recipe.argv[1..]);
    args
}

fn valid_container_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn container_id(value: &Value) -> Option<String> {
    value["Id"]
        .as_str()
        .filter(|id| valid_container_id(id))
        .map(str::to_owned)
}

fn owns(value: &Value, owner: &str) -> bool {
    value["Config"]["Labels"][OWNER_LABEL] == owner
}

fn configuration_allowed(value: &Value, request: &ExecutionRequest, source: &Path) -> bool {
    let host = &value["HostConfig"];
    let config = &value["Config"];
    let limits = &request.recipe.limits;
    let empty = |v: &Value| {
        v.is_null()
            || v.as_array().is_some_and(Vec::is_empty)
            || v.as_object().is_some_and(serde_json::Map::is_empty)
    };
    let security = host["SecurityOpt"].as_array();
    let mounts = value["Mounts"].as_array();
    value["Image"] == request.recipe.image
        && value["State"]["Status"] == "created"
        && config["User"] == "65532:65532"
        && config["WorkingDir"] == "/source"
        && config["Tty"] == false
        && config["OpenStdin"] == false
        && config["AttachStdout"] == true
        && config["AttachStderr"] == true
        && config["Healthcheck"]["Test"] == json!(["NONE"])
        && config["Entrypoint"] == json!([request.recipe.argv[0]])
        && (config["Cmd"] == json!(&request.recipe.argv[1..])
            || (request.recipe.argv.len() == 1 && config["Cmd"].is_null()))
        && parsed_env(&config["Env"]) == Some(environment(request))
        && host["NetworkMode"] == "none"
        && host["ReadonlyRootfs"] == true
        && host["Privileged"] == false
        && host["CapDrop"] == json!(["ALL"])
        && empty(&host["CapAdd"])
        && security.is_some_and(|s| {
            s.len() == 2
                && s.contains(&json!("no-new-privileges=true"))
                && s.contains(&json!("seccomp=builtin"))
        })
        && host["Memory"] == limits.memory_bytes
        && host["MemorySwap"] == limits.memory_bytes
        && host["PidsLimit"] == limits.pids
        && host["CpuPeriod"] == 100000
        && host["CpuQuota"] == u64::from(limits.cpu_millis) * 100
        && host["Runtime"] == "runc"
        && host["IpcMode"] == "none"
        && host["CgroupnsMode"] == "private"
        && host["PidMode"] == ""
        && host["UTSMode"] == ""
        && empty(&host["Devices"])
        && empty(&host["DeviceRequests"])
        && empty(&host["VolumesFrom"])
        && empty(&host["PortBindings"])
        && host["PublishAllPorts"] == false
        && host["LogConfig"]["Type"] == "none"
        && host["Ulimits"].as_array().is_some_and(|ulimits| {
            ulimits.len() == 2
                && ulimits
                    .iter()
                    .any(|u| u["Name"] == "nofile" && u["Soft"] == 128 && u["Hard"] == 128)
                && ulimits
                    .iter()
                    .any(|u| u["Name"] == "core" && u["Soft"] == 0 && u["Hard"] == 0)
        })
        && host["Mounts"].as_array().is_some_and(|mounts| {
            mounts.len() == 1
                && mounts[0]["Type"] == "bind"
                && mounts[0]["Source"].as_str() == source.to_str()
                && mounts[0]["Target"] == "/source"
                && mounts[0]["ReadOnly"] == true
                && mounts[0]["BindOptions"]["NonRecursive"] == true
                && mounts[0]["BindOptions"]["Propagation"] == "rprivate"
        })
        && host["Tmpfs"]
            == json!({"/scratch": format!("rw,nosuid,nodev,size={},mode=1777", limits.scratch_bytes)})
        && mounts.is_some_and(|mounts| {
            let binds: Vec<_> = mounts.iter().filter(|m| m["Type"] == "bind").collect();
            binds.len() == 1
                && binds[0]["Source"].as_str() == source.to_str()
                && binds[0]["Destination"] == "/source"
                && binds[0]["RW"] == false
                && binds[0]["Propagation"] == "rprivate"
                && mounts.iter().all(|m| {
                    m["Type"] == "bind"
                        || (m["Type"] == "tmpfs"
                            && m["Destination"] == "/scratch"
                            && m["RW"] == true)
                })
                && mounts.len() <= 2
        })
}

fn parsed_env(value: &Value) -> Option<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    if value.is_null() {
        return Some(result);
    }
    for entry in value.as_array()? {
        let (key, value) = entry.as_str()?.split_once('=')?;
        if result.insert(key.into(), value.into()).is_some() {
            return None;
        }
    }
    Some(result)
}
fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}
fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckSubject;
    use crate::context::ChangeId;
    use crate::execution::{CommandRecipe, ExecutionLimits, SyscallPolicy, environment_digest};

    fn request() -> ExecutionRequest {
        let env = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
        let recipe = CommandRecipe {
            version: 1,
            image: format!("sha256:{}", "a".repeat(64)),
            argv: vec!["/bin/true".into()],
            environment_sha256: environment_digest(&env).unwrap(),
            environment: env,
            toolchain_sha256: "b".repeat(64),
            limits: ExecutionLimits {
                timeout_ms: 1000,
                max_output_bytes: 1024,
                memory_bytes: 64 * 1024 * 1024,
                scratch_bytes: 1024 * 1024,
                pids: 16,
                cpu_millis: 1000,
            },
            syscall_policy: SyscallPolicy::DockerBuiltin,
            no_new_privileges: true,
        };
        ExecutionRequest {
            attempt_id: "attempt-1".into(),
            run_generation: 1,
            subject: CheckSubject {
                repository: "repo".into(),
                change: ChangeId {
                    repository: "repo".into(),
                    id: "change".into(),
                },
                change_version: 1,
                context_digest: "c".repeat(64),
                revision: "d".repeat(64),
                recipe_sha256: recipe.digest().unwrap(),
                toolchain_sha256: recipe.toolchain_sha256.clone(),
                environment_sha256: recipe.environment_sha256.clone(),
            },
            recipe,
        }
    }

    fn inspected(request: &ExecutionRequest) -> Value {
        let r = &request.recipe;
        json!({"Image":r.image, "State":{"Status":"created"},
            "Config":{"User":"65532:65532", "WorkingDir":"/source", "Tty":false, "OpenStdin":false,"AttachStdout":true,"AttachStderr":true,
                "Entrypoint":[r.argv[0]], "Cmd":null, "Healthcheck":{"Test":["NONE"]}, "Env":environment(request).iter().map(|(k,v)| format!("{k}={v}")).collect::<Vec<_>>()},
            "HostConfig":{"NetworkMode":"none", "ReadonlyRootfs":true,"Privileged":false,"CapDrop":["ALL"],"CapAdd":null,
                "SecurityOpt":["no-new-privileges=true","seccomp=builtin"], "Memory":r.limits.memory_bytes,"MemorySwap":r.limits.memory_bytes,
                "PidsLimit":r.limits.pids,"CpuPeriod":100000,"CpuQuota":u64::from(r.limits.cpu_millis)*100,
                "Runtime":"runc","IpcMode":"none","CgroupnsMode":"private","PidMode":"","UTSMode":"","Devices":[],"DeviceRequests":null,"VolumesFrom":[],
                "PortBindings":{},"PublishAllPorts":false,"LogConfig":{"Type":"none"},
                "Tmpfs":{"/scratch":format!("rw,nosuid,nodev,size={},mode=1777",r.limits.scratch_bytes)},
                "Ulimits":[{"Name":"nofile","Soft":128,"Hard":128},{"Name":"core","Soft":0,"Hard":0}],
                "Mounts":[{"Type":"bind","Source":"/private/source","Target":"/source","ReadOnly":true,"BindOptions":{"NonRecursive":true,"Propagation":"rprivate"}}]},
            "Mounts":[{"Type":"bind","Source":"/private/source","Destination":"/source","RW":false,"Propagation":"rprivate"}]})
    }

    #[test]
    fn effective_configuration_rejects_each_weakened_boundary() {
        let request = request();
        let value = inspected(&request);
        assert!(configuration_allowed(
            &value,
            &request,
            Path::new("/private/source")
        ));
        let changes = [
            ("/HostConfig/Memory", json!(0)),
            ("/HostConfig/MemorySwap", json!(-1)),
            ("/HostConfig/PidsLimit", json!(0)),
            ("/HostConfig/CpuQuota", json!(0)),
            ("/HostConfig/NetworkMode", json!("host")),
            ("/HostConfig/Privileged", json!(true)),
            ("/HostConfig/ReadonlyRootfs", json!(false)),
            ("/HostConfig/CapAdd", json!(["SYS_ADMIN"])),
            ("/HostConfig/SecurityOpt", json!(["seccomp=unconfined"])),
            ("/HostConfig/PidMode", json!("host")),
            ("/HostConfig/IpcMode", json!("host")),
            ("/HostConfig/CgroupnsMode", json!("host")),
            ("/HostConfig/Tmpfs", json!({"/scratch":"rw"})),
            ("/HostConfig/LogConfig/Type", json!("json-file")),
            ("/Config/AttachStdout", json!(false)),
            ("/Config/AttachStderr", json!(false)),
            (
                "/Config/Healthcheck/Test",
                json!(["CMD-SHELL", "touch /scratch/health"]),
            ),
            ("/HostConfig/Ulimits/0/Hard", json!(4096)),
            ("/HostConfig/Ulimits/1/Soft", json!(1000000)),
            (
                "/HostConfig/Mounts/0/BindOptions/NonRecursive",
                json!(false),
            ),
            ("/HostConfig/Mounts/0/ReadOnly", json!(false)),
            ("/Config/User", json!("0")),
            ("/Config/Env", json!(["SECRET=key"])),
            ("/Config/Entrypoint", json!(["/malicious"])),
            ("/Config/OpenStdin", json!(true)),
            ("/Mounts/0/RW", json!(true)),
            ("/Mounts/0/Source", json!("/var/run/docker.sock")),
            ("/Mounts/0/Propagation", json!("shared")),
            ("/Image", json!("mutable:tag")),
        ];
        for (pointer, replacement) in changes {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                !configuration_allowed(&changed, &request, Path::new("/private/source")),
                "accepted {pointer}"
            );
        }
        let mut extra = value.clone();
        extra["Mounts"].as_array_mut().unwrap().push(json!({"Type":"bind","Source":"/secrets","Destination":"/secrets","RW":false,"Propagation":"rprivate"}));
        assert!(!configuration_allowed(
            &extra,
            &request,
            Path::new("/private/source")
        ));
    }

    #[test]
    fn image_environment_or_anonymous_volume_cannot_expand_access() {
        let request = request();
        let image = json!({"Id":request.recipe.image,"Os":"linux","Config":{"Env":["PATH=/usr/bin:/bin"],"Volumes":null}});
        assert!(image_allowed(&image, &request));
        let mut altered = image.clone();
        altered["Config"]["Volumes"] = json!({"/data":{}});
        assert!(!image_allowed(&altered, &request));
        altered = image.clone();
        altered["Config"]["Env"] = json!(["PATH=/usr/bin:/bin", "INHERITED_SECRET=sensitive"]);
        assert!(!image_allowed(&altered, &request));
        assert!(parsed_env(&json!(["PATH=one", "PATH=two"])).is_none());
    }

    #[test]
    fn create_uses_only_digest_image_and_fixed_isolation_arguments() {
        let request = request();
        let args = create_args(
            &request,
            Path::new("/private/source"),
            "owned-name",
            "owner",
        );
        for required in [
            "--pull=never",
            "none",
            "65532:65532",
            "--read-only",
            "ALL",
            "seccomp=builtin",
            "no-new-privileges=true",
            "--no-healthcheck",
            "--log-driver",
        ] {
            assert!(
                args.iter().any(|value| value == required),
                "missing {required}"
            );
        }
        assert_eq!(args.last(), Some(&request.recipe.image));
        assert!(
            !args
                .iter()
                .any(|value| value.contains("docker.sock") || value == "--privileged")
        );
    }
}
