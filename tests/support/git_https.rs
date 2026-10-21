use super::{git, run};
use agentscm::git_export::remote::network::trust_digest;
use agentscm::git_export::remote::{Access, Endpoint, HttpsTrust};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

pub struct HttpsFixture {
    child: Child,
    pub root: TempDir,
    pub remote: PathBuf,
    pub ca: PathBuf,
    pub credential: PathBuf,
    pub settings: PathBuf,
    pub url: String,
}

impl Drop for HttpsFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl HttpsFixture {
    #[allow(dead_code)]
    pub fn repository(mut self, name: &str) -> Self {
        let parent = self.remote.parent().unwrap();
        let remote = parent.join(format!("{name}.git"));
        fs::create_dir_all(remote.parent().unwrap()).unwrap();
        fs::rename(&self.remote, &remote).unwrap();
        self.remote = remote;
        self.url = self.url.replace("fixture.git", &format!("{name}.git"));
        self
    }

    pub fn new(bad_hostname: bool) -> Self {
        let root = TempDir::new().unwrap();
        let repository_root = root.path().join("repositories");
        fs::create_dir(&repository_root).unwrap();
        let remote = repository_root.join("fixture.git");
        fs::create_dir(&remote).unwrap();
        run(
            &remote,
            &["init", "--bare", "--template=", "--object-format=sha1"],
        );
        run(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        run(&remote, &["config", "http.receivepack", "true"]);
        let credential = root.path().join("credential.json");
        let mut random = [0u8; 24];
        getrandom::fill(&mut random).unwrap();
        let token = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        fs::write(
            &credential,
            serde_json::to_vec(&json!({"username":"synthetic","password":token})).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
        let ca = root.path().join("certificate.pem");
        let key = root.path().join("key.pem");
        let extension = if bad_hostname {
            "subjectAltName=IP:127.0.0.2"
        } else {
            "subjectAltName=IP:127.0.0.1,DNS:localhost"
        };
        let result = Command::new("/usr/bin/openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=synthetic",
                "-addext",
                extension,
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&ca)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(result.success());
        fs::set_permissions(&ca, fs::Permissions::from_mode(0o600)).unwrap();
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let global = root.path().join("global");
        let system = root.path().join("system");
        fs::write(&global, "").unwrap();
        fs::write(&system, "").unwrap();
        let port_file = root.path().join("port");
        let settings = root.path().join("server.json");
        fs::write(&settings,serde_json::to_vec(&json!({"root":repository_root,"git":git(),"home":home,"global_config":global,"system_config":system,"certificate":ca,"key":key,"credential":credential,"port_file":port_file,"requests":root.path().join("requests.jsonl")})).unwrap()).unwrap();
        let mut child = Command::new("/usr/bin/python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/git_smart_http.py"
            ))
            .arg(&settings)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !port_file.exists() {
            assert!(Instant::now() < deadline, "TLS fixture startup timed out");
            assert!(child.try_wait().unwrap().is_none(), "TLS fixture stopped");
            std::thread::sleep(Duration::from_millis(10));
        }
        let port = fs::read_to_string(port_file).unwrap();
        Self {
            child,
            root,
            remote,
            ca,
            credential,
            settings,
            url: format!("https://127.0.0.1:{port}/fixture.git"),
        }
    }
    #[allow(dead_code)]
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Https {
            url: self.url.clone(),
            trust: HttpsTrust::CaFile {
                sha256: trust_digest(&self.ca).unwrap(),
            },
        }
    }
    #[allow(dead_code)]
    pub fn access(&self) -> Access {
        Access {
            credential_file: Some(self.credential.clone()),
            credential_helper: Some(PathBuf::from(env!("CARGO_BIN_EXE_agentscm"))),
            ca_file: Some(self.ca.clone()),
            ssh: None,
        }
    }
    pub fn option(&self, key: &str, value: serde_json::Value) {
        let mut settings: serde_json::Value =
            serde_json::from_slice(&fs::read(&self.settings).unwrap()).unwrap();
        settings[key] = value;
        fs::write(&self.settings, serde_json::to_vec(&settings).unwrap()).unwrap();
    }
    pub fn requests(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.root.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
