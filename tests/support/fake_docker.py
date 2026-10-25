#!/usr/bin/python3
import hashlib, json, os, pathlib, sys

args = sys.argv[1:]
state_directory = None
while args and args[0] in ("--host", "--config"):
    if args[0] == "--host" and len(args) > 1 and args[1].startswith("unix://"):
        sidecar = pathlib.Path(args[1][len("unix://"):] + ".fixture-state-dir")
        if sidecar.exists():
            state_directory = pathlib.Path(sidecar.read_text())
            if not state_directory.is_absolute() or not state_directory.is_dir():
                raise ValueError("fixture state directory must be an existing absolute directory")
    args = args[2:]
home = pathlib.Path(os.environ["HOME"])
if state_directory is not None:
    home = state_directory
if (home / "state_dir").exists():  # a second backend sharing the first one's daemon state
    home = pathlib.Path((home / "state_dir").read_text())
state_path = home / "state.json"
scenario_path = home / "scenario"
scenario = scenario_path.read_text().strip() if scenario_path.exists() else "success"
with (home / "calls.jsonl").open("a") as log:
    log.write(json.dumps(args) + "\n")

def load():
    return json.loads(state_path.read_text()) if state_path.exists() else None

def save(value):
    state_path.write_text(json.dumps(value, separators=(",", ":")))

if args[:1] == ["info"]:
    if scenario == "hanging_probe":
        import time; time.sleep(10)
    if scenario == "unavailable":
        print(json.dumps({"OSType":"darwin"}))
    else:
        print(json.dumps({"ID":("fixture-daemon-next" if ((scenario == "rollover_before_start" and load() is not None) or (scenario in ("rollover_after_start", "rollover_absent") and load() is not None and load()["State"]["Status"] != "created")) else "fixture-daemon"),"OSType":"linux","CgroupVersion":"2","Runtimes":{"runc":{}},"MemoryLimit":True,"SwapLimit":True,"CpuCfsPeriod":True,"CpuCfsQuota":True,"PidsLimit":True,"SecurityOptions":["name=seccomp,profile=builtin"],"ServerVersion":"fake-1"}))
elif args[:1] == ["version"]:
    print(json.dumps({"Version":"fake-1","Os":"linux","GitCommit":"fixture"}))
elif args[:2] == ["image", "inspect"]:
    if scenario == "hanging_image":
        import time; time.sleep(10)
    image = args[-1]
    print(json.dumps({"Id":image,"Os":"linux","Config":{"Volumes":None,"Env":None}}))
