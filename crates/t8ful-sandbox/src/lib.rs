//! Linux planning sandbox.
//!
//! The selected backend is Bubblewrap. Planning commands see the whole host as
//! a read-only tree: the root and every submount are bound read-only, so reads
//! behave exactly like the host and every write fails with `EROFS`, except in a
//! private `/tmp` and `/var/tmp` that are discarded when the command ends. The
//! project and read-scope roots are re-bound after the scratch tmpfs so a
//! project living under `/tmp` stays readable. Network and host sockets are
//! unusable: `--unshare-all` removes the interfaces and the seccomp filter
//! denies `socket`, `connect`, and `io_uring_setup`. There is no unsandboxed
//! fallback.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use t8ful_core::sandbox::{Sandbox, SandboxError, SandboxedCommand, SandboxedProcess};
use tokio::process::Command;

/// Bubblewrap-backed planning sandbox.
#[derive(Debug, Clone)]
pub struct Bubblewrap {
    program: PathBuf,
}

impl Bubblewrap {
    /// Use a specific `bwrap` executable.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Find `bwrap` on `PATH`.
    pub fn discover() -> Result<Self, SandboxError> {
        let path = std::env::var_os("PATH").ok_or_else(|| {
            SandboxError::Unavailable("PATH is not set, so bubblewrap cannot be located".to_owned())
        })?;

        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("bwrap");

            if is_executable(&candidate) {
                return Ok(Self::new(candidate));
            }
        }

        Err(SandboxError::Unavailable(
            "bubblewrap (`bwrap`) is not installed; install it through Nix or the system package \
             manager before using plan mode"
                .to_owned(),
        ))
    }
}

#[async_trait]
impl Sandbox for Bubblewrap {
    fn name(&self) -> &'static str {
        "bubblewrap"
    }

    async fn spawn(&self, command: SandboxedCommand) -> Result<SandboxedProcess, SandboxError> {
        if command.program.trim().is_empty() {
            return Err(SandboxError::Launch(
                "the sandboxed program must not be empty".to_owned(),
            ));
        }

        // The filter denies syscalls by target-architecture number and guards
        // the ABI with an AUDIT_ARCH comparison, so an unsupported target has no
        // usable program. Refuse here instead of installing a filter that would
        // kill every syscall of every command.
        if denied_syscalls().is_empty() {
            return Err(SandboxError::Unsupported(format!(
                "the plan-mode sandbox supports only x86_64 and aarch64, not {}",
                std::env::consts::ARCH
            )));
        }

        let cwd = std::fs::canonicalize(&command.cwd).map_err(|error| {
            SandboxError::Launch(format!(
                "cannot resolve the working directory {}: {error}",
                command.cwd.display()
            ))
        })?;

        let mounts = canonical_mounts(&command.read_only_paths)?;
        let host = host_mounts()?;
        let root = File::open("/").map_err(|error| {
            SandboxError::Launch(format!("cannot pin the host root for reading: {error}"))
        })?;
        let mount_files = open_mounts(&mounts)?;
        let (host, host_files) = open_optional_mounts(&host);

        let seccomp = write_seccomp_filter()?;
        let seccomp_fd = clear_cloexec(&seccomp)?;
        let root_fd = clear_cloexec(&root)?;
        let mut keep = host_files
            .iter()
            .map(clear_cloexec)
            .collect::<Result<Vec<_>, _>>()?;

        keep.extend(
            mount_files
                .iter()
                .map(clear_cloexec)
                .collect::<Result<Vec<_>, _>>()?,
        );
        keep.push(root_fd);
        keep.push(seccomp_fd);

        let host_entries = host
            .iter()
            .zip(host_files.iter())
            .map(|(mount, file)| (file.as_raw_fd(), mount.as_path()))
            .collect::<Vec<_>>();
        let mount_entries = mounts
            .iter()
            .zip(mount_files.iter())
            .map(|(mount, file)| (file.as_raw_fd(), mount.as_path()))
            .collect::<Vec<_>>();
        let env = sandbox_env(&command.env);
        let args = profile_args(
            &cwd,
            &command.program,
            &command.args,
            root_fd,
            &host_entries,
            &mount_entries,
            seccomp_fd,
        );

        let bash = find_program("bash").ok_or_else(|| {
            SandboxError::Unavailable(
                "bash is required to close inherited descriptors before starting bubblewrap"
                    .to_owned(),
            )
        })?;
        let mut process = Command::new(bash);
        let close = descriptor_list(&keep)?;
        let script = "for n in $T8FUL_CLOSE_FDS; do eval \"exec $n>&-\" 2>/dev/null || true; done; \
                      unset T8FUL_CLOSE_FDS; exec \"$@\"";

        process
            .arg("-c")
            .arg(script)
            .arg("t8ful-sandbox")
            .arg(&self.program)
            .args(args)
            .env_clear()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .env("T8FUL_CLOSE_FDS", close)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            process.process_group(0);
        }

        let child = process.spawn().map_err(|error| {
            SandboxError::Launch(format!(
                "cannot start bubblewrap ({}): {error}",
                self.program.display()
            ))
        })?;

        // The child inherited copies of these descriptors at exec. Closing the
        // parent copies now prevents a long-lived sandbox from pinning them.
        drop(root);
        drop(host_files);
        drop(mount_files);
        drop(seccomp);

        Ok(SandboxedProcess { child })
    }
}

