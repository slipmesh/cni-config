//! The binary is PID 1 in its pod, where a SIGTERM with no handler installed is dropped and the pod
//! waits out its grace period. Here it is not PID 1, so a SIGTERM with no handler kills it instead:
//! the exit status tells whether one was there, during startup and once it watches.

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::Mode;
use nix::unistd::{Pid, mkfifo};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Points at a port nothing listens on: the watch only has to start, not succeed.
const KUBECONFIG: &str = "\
apiVersion: v1
kind: Config
clusters:
  - name: test
    cluster:
      server: https://127.0.0.1:9
contexts:
  - name: test
    context:
      cluster: test
      user: test
current-context: test
users:
  - name: test
    user: {}
";

#[test]
fn a_sigterm_during_startup_ends_the_process_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let kubeconfig = dir.path().join("kubeconfig");
    mkfifo(&kubeconfig, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cni-config"))
        .env("NODE_NAME", "node-a")
        .env("KUBECONFIG", &kubeconfig)
        .env_remove("KUBERNETES_SERVICE_HOST")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut fifo = open_once_read(&kubeconfig, &mut child, deadline);
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();
    // Fails with EPIPE when the signal has already killed the reader.
    let _ = fifo.write_all(KUBECONFIG.as_bytes());
    drop(fifo);

    assert_exits_cleanly(&mut child, deadline);
}

#[test]
fn a_sigterm_while_it_watches_ends_the_process_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let kubeconfig = dir.path().join("kubeconfig");
    std::fs::write(&kubeconfig, KUBECONFIG).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cni-config"))
        .env("NODE_NAME", "node-a")
        .env("KUBECONFIG", &kubeconfig)
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // The API server it points at refuses every connection, so from here on it retries the watch.
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let started = stdout
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains("starting cni-config"));
    assert!(started, "exited before it started");
    std::thread::sleep(Duration::from_millis(500));
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    assert_exits_cleanly(&mut child, deadline);
}

fn assert_exits_cleanly(child: &mut Child, deadline: Instant) {
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("still running 10 s after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status}");
}

/// The FIFO's write end, once the binary has opened the read end: from then on it is inside its
/// startup, reading the kubeconfig, and has not reached its main loop.
fn open_once_read(fifo: &Path, child: &mut Child, deadline: Instant) -> File {
    loop {
        // A non-blocking open for writing fails with ENXIO while no reader has the FIFO open.
        match OpenOptions::new()
            .write(true)
            .custom_flags(OFlag::O_NONBLOCK.bits())
            .open(fifo)
        {
            Ok(file) => return file,
            Err(e) if e.raw_os_error() == Some(Errno::ENXIO as i32) => {}
            Err(e) => panic!("{e}"),
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("exited before reading its kubeconfig: {status}");
        }
        assert!(Instant::now() < deadline, "never read its kubeconfig");
        std::thread::sleep(Duration::from_millis(10));
    }
}