elif args[:2] == ["container", "create"]:
    if scenario == "uncertain_create": sys.exit(1)
    if scenario == "slow_create":
        import time; time.sleep(6)
    def value(flag): return args[args.index(flag) + 1]
    env = [args[i + 1] for i, item in enumerate(args) if item == "--env"]
    mount = value("--mount")
    source = mount.split("src=", 1)[1].split(",dst=", 1)[0]
    entry = value("--entrypoint")
    image_index = next(i for i, item in enumerate(args) if item.startswith("sha256:"))
    image = args[image_index]
    command = args[image_index + 1:]
    label = value("--label").split("=", 1)[1]
    memory = int(value("--memory")); pids = int(value("--pids-limit")); quota = int(value("--cpu-quota"))
    scratch = value("--tmpfs").split(":", 1)[1]
    effective_quota = quota + 1 if scenario == "bad_cpu" else quota
    health = {"Test":["CMD-SHELL","exit 0"]} if scenario == "bad_health" else {"Test":["NONE"]}
    value = {"name":value("--name"),"Image":image,"Config":{"Labels":{"io.agentscm.execution":label},"User":"65532:65532","WorkingDir":"/source","Tty":False,"OpenStdin":False,"Entrypoint":[entry],"Cmd":command or None,"Env":env,"Healthcheck":health},"State":{"Status":"created","Running":False,"Error":"","ExitCode":0,"OOMKilled":False},"HostConfig":{"NetworkMode":"none","ReadonlyRootfs":True,"Privileged":False,"CapDrop":["ALL"],"CapAdd":[],"SecurityOpt":["no-new-privileges=true","seccomp=builtin"],"Memory":memory,"MemorySwap":memory,"PidsLimit":pids,"CpuPeriod":100000,"CpuQuota":effective_quota,"Runtime":"runc","IpcMode":"none","CgroupnsMode":"private","PidMode":"","UTSMode":"","Devices":[],"DeviceRequests":[],"VolumesFrom":[],"PortBindings":{},"PublishAllPorts":False,"LogConfig":{"Type":"none"},"Tmpfs":{"/scratch":scratch}},"Mounts":[{"Type":"bind","Source":source,"Destination":"/source","RW":False,"Propagation":"rprivate"},{"Type":"tmpfs","Destination":"/scratch","RW":True}]}
    value["Id"] = hashlib.sha256(label.encode()).hexdigest()
    value["Config"]["AttachStdout"] = True
    value["Config"]["AttachStderr"] = True
    value["HostConfig"]["Ulimits"] = [{"Name":"nofile","Soft":128,"Hard":128},{"Name":"core","Soft":0,"Hard":0}]
    value["HostConfig"]["Mounts"] = [{"Type":"bind","Source":source,"Target":"/source","ReadOnly":True,"BindOptions":{"NonRecursive":True,"Propagation":"rprivate"}}]
    save(value)
    (home / "observed.json").write_text(json.dumps(value, indent=2))
    print(value["Id"])
    if scenario == "create_warning": print("warning", file=sys.stderr)
    if scenario == "late_create":  # the reply is lost but the daemon finishes the create
        (home / "hidden_once").write_text("1"); sys.exit(1)
elif args[:2] == ["container", "inspect"]:
    if scenario == "inspect_unavailable": sys.exit(2)
    if (home / "hidden_once").exists():  # the daemon has not published the late create yet
        (home / "hidden_once").unlink()
        print("Error response from daemon: No such container: " + args[-1], file=sys.stderr)
        sys.exit(1)
    value = load()
    name = args[-1]
    if value is None or name not in (value["name"], value["Id"]) or (scenario == "rollover_absent" and value["State"]["Status"] == "running"):
        print("Error response from daemon: No such container: " + name, file=sys.stderr)
        sys.exit(1)
    print(json.dumps(value))
elif args[:3] == ["container", "start", "--attach"]:
    if scenario in ("timeout", "cancel"):
        import time; time.sleep(10)
    if scenario == "flood":
        sys.stdout.buffer.write(b"x" * 131072); sys.stdout.buffer.flush()
    value = load(); value["State"]["Status"] = "running" if scenario == "rollover_absent" else "exited"; value["State"]["ExitCode"] = {"nonzero": 7, "killed": 137}.get(scenario, 0); value["State"]["Running"] = scenario == "rollover_absent"; save(value)
    candidate = home / "candidate.json"  # the agent's stdout for the "candidate" scenario
    if scenario in ("candidate", "candidate_flood"): sys.stdout.buffer.write(candidate.read_bytes()); sys.stdout.buffer.flush()
    else: print("fake candidate output")
    if scenario == "candidate_flood": sys.stdout.buffer.write(b" " * 131072); sys.stdout.buffer.flush()
    if scenario == "nonzero": sys.exit(7)
    if scenario == "killed": print("Killed", file=sys.stderr); sys.exit(137)  # OOMKilled stays false
elif args[:2] == ["container", "rm"]:
    if scenario == "cleanup_failure":
        print("cleanup failed", file=sys.stderr); sys.exit(1)
    current = load()
    if scenario == "replace_on_remove":
        current["Id"] = "f" * 64
        current["Config"]["Labels"]["io.agentscm.execution"] = "replacement-owner"
        save(current)
    if current is None or args[-1] not in (current["name"], current["Id"]):
        print("Error: No such container: " + args[-1], file=sys.stderr); sys.exit(1)
    if state_path.exists(): state_path.unlink()
    if scenario == "lost_remove_reply": sys.exit(1)
else:
    print("unsupported fake Docker invocation: " + repr(args), file=sys.stderr)
    sys.exit(2)
