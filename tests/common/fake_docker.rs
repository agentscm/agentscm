// The deterministic control-plane fixture executes only the system Python at
// /usr/bin/python3. It never launches a candidate command on the host.
pub const FAKE_DOCKER: &str = include_str!("../support/fake_docker.py");
