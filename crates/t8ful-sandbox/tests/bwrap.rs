//! Host-capability tests for the selected Bubblewrap backend.
//!
//! These tests exercise real namespaces, mounts, and seccomp. Set
//! `T8FUL_SANDBOX_TESTS=required` to make a missing or unusable host fail the run
//! instead of skipping. A Nix build sandbox may leave the variable unset.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use t8ful_core::sandbox::{Sandbox, SandboxedCommand};
use t8ful_sandbox::Bubblewrap;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

fn backend() -> Option<Bubblewrap> {
    match Bubblewrap::discover() {
        Ok(backend) => Some(backend),
        Err(error) => {
            assert_ne!(
                std::env::var("T8FUL_TEST_SANDBOX_REQUIRED").ok().as_deref(),
                Some("required"),
                "sandbox tests are required but no backend is available: {error}"
            );
            assert_ne!(
                std::env::var("T8FUL_SANDBOX_TESTS").ok().as_deref(),
                Some("required"),
                "sandbox tests are required but no backend is available: {error}"
            );

            None
        }
    }
}

/// Whether the tests are required to run rather than skipped.
fn required() -> bool {
    std::env::var("T8FUL_SANDBOX_TESTS").ok().as_deref() == Some("required")
        || std::env::var("T8FUL_TEST_SANDBOX_REQUIRED").ok().as_deref() == Some("required")
}

/// Whether this host can actually start a trivial sandboxed command.
///
/// `Bubblewrap::discover` only proves the binary exists. A Nix build sandbox
/// has the binary on `PATH` but may forbid user namespaces, so a probe is the
/// only reliable way to honor the documented skip behavior.
async fn backend_usable(backend: &Bubblewrap) -> bool {
    let fixture = Fixture::new();

    match backend.spawn(fixture.command("true")).await {
        Ok(mut process) => matches!(process.child.wait().await, Ok(status) if status.success()),
        Err(_) => false,
    }
}

async fn backend_or_skip() -> Option<Bubblewrap> {
    let backend = backend()?;

    if backend_usable(&backend).await {
        return Some(backend);
    }

    assert!(
        !required(),
        "sandbox tests are required but the backend cannot start a command"
    );

    None
}

fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);

        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

fn bash() -> PathBuf {
    find_program("bash").expect("bash must be on PATH for sandbox tests")
}

struct Fixture {
    project: TempDir,
    extra: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let project = TempDir::new().expect("project");
        let extra = TempDir::new().expect("extra");
        let home = project.path().join("home");

        std::fs::create_dir(&home).unwrap();
        std::fs::write(project.path().join("file.txt"), "hello").unwrap();
        std::fs::write(extra.path().join("extra.txt"), "extra").unwrap();
        std::fs::write(home.join("notes.txt"), "home-contents").unwrap();

        Self { project, extra }
    }

    /// A home directory inside the read-only project bind. A real home lives on
    /// a host submount, which is read-only for the same reason.
    fn home(&self) -> PathBuf {
        self.project.path().join("home")
    }

    fn command(&self, script: &str) -> SandboxedCommand {
        let mut env = BTreeMap::new();

        env.insert(
            "PATH".to_owned(),
            std::env::var("PATH").unwrap_or_else(|_| "/nix/store".to_owned()),
        );
        env.insert("LANG".to_owned(), "C.UTF-8".to_owned());
        env.insert("HOME".to_owned(), self.home().display().to_string());
        env.insert("EXTRA".to_owned(), self.extra.path().display().to_string());

        SandboxedCommand {
            program: bash().display().to_string(),
            args: vec!["-c".to_owned(), script.to_owned()],
            cwd: self.project.path().to_path_buf(),
            env,
            read_only_paths: vec![
                PathBuf::from("/nix/store"),
                self.project.path().to_path_buf(),
                self.extra.path().to_path_buf(),
            ],
        }
    }

    async fn run(&self, script: &str) -> Option<std::process::Output> {
        self.run_with(script, None).await
    }

    async fn run_with(&self, script: &str, input: Option<&str>) -> Option<std::process::Output> {
        let backend = backend_or_skip().await?;
        let mut process = backend
            .spawn(self.command(script))
            .await
            .expect("sandbox starts");

        if let Some(input) = input {
            let mut stdin = process.child.stdin.take().expect("stdin");
            stdin.write_all(input.as_bytes()).await.unwrap();
            stdin.shutdown().await.unwrap();
        }

        process.child.wait_with_output().await.ok()
    }
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn combined(output: &std::process::Output) -> String {
    format!("{} {}", stdout(output), stderr(output))
}