/// Canonicalize, de-duplicate, and remove nested mounts. Mount parents before
/// children so a project under `/tmp` is not hidden by the scratch tmpfs.
fn canonical_mounts(paths: &[PathBuf]) -> Result<Vec<PathBuf>, SandboxError> {
    let mut unique = BTreeSet::new();

    for path in paths {
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            SandboxError::Launch(format!(
                "cannot resolve the read-only path {}: {error}",
                path.display()
            ))
        })?;

        if canonical == Path::new("/") {
            return Err(SandboxError::Unsupported(
                "refusing to mount the entire host root read-only".to_owned(),
            ));
        }

        unique.insert(canonical);
    }

    let mut sorted = unique.into_iter().collect::<Vec<_>>();

    sorted.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });

    let mut mounts: Vec<PathBuf> = Vec::new();

    for path in sorted {
        if !mounts.iter().any(|parent| path.starts_with(parent)) {
            mounts.push(path);
        }
    }

    Ok(mounts)
}

fn open_mounts(mounts: &[PathBuf]) -> Result<Vec<File>, SandboxError> {
    mounts
        .iter()
        .map(|path| {
            File::open(path).map_err(|error| {
                SandboxError::Launch(format!(
                    "cannot pin the read-only path {}: {error}",
                    path.display()
                ))
            })
        })
        .collect()
}

/// Every mount point on the host, excluding the root itself.
///
/// A recursive bind of `/` does not make the filesystems mounted below it
/// read-only, so each one is bound separately. A mount point the t8ful process
/// cannot open is skipped: the sandboxed command runs as the same user, so it
/// cannot open that mount either, and the read-only parent bind still blocks
/// creating files through it.
fn host_mounts() -> Result<Vec<PathBuf>, SandboxError> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").map_err(|error| {
        SandboxError::Unavailable(format!(
            "cannot enumerate host mount points to make them read-only: {error}"
        ))
    })?;

    Ok(parse_mountinfo(&text))
}

/// Parse the fifth field of `/proc/self/mountinfo` lines.
///
/// The field is octal-escaped: `\040` is a space, `\011` a tab, `\012` a
/// newline, and `\134` a backslash. Paths are returned in binding order, which
/// is path order so parents are bound before their children.
fn parse_mountinfo(text: &str) -> Vec<PathBuf> {
    let mut mounts = BTreeSet::new();

    for line in text.lines() {
        let Some(field) = line.split_whitespace().nth(4) else {
            continue;
        };
        let path = PathBuf::from(unescape_mount_field(field));

        if path != Path::new("/") {
            mounts.insert(path);
        }
    }

    let mut mounts = mounts.into_iter().collect::<Vec<_>>();

    mounts.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });

    mounts
}

fn unescape_mount_field(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();

    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);

            continue;
        }

        let digits = chars.clone().take(3).collect::<String>();

        match digits.as_str() {
            "040" => {
                out.push(' ');
                chars.next();
                chars.next();
                chars.next();
            }
            "011" => {
                out.push('\t');
                chars.next();
                chars.next();
                chars.next();
            }
            "012" => {
                out.push('\n');
                chars.next();
                chars.next();
                chars.next();
            }
            "134" => {
                out.push('\\');
                chars.next();
                chars.next();
                chars.next();
            }
            _ => out.push('\\'),
        }
    }

    out
}

