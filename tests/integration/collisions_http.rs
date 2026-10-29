use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

struct Server {
    child: Child,
    address: SocketAddr,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    fn start(directory: &TempDir) -> Self {
        Self::start_with(directory, &[])
    }
    fn start_with(directory: &TempDir, extra: &[&std::ffi::OsStr]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--data-dir")
            .arg(directory.path())
            .args(["serve", "--bind", "127.0.0.1:0"])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let line = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => line,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("listen failed: {error}");
            }
        };
        let address = line
            .trim()
            .strip_prefix("listening on ")
            .expect("listening address")
            .parse()
            .unwrap();
        Self { child, address }
    }
    fn request(&self, method: &str, target: &str) -> String {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(
                format!(
                    "{method} {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}

#[cfg(unix)]
fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("/usr/bin/git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[cfg(unix)]
#[test]
fn collisions_are_off_by_default_and_report_conflicts_over_the_api() {
    let data = TempDir::new().unwrap();
    let off = Server::start(&data);
    let response = off.request("GET", "/v1/collisions");
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(response.contains("--collisions-repo"));
    drop(off);

    let repo = TempDir::new().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("f"), "one\n").unwrap();
    std::fs::write(dir.join("g"), "1\n2\n3\n4\n5\n6\n7\n8\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "base"]);
    for (branch, file, text) in [
        ("left", "f", "left\n"),
        ("right", "f", "right\n"),
        ("top<i>", "g", "top\n2\n3\n4\n5\n6\n7\n8\n"),
        ("bottom", "g", "1\n2\n3\n4\n5\n6\n7\nbottom\n"),
    ] {
        git(dir, &["checkout", "-q", "-b", branch, "main"]);
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["commit", "-qam", branch]);
    }
    git(dir, &["checkout", "-q", "main"]);
    let on = Server::start_with(&data, &["--collisions-repo".as_ref(), dir.as_os_str()]);
    assert!(
        on.request("GET", "/v1/collisions?base=x")
            .starts_with("HTTP/1.1 400")
    );
    let api = on.request("GET", "/v1/collisions");
    assert!(api.starts_with("HTTP/1.1 200"), "{api}");
    assert!(api.contains("\"level\":\"conflict\""));
    assert!(api.contains("\"truncated\":null"));
    assert!(api.contains("top<i>"));
}
