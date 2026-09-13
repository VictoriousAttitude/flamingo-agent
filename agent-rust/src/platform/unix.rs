//! Unix implementation: root-owned `0700` directory and `0600` file, `geteuid` privilege
//! check, no self-elevation (the user runs with `sudo`).

use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::PlatformError;

/// File name of the child binary next to the agent executable.
pub const CHILD_BINARY_NAME: &str = "logger-child";

/// There is no service control manager here, so `--install`/`--uninstall` need no privilege;
/// they report the operation as unsupported instead.
pub const SERVICE_MANAGEMENT_NEEDS_PRIVILEGE: bool = false;

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// `/var/log/flamingo-agent`.
pub fn default_log_dir() -> PathBuf {
    PathBuf::from("/var/log/flamingo-agent")
}

/// True when the effective user is root.
pub fn is_privileged() -> Result<bool, PlatformError> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    Ok(unsafe { libc::geteuid() } == 0)
}

/// There is no portable way to acquire root from a running process; the caller must use sudo.
pub fn relaunch_privileged(_args: &[OsString]) -> Result<i32, PlatformError> {
    Err(PlatformError::Unsupported("self-elevation"))
}

/// Arrange for the kernel to kill the child if this process dies for any reason, including
/// `SIGKILL` and crashes, which `kill_on_drop` cannot cover because nothing runs. On Linux
/// the parent-death signal is armed in the child between `fork` and `exec`; other Unix
/// targets have no equivalent and get no binding.
pub fn prepare_child(command: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id();
        // SAFETY: the closure runs in the forked child before `exec` and calls only
        // async-signal-safe functions (`prctl`, `getppid`, `_exit`).
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent can die between `fork` and the `prctl` above, in which case the
                // signal was armed too late: check, and leave rather than run unbound.
                if libc::getppid() as u32 != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
    }
}

/// Nothing to do after the spawn on Unix: the binding is armed by [`prepare_child`].
pub fn bind_child(_child: &tokio::process::Child) -> Result<(), PlatformError> {
    Ok(())
}

/// Refuse an existing path that could have been planted by another user before the agent's
/// first start: a symbolic link (which would redirect root's writes anywhere) or an object
/// owned by someone other than the effective user. The agent never takes such a path over.
fn assert_trusted_existing(path: &Path) -> Result<(), PlatformError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(PlatformError::io("inspecting existing log location", err)),
    };
    if meta.file_type().is_symlink() {
        return Err(untrusted(path, "a symbolic link"));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(untrusted(path, "owned by another user"));
    }
    Ok(())
}

fn untrusted(path: &Path, reason: &'static str) -> PlatformError {
    PlatformError::Untrusted {
        path: path.display().to_string(),
        reason,
    }
}

/// Create the directory (and parents) with mode 0700, tighten it if it already exists,
/// and hand it to root when running as root. An existing directory that is a symbolic link
/// or belongs to another user is refused (see [`assert_trusted_existing`]).
pub fn secure_dir(path: &Path) -> Result<(), PlatformError> {
    assert_trusted_existing(path)?;
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)
        .map_err(|e| PlatformError::io("creating log directory", e))?;
    fs::set_permissions(path, Permissions::from_mode(DIR_MODE))
        .map_err(|e| PlatformError::io("setting log directory mode", e))?;
    chown_root_if_privileged(path)
}

/// Create the file with mode 0600 (born locked), tighten it if it already exists without
/// touching its content, and hand it to root when running as root. An existing file that is
/// a symbolic link or belongs to another user is refused (see [`assert_trusted_existing`]).
pub fn secure_file(path: &Path) -> Result<(), PlatformError> {
    assert_trusted_existing(path)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .open(path)
        .map_err(|e| PlatformError::io("creating child log file", e))?;
    fs::set_permissions(path, Permissions::from_mode(FILE_MODE))
        .map_err(|e| PlatformError::io("setting child log mode", e))?;
    chown_root_if_privileged(path)
}

/// Human-readable protection summary, e.g. `mode=0600 uid=0 gid=0`.
pub fn describe_protection(path: &Path) -> Result<String, PlatformError> {
    let meta = fs::metadata(path).map_err(|e| PlatformError::io("reading file metadata", e))?;
    Ok(format!(
        "mode={:04o} uid={} gid={}",
        meta.mode() & 0o7777,
        meta.uid(),
        meta.gid()
    ))
}

fn chown_root_if_privileged(path: &Path) -> Result<(), PlatformError> {
    if is_privileged()? {
        std::os::unix::fs::chown(path, Some(0), Some(0))
            .map_err(|e| PlatformError::io("changing owner to root", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// A planted symbolic link must be refused, not followed: with root's privileges the
    /// agent would otherwise lock and write wherever the planter pointed it.
    #[test]
    fn secure_dir_refuses_a_symbolic_link() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real");
        fs::create_dir(&target).unwrap();
        let link = tmp.path().join("planted");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = secure_dir(&link).unwrap_err();
        assert!(
            matches!(
                err,
                PlatformError::Untrusted {
                    reason: "a symbolic link",
                    ..
                }
            ),
            "{err}"
        );
        assert!(
            link.is_symlink() && target.is_dir(),
            "the link must be left untouched"
        );
    }

    #[test]
    fn secure_file_refuses_a_symbolic_link() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real.log");
        fs::write(&target, "planted\n").unwrap();
        let link = tmp.path().join("child.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = secure_file(&link).unwrap_err();
        assert!(
            matches!(
                err,
                PlatformError::Untrusted {
                    reason: "a symbolic link",
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "planted\n");
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn secure_dir_creates_with_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        secure_dir(&dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn secure_dir_tightens_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        secure_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn secure_file_creates_with_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("child.log");
        secure_file(&file).unwrap();
        assert!(file.is_file());
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn secure_file_tightens_existing_file_and_keeps_content() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("child.log");
        fs::write(&file, "keep me\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        secure_file(&file).unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep me\n");
    }

    #[test]
    fn describe_protection_reports_mode_and_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("child.log");
        secure_file(&file).unwrap();
        let description = describe_protection(&file).unwrap();
        assert!(description.starts_with("mode=0600 uid="), "{description}");
    }

    #[test]
    fn relaunch_is_unsupported() {
        assert!(matches!(
            relaunch_privileged(&[]),
            Err(PlatformError::Unsupported(_))
        ));
    }

    #[test]
    fn default_log_dir_is_under_var_log() {
        assert_eq!(default_log_dir(), Path::new("/var/log/flamingo-agent"));
    }
}