fn open_optional_mounts(mounts: &[PathBuf]) -> (Vec<PathBuf>, Vec<File>) {
    let mut kept = Vec::new();
    let mut files = Vec::new();

    for mount in mounts {
        let Ok(file) = File::open(mount) else {
            continue;
        };

        kept.push(mount.clone());
        files.push(file);
    }

    (kept, files)
}

/// The bubblewrap argument vector for the planning profile.
///
/// Kept pure so the shape can be asserted without starting a sandbox: the host
/// root is bound read-only first, then each submount, then fresh `/proc` and
/// `/dev`, then the writable scratch tmpfs, and only then the fd-pinned
/// read-only roots (so a project under `/tmp` stays visible).
/// The environment is deliberately not part of the argument vector: bubblewrap
/// inherits it from the wrapper process, so a secret is never published in
/// `/proc/<pid>/cmdline`.
#[allow(clippy::too_many_arguments)]
fn profile_args(
    cwd: &Path,
    program: &str,
    args: &[String],
    root_fd: i32,
    host_mounts: &[(i32, &Path)],
    read_only_mounts: &[(i32, &Path)],
    seccomp_fd: i32,
) -> Vec<OsString> {
    let mut profile: Vec<OsString> = vec![
        "--die-with-parent".into(),
        "--unshare-all".into(),
        "--ro-bind-fd".into(),
        root_fd.to_string().into(),
        "/".into(),
    ];

    for (fd, mount) in host_mounts {
        profile.push("--ro-bind-fd".into());
        profile.push(fd.to_string().into());
        profile.push(mount.as_os_str().to_owned());
    }

    profile.extend([
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        // The only writable locations; both are discarded with the sandbox.
        "--tmpfs".into(),
        "/tmp".into(),
        "--tmpfs".into(),
        "/var/tmp".into(),
    ]);

    for (fd, mount) in read_only_mounts {
        profile.push("--ro-bind-fd".into());
        profile.push(fd.to_string().into());
        profile.push(mount.as_os_str().to_owned());
    }

    profile.extend([
        "--chdir".into(),
        cwd.as_os_str().to_owned(),
        "--seccomp".into(),
        seccomp_fd.to_string().into(),
    ]);

    profile.push("--".into());
    profile.push(program.into());

    for argument in args {
        profile.push(argument.into());
    }

    profile
}

fn clear_cloexec(file: &File) -> Result<i32, SandboxError> {
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;

    nix::fcntl::fcntl(
        file.as_fd(),
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .map_err(|error| {
        SandboxError::Launch(format!("cannot prepare an inherited descriptor: {error}"))
    })?;

    Ok(file.as_raw_fd())
}

/// Environment visible to the planning process.
///
/// Callers already filter credentials; this removes host-session locations that
/// would be meaningless or misleading in the sandbox and supplies a private
/// home and scratch directory.
fn sandbox_env(env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    const DROP: &[&str] = &[
        "TMPDIR",
        "TERM",
        "SSH_AUTH_SOCK",
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "NIX_REMOTE",
        "OPENROUTER_API_KEY",
        "OPENCODE_GO_API_KEY",
    ];

    let mut values = env
        .iter()
        .filter(|(name, _)| !DROP.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Vec<_>>();
    // `HOME` and the `XDG_*` variables pass through, so `~`, global git config,
    // and XDG paths resolve the same way they do on the host.
    let home = env
        .get("HOME")
        .cloned()
        .unwrap_or_else(|| "/tmp".to_owned());

    values.push(("HOME".to_owned(), home));
    values.push(("TMPDIR".to_owned(), "/tmp".to_owned()));
    values.push(("TERM".to_owned(), "dumb".to_owned()));
    values.sort_by(|left, right| left.0.cmp(&right.0));

    values
}

/// Write a raw cBPF program in the format `seccomp_export_bpf` produces.
///
/// Denying `socket`/`connect` also blocks connections to Unix sockets that
/// happen to live inside a read-only mount. Denying `io_uring_setup` prevents
/// using io_uring as a path-check side channel.
fn write_seccomp_filter() -> Result<File, SandboxError> {
    use std::io::{Seek, SeekFrom, Write};

    let mut file = tempfile::tempfile()
        .map_err(|error| SandboxError::Launch(format!("cannot create a seccomp file: {error}")))?;

    file.write_all(&seccomp_program()).map_err(|error| {
        SandboxError::Launch(format!("cannot write the seccomp filter: {error}"))
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        SandboxError::Launch(format!("cannot rewind the seccomp file: {error}"))
    })?;

    Ok(file)
}

const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const EPERM: u32 = 1;

/// The syscall numbers below are only meaningful for the architecture the
/// filter is built for. Without an explicit `seccomp_data.arch` check a process
/// entering through the i386 compat ABI on x86_64 (where `socket` is
/// `socketcall == 102` and `socket == 359`) would run with none of the numbers
/// matching and keep its sockets.
fn seccomp_program() -> Vec<u8> {
    let mut instructions = Vec::new();

    // Load seccomp_data.arch, skip the kill when it matches, then load
    // seccomp_data.nr.
    push_instruction(&mut instructions, 0x20, 0, 0, 4);
    push_instruction(&mut instructions, 0x15, 1, 0, target_audit_arch());
    push_instruction(&mut instructions, 0x06, 0, 0, SECCOMP_RET_KILL_PROCESS);
    push_instruction(&mut instructions, 0x20, 0, 0, 0);

    for syscall in denied_syscalls() {
        // if nr == syscall: fall through to the ERRNO return; else skip it.
        push_instruction(&mut instructions, 0x15, 0, 1, *syscall);
        push_instruction(&mut instructions, 0x06, 0, 0, SECCOMP_RET_ERRNO | EPERM);
    }

    push_instruction(&mut instructions, 0x06, 0, 0, SECCOMP_RET_ALLOW);

    instructions
}

/// The `AUDIT_ARCH_*` value in `seccomp_data.arch` for the target architecture.
fn target_audit_arch() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

        AUDIT_ARCH_X86_64
    }

    #[cfg(target_arch = "aarch64")]
    {
        const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;

        AUDIT_ARCH_AARCH64
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        // The sandbox is only supported on Linux x86_64/aarch64 for now.
        0
    }
}