#[tokio::test]
async fn project_and_approved_extra_paths_are_read_only() {
    let fixture = Fixture::new();

    let Some(output) = fixture
        .run("cat file.txt; cat \"$EXTRA/extra.txt\"; echo nope > file.txt 2>&1 || echo write-project-denied")
        .await
    else {
        return;
    };

    assert!(stdout(&output).contains("hello"), "{}", stderr(&output));
    assert!(stdout(&output).contains("extra"), "{}", stderr(&output));
    assert!(stdout(&output).contains("write-project-denied"));
    assert_eq!(
        std::fs::read_to_string(fixture.project.path().join("file.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn the_whole_host_is_readable_but_writes_are_refused() {
    let fixture = Fixture::new();

    let Some(output) = fixture
        .run(
            "head -1 /etc/passwd; cat \"$HOME/notes.txt\"; \
             ls \"$(command -v bash)\" >/dev/null && echo store-readable; \
             echo nope > \"$HOME/notes.txt\" 2>&1 || echo home-read-only; \
             echo nope > /etc/passwd 2>&1 || echo etc-read-only; \
             echo nope > /nix/store/t8ful-probe 2>&1 || echo store-read-only; \
             echo nope > /var/t8ful-probe 2>&1 || echo var-read-only; \
             touch /tmp/t8ful-probe && echo scratch-writable",
        )
        .await
    else {
        return;
    };
    let text = combined(&output);

    assert!(
        text.contains("root:"),
        "the host passwd is readable: {text}"
    );
    assert!(text.contains("home-contents"), "{text}");
    assert!(text.contains("store-readable"), "{text}");
    assert!(text.contains("home-read-only"), "{text}");
    assert!(text.contains("etc-read-only"), "{text}");
    assert!(text.contains("store-read-only"), "{text}");
    assert!(text.contains("var-read-only"), "{text}");
    assert!(text.contains("scratch-writable"), "{text}");
    assert_eq!(
        std::fs::read_to_string(fixture.home().join("notes.txt")).unwrap(),
        "home-contents",
        "the host home directory was never written"
    );
}

#[tokio::test]
async fn the_scratch_directory_is_private_and_discarded() {
    let fixture = Fixture::new();

    let Some(first) = fixture
        .run("touch /tmp/t8ful-scratch && test -e /tmp/t8ful-scratch && echo scratch-ok")
        .await
    else {
        return;
    };
    let Some(second) = fixture
        .run("test -e /tmp/t8ful-scratch && echo leaked || echo scratch-fresh")
        .await
    else {
        return;
    };

    assert!(stdout(&first).contains("scratch-ok"), "{}", stderr(&first));
    assert!(
        stdout(&second).contains("scratch-fresh"),
        "the scratch file outlived its command: {}",
        stdout(&second)
    );
    assert!(
        !std::path::Path::new("/tmp/t8ful-scratch").exists(),
        "the host /tmp was written"
    );
}

#[tokio::test]
async fn the_read_only_root_cannot_be_remounted_writable() {
    let fixture = Fixture::new();

    let Some(output) = fixture
        .run("mount -o remount,rw / 2>&1 || echo remount-denied")
        .await
    else {
        return;
    };
    let text = combined(&output);

    assert!(text.contains("remount-denied"), "{text}");
}

#[tokio::test]
async fn a_project_under_tmp_stays_readable_after_the_scratch_tmpfs() {
    // `TempDir` already places the project under /tmp; the read-only rebind has
    // to happen after the scratch tmpfs masks the host scratch directory.
    let fixture = Fixture::new();
    assert!(
        fixture
            .project
            .path()
            .starts_with(std::env::temp_dir().as_path().join(""))
            || fixture.project.path().starts_with("/tmp"),
        "the fixture is expected under /tmp: {}",
        fixture.project.path().display()
    );

    let Some(output) = fixture.run("cat file.txt").await else {
        return;
    };

    assert!(stdout(&output).contains("hello"), "{}", stderr(&output));
}

#[tokio::test]
async fn network_and_unix_socket_creation_are_denied() {
    let fixture = Fixture::new();

    let Some(output) = fixture
        .run(
            "if (exec 3<>/dev/tcp/127.0.0.1/9) 2>/dev/null; then echo network-escape; else \
             echo network-denied; fi; \
             python3 -c 'import socket; socket.socket()' 2>&1 || echo socket-denied",
        )
        .await
    else {
        return;
    };
    let text = combined(&output);

    assert!(text.contains("network-denied"), "{text}");
    assert!(
        text.contains("socket-denied") || text.contains("PermissionError"),
        "{text}"
    );
}

#[tokio::test]
async fn environment_credentials_are_dropped_from_the_sandbox() {
    let fixture = Fixture::new();
    let Some(backend) = backend_or_skip().await else {
        return;
    };
    let mut command = fixture.command("test -z \"$OPENROUTER_API_KEY\" && echo credentials-hidden");

    command
        .env
        .insert("OPENROUTER_API_KEY".to_owned(), "secret".to_owned());
    let process = backend.spawn(command).await.expect("sandbox starts");
    let output = process.child.wait_with_output().await.unwrap();
    let text = combined(&output);

    assert!(text.contains("credentials-hidden"), "{text}");
    assert!(!text.contains("secret"), "{text}");
}

#[tokio::test]
async fn environment_values_are_not_published_in_the_sandbox_argv() {
    let fixture = Fixture::new();
    let Some(backend) = backend_or_skip().await else {
        return;
    };
    let secret = "sk-or-v1-argv-probe-secret";
    let mut command = fixture.command("printf 'seen:%s\\n' \"$T8FUL_TEST_SECRET\"; sleep 1");

    command
        .env
        .insert("T8FUL_TEST_SECRET".to_owned(), secret.to_owned());

    let process = backend.spawn(command).await.expect("sandbox starts");
    let pid = process.child.id().expect("sandbox pid");
    let mut argv_seen = false;

    for _ in 0..200 {
        if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            let text = String::from_utf8_lossy(&bytes);

            if text.contains("--seccomp") {
                argv_seen = true;
                assert!(
                    !text.contains(secret),
                    "the environment value leaked into argv: {text}"
                );
                break;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert!(argv_seen, "the sandbox command did not reach bubblewrap");

    let output = process.child.wait_with_output().await.unwrap();
    let text = combined(&output);

    assert!(text.contains(&format!("seen:{secret}")), "{text}");
}

#[tokio::test]
async fn io_uring_setup_is_denied_by_seccomp() {
    if find_program("python3").is_none() {
        return;
    }

    let fixture = Fixture::new();
    let Some(output) = fixture
        .run(
            "python3 -c 'import ctypes; libc=ctypes.CDLL(None, use_errno=True); \
             r=libc.syscall(425, 0, 0); e=ctypes.get_errno(); print(r, e); \
             raise SystemExit(0 if e == 1 else 1)'",
        )
        .await
    else {
        return;
    };

    assert!(output.status.success(), "{}", combined(&output));
}

#[tokio::test]
async fn inherited_descriptors_are_not_exposed() {
    let fixture = Fixture::new();
    let leaked = fixture.project.path().join("leaked.txt");

    std::fs::write(&leaked, "leaked").unwrap();

    let file = std::fs::File::open(&leaked).unwrap();
    let raw = {
        use std::os::fd::AsRawFd;

        file.as_raw_fd()
    };

    nix::fcntl::fcntl(
        &file,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .unwrap();

    let Some(backend) = backend_or_skip().await else {
        return;
    };

    let mut command = fixture.command(&format!(
        "if [ -e /proc/self/fd/{raw} ]; then cat /proc/self/fd/{raw}; else echo fd-closed; fi"
    ));
    command.read_only_paths.push(leaked.clone());

    let process = backend.spawn(command).await.expect("sandbox starts");
    let output = process.child.wait_with_output().await.unwrap();
    let text = combined(&output);

    assert!(
        text.contains("fd-closed") || !text.contains("leaked"),
        "{text}"
    );
    drop(file);
}

#[tokio::test]
async fn stdio_pipes_work_for_a_fake_server() {
    let fixture = Fixture::new();
    let Some(output) = fixture
        .run_with("read line; printf 'server:%s\\n' \"$line\"", Some("ping\n"))
        .await
    else {
        return;
    };

    assert!(
        stdout(&output).contains("server:ping"),
        "{}",
        combined(&output)
    );
}

#[tokio::test]
async fn dropping_the_child_cleans_up_the_sandbox() {
    let Some(backend) = backend_or_skip().await else {
        return;
    };
    let fixture = Fixture::new();
    let command = fixture.command("sleep 30");

    let mut process = backend.spawn(command).await.expect("sandbox starts");

    process.child.start_kill().expect("kill");
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), process.child.wait())
        .await
        .expect("cleanup bound")
        .expect("wait");

    use std::os::unix::process::ExitStatusExt;

    assert!(!status.success() || status.signal().is_some());
}

/// Compile-time proof that the test helper's imports stay used on all targets.
#[allow(dead_code)]
async fn _read_to_end_for_future_fixtures(mut child: tokio::process::Child) {
    let mut buffer = Vec::new();

    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut buffer).await;
    }
}

#[allow(dead_code)]
async fn _execute_for_future_fixtures(program: &Path) {
    let _ = Command::new(program).stdin(Stdio::null()).output().await;
}
