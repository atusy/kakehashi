#![cfg(all(target_os = "linux", feature = "e2e"))]

use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

struct Topology {
    directory: tempfile::TempDir,
    parent: Child,
    _parent_stdin: ChildStdin,
    _liveness_writer: std::os::fd::OwnedFd,
}

impl Topology {
    fn spawn(race_gate: Option<&Path>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let helper_pid = directory.path().join("helper.pid");
        let armed = directory.path().join("pdeathsig.armed");
        let before = directory.path().join("pdeathsig.before");
        let published_worker_pid = directory.path().join("worker.pid");
        let (read_end, write_end) = nix::unistd::pipe().unwrap();
        fcntl(&read_end, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).unwrap();
        fcntl(&write_end, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).unwrap();
        let read_fd = read_end.as_raw_fd();
        let script = r#"
"$KAKEHASHI_BIN" __tree-worker \
  --threads 1 \
  --parent-liveness-fd "$LIVENESS_FD" \
  --expected-parent-pid "$$" &
helper=$!
echo "$helper" > "$HELPER_PID_FILE"
while [ ! -e "$READY_FILE" ]; do sleep 0.01; done
"#;
        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .env("KAKEHASHI_BIN", env!("CARGO_BIN_EXE_kakehashi"))
            .env("LIVENESS_FD", read_fd.to_string())
            .env("HELPER_PID_FILE", &helper_pid)
            .env(
                "READY_FILE",
                race_gate.map_or_else(|| armed.as_path(), |_| before.as_path()),
            )
            .env("KAKEHASHI_TREE_WORKER_PDEATHSIG_MARKER", &armed)
            .env("KAKEHASHI_TREE_WORKER_PDEATHSIG_BEFORE_MARKER", &before)
            .env("KAKEHASHI_TREE_WORKER_PID_FILE", &published_worker_pid)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(gate) = race_gate {
            command.env("KAKEHASHI_TREE_WORKER_PDEATHSIG_GATE", gate);
        }
        // SAFETY: fcntl is async-signal-safe and only clears CLOEXEC on the
        // liveness descriptor intentionally inherited by this topology.
        unsafe {
            command.pre_exec(move || {
                if nix::libc::fcntl(read_fd, nix::libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut parent = command.spawn().unwrap();
        let parent_stdin = parent.stdin.take().unwrap();
        drop(read_end);
        Self {
            directory,
            parent,
            _parent_stdin: parent_stdin,
            _liveness_writer: write_end,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn wait_for(&self, name: &str) -> bool {
        let path = self.path(name);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn helper_pid(&self) -> Option<Pid> {
        std::fs::read_to_string(self.path("helper.pid"))
            .ok()
            .and_then(|value| value.trim().parse::<i32>().ok())
            .map(Pid::from_raw)
    }

    fn wait_parent(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.parent.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("spawning parent did not exit");
    }

    fn helper_exited(&self) -> bool {
        let Some(pid) = self.helper_pid() else {
            return false;
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Drop for Topology {
    fn drop(&mut self) {
        let _ = self.parent.kill();
        let _ = self.parent.wait();
        if let Some(pid) = self.helper_pid() {
            let _ = kill(pid, Signal::SIGKILL);
        }
    }
}

#[test]
fn registered_pdeathsig_kills_worker_while_liveness_pipe_stays_open() {
    let mut topology = Topology::spawn(None);
    assert!(topology.wait_for("pdeathsig.armed"), "worker never armed");
    assert_eq!(
        std::fs::read_to_string(topology.path("pdeathsig.armed"))
            .unwrap()
            .trim(),
        (Signal::SIGKILL as i32).to_string()
    );
    topology.wait_parent();
    assert!(topology.helper_exited(), "worker survived its Linux parent");
}

#[test]
fn parent_loss_before_registration_exits_before_worker_publication() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("registration.gate");
    std::fs::write(&gate, "blocked").unwrap();
    let mut topology = Topology::spawn(Some(&gate));
    assert!(
        topology.wait_for("pdeathsig.before"),
        "worker never reached registration barrier"
    );
    topology.wait_parent();
    std::fs::remove_file(gate).unwrap();
    assert!(
        topology.helper_exited(),
        "worker survived registration race"
    );
    assert!(!topology.path("pdeathsig.armed").exists());
    assert!(!topology.path("worker.pid").exists());
}