fn push_instruction(out: &mut Vec<u8>, code: u16, jt: u8, jf: u8, k: u32) {
    out.extend_from_slice(&code.to_ne_bytes());
    out.push(jt);
    out.push(jf);
    out.extend_from_slice(&k.to_ne_bytes());
}

/// x86_64 and aarch64 syscall numbers for the socket family and io_uring.
fn denied_syscalls() -> &'static [u32] {
    #[cfg(target_arch = "x86_64")]
    {
        &[425, 41, 53, 42, 49, 50, 43, 288]
    }

    #[cfg(target_arch = "aarch64")]
    {
        &[425, 198, 199, 200, 201, 202, 203, 242]
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        // The sandbox is only supported on Linux x86_64/aarch64 for now.
        &[]
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);

        if is_executable(&candidate) {
            return Some(candidate);
        }
    }

    None
}

/// Space-separated descriptors to close in the wrapper before exec.
fn descriptor_list(keep: &[i32]) -> Result<String, SandboxError> {
    let entries = std::fs::read_dir("/proc/self/fd").map_err(|error| {
        SandboxError::Launch(format!("cannot enumerate inherited descriptors: {error}"))
    })?;
    let mut close = Vec::new();

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(fd) = name.parse::<i32>() else {
            continue;
        };

        if fd <= 2 || keep.contains(&fd) || close.contains(&fd) {
            continue;
        }

        close.push(fd);
    }

    close.sort_unstable();

    Ok(close
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(" "))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        SECCOMP_RET_ALLOW, SECCOMP_RET_KILL_PROCESS, denied_syscalls, parse_mountinfo,
        profile_args, sandbox_env, seccomp_program, target_audit_arch,
    };

    #[test]
    fn seccomp_program_is_a_filter_that_allows_by_default() {
        let program = seccomp_program();

        assert_eq!(program.len() % 8, 0);
        assert!(program.len() >= 24);

        let last = &program[program.len() - 4..];
        let value = u32::from_ne_bytes(last.try_into().unwrap());

        assert_eq!(value, SECCOMP_RET_ALLOW);
        assert!(denied_syscalls().contains(&425));
    }

    #[test]
    fn seccomp_program_kills_any_other_architecture_before_comparing_syscalls() {
        let program = seccomp_program();
        let code = |index: usize| {
            u16::from_ne_bytes(program[index * 8..index * 8 + 2].try_into().unwrap())
        };
        let word = |index: usize| {
            u32::from_ne_bytes(program[index * 8 + 4..index * 8 + 8].try_into().unwrap())
        };

        // BPF_LD|BPF_W|BPF_ABS of seccomp_data.arch, skipping one instruction
        // to the syscall filter only when the arch matches.
        assert_eq!(code(0), 0x20);
        assert_eq!(word(0), 4, "the first load must read seccomp_data.arch");
        assert_eq!(code(1), 0x15);
        assert_eq!(word(1), target_audit_arch());
        assert_eq!(code(2), 0x06);
        assert_eq!(word(2), SECCOMP_RET_KILL_PROCESS);

        // Only then is seccomp_data.nr loaded for the per-syscall comparisons.
        assert_eq!(code(3), 0x20);
        assert_eq!(word(3), 0, "the syscall check must read seccomp_data.nr");
    }

    #[test]
    fn mountinfo_paths_are_unescaped_and_ordered_parents_first() {
        let text = "36 25 8:3 / /run/user/1000 rw,nosuid - tmpfs tmpfs rw\n\
                    31 25 0:28 / /nix/store rw - ext4 /dev/nvme0n1p2 rw\n\
                    28 25 0:27 / /tmp/a\\040b rw - ext4 /dev/nvme0n1p2 rw\n\
                    25 1 0:26 / / rw - ext4 /dev/nvme0n1p2 rw\n";
        let mounts = parse_mountinfo(text);

        assert_eq!(
            mounts,
            vec![
                PathBuf::from("/nix/store"),
                PathBuf::from("/tmp/a b"),
                PathBuf::from("/run/user/1000"),
            ],
            "the root is bound separately and parents come first: {mounts:?}"
        );
    }

    #[test]
    fn the_profile_binds_the_host_then_scratch_then_read_only_roots() {
        let host = Path::new("/nix/store");
        let project = Path::new("/tmp/project");
        let args = profile_args(
            Path::new("/tmp/project"),
            "/run/current-system/sw/bin/bash",
            &["-c".to_owned(), "true".to_owned()],
            3,
            &[(4, host)],
            &[(5, project)],
            6,
        )
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        let position = |needle: &str| {
            args.iter()
                .position(|arg| arg == needle)
                .unwrap_or_else(|| panic!("missing {needle} in {args:?}"))
        };

        assert_eq!(args[0], "--die-with-parent");
        assert!(args.contains(&"--unshare-all".to_owned()));
        assert!(!args.contains(&"--clearenv".to_owned()), "{args:?}");
        assert!(!args.contains(&"--setenv".to_owned()), "{args:?}");
        assert_eq!(position("--ro-bind-fd"), 2);
        assert_eq!(args[position("--ro-bind-fd") + 1], "3");
        assert_eq!(args[position("--ro-bind-fd") + 2], "/");
        assert!(position("/nix/store") < position("--tmpfs"), "{args:?}");
        assert!(
            position("/tmp") < position("/tmp/project"),
            "the read-only project rebind must come after the scratch tmpfs: {args:?}"
        );
        assert_eq!(args[args.len() - 4], "--");
        assert_eq!(args[args.len() - 3], "/run/current-system/sw/bin/bash");
        assert_eq!(args[args.len() - 2], "-c");
        assert_eq!(args[args.len() - 1], "true");
    }

    #[test]
    fn the_sandbox_keeps_home_and_drops_session_sockets() {
        let mut env = std::collections::BTreeMap::new();

        env.insert("HOME".to_owned(), "/home/u".to_owned());
        env.insert("XDG_CONFIG_HOME".to_owned(), "/home/u/.config".to_owned());
        env.insert("SSH_AUTH_SOCK".to_owned(), "/run/ssh".to_owned());
        env.insert("OPENROUTER_API_KEY".to_owned(), "sk-or-secret".to_owned());
        env.insert("OPENCODE_GO_API_KEY".to_owned(), "oc_sk-secret".to_owned());

        let values = sandbox_env(&env);
        let get = |name: &str| {
            values
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.as_str())
        };

        assert_eq!(get("HOME"), Some("/home/u"));
        assert_eq!(get("XDG_CONFIG_HOME"), Some("/home/u/.config"));
        assert_eq!(get("TMPDIR"), Some("/tmp"));
        assert_eq!(get("TERM"), Some("dumb"));
        assert_eq!(get("SSH_AUTH_SOCK"), None);
        assert_eq!(get("OPENROUTER_API_KEY"), None);
        assert_eq!(get("OPENCODE_GO_API_KEY"), None);
    }

    #[test]
    fn nested_mounts_are_collapsed() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();

        let mounts = super::canonical_mounts(&[child.clone(), dir.path().to_path_buf()]).unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0], std::fs::canonicalize(dir.path()).unwrap());
    }
}
