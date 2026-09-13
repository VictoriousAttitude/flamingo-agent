//! Size-based log rotation with a bounded number of kept files (design §8).
//!
//! The live file keeps its name (`agent.log`, `child.log`); on rotation it is renamed to
//! `<stem>.1.log`, older generations shift up by one, and the one past `keep` is deleted, so
//! a log never occupies more than `(keep + 1) * max_bytes` on disk. A rename preserves the
//! file's permissions, so rotated generations stay as locked as the live file was.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::platform;

/// When to rotate and how many rotated generations to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    /// Rotate before a write would take the live file past this size.
    pub max_bytes: u64,
    /// Rotated generations to keep (`<stem>.1.log` … `<stem>.<keep>.log`).
    pub keep: u32,
}

/// `dir/agent.log` with index 3 is `dir/agent.3.log`; a path without an extension gets
/// the index appended (`dir/agent.3`).
pub fn rotated_name(path: &Path, index: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.{index}.{}", ext.to_string_lossy()),
        None => format!("{stem}.{index}"),
    };
    path.with_file_name(name)
}

/// Shift the generations up by one and rename the live file to generation 1. A missing
/// live file is not an error (there is nothing to rotate). The caller creates the next
/// live file; on Unix the rename of an open file is always allowed, and on Windows it is
/// allowed because the standard library opens files with `FILE_SHARE_DELETE`.
pub fn rotate(path: &Path, keep: u32) -> io::Result<()> {
    if keep == 0 {
        return match fs::remove_file(path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
    }
    let oldest = rotated_name(path, keep);
    match fs::remove_file(&oldest) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    for index in (1..keep).rev() {
        let from = rotated_name(path, index);
        if from.exists() {
            fs::rename(&from, rotated_name(path, index + 1))?;
        }
    }
    match fs::rename(path, rotated_name(path, 1)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Rotate `path` if it has reached the policy's size. Returns whether it was rotated.
pub fn rotate_if_large(path: &Path, policy: &RotationPolicy) -> io::Result<bool> {
    let size = match fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if size < policy.max_bytes {
        return Ok(false);
    }
    rotate(path, policy.keep)?;
    Ok(true)
}

/// An append-only writer for the live file that rotates it by size. Every rotation is
/// best-effort: if the rename or the reopen fails, writing continues into the current file
/// rather than losing lines, and the next write tries again.
#[derive(Debug)]
pub struct RotatingWriter {
    path: PathBuf,
    policy: RotationPolicy,
    file: File,
    size: u64,
}

impl RotatingWriter {
    /// Open (creating if needed, born locked) the live file for appending.
    pub fn open(path: &Path, policy: RotationPolicy) -> io::Result<Self> {
        let file = open_locked(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            policy,
            file,
            size,
        })
    }

    fn rotate_now(&mut self) -> io::Result<()> {
        rotate(&self.path, self.policy.keep)?;
        self.file = open_locked(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

/// Create the file with the platform's lock applied (0600 / protected DACL) and open it
/// for appending.
fn open_locked(path: &Path) -> io::Result<File> {
    platform::secure_file(path).map_err(|err| io::Error::other(err.to_string()))?;
    OpenOptions::new().append(true).open(path)
}

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.policy.max_bytes {
            // A failed rotation keeps the current file; the write below still happens.
            let _ = self.rotate_now();
        }
        self.file.write_all(buf)?;
        self.size += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_bytes: u64, keep: u32) -> RotationPolicy {
        RotationPolicy { max_bytes, keep }
    }

    #[test]
    fn rotated_names_keep_the_extension() {
        assert_eq!(
            rotated_name(Path::new("/x/agent.log"), 3),
            PathBuf::from("/x/agent.3.log")
        );
        assert_eq!(
            rotated_name(Path::new("/x/agent"), 1),
            PathBuf::from("/x/agent.1")
        );
    }

    /// Three rotations with keep = 2: the newest content is always in `.1`, the oldest
    /// generation falls off the end, and the live file is gone until recreated.
    #[test]
    fn rotate_shifts_generations_and_drops_the_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("agent.log");
        for content in ["first", "second", "third"] {
            fs::write(&live, content).unwrap();
            rotate(&live, 2).unwrap();
            assert!(!live.exists(), "live file must have been renamed away");
        }
        assert_eq!(fs::read_to_string(rotated_name(&live, 1)).unwrap(), "third");
        assert_eq!(
            fs::read_to_string(rotated_name(&live, 2)).unwrap(),
            "second"
        );
        assert!(
            !rotated_name(&live, 3).exists(),
            "generation 3 exceeds keep = 2"
        );
    }

    #[test]
    fn rotate_with_keep_zero_deletes_the_live_file() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("agent.log");
        fs::write(&live, "gone").unwrap();
        rotate(&live, 0).unwrap();
        assert!(!live.exists());
        assert!(!rotated_name(&live, 1).exists());
    }

    #[test]
    fn rotating_a_missing_file_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        rotate(&tmp.path().join("absent.log"), 3).unwrap();
        assert!(!rotate_if_large(&tmp.path().join("absent.log"), &policy(10, 3)).unwrap());
    }

    #[test]
    fn rotate_if_large_only_rotates_at_the_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("child.log");
        fs::write(&live, "123456789").unwrap();
        assert!(!rotate_if_large(&live, &policy(10, 1)).unwrap());
        assert!(live.exists());
        fs::write(&live, "1234567890").unwrap();
        assert!(rotate_if_large(&live, &policy(10, 1)).unwrap());
        assert!(!live.exists());
        assert_eq!(
            fs::read_to_string(rotated_name(&live, 1)).unwrap(),
            "1234567890"
        );
    }

    /// Lines never straddle a rotation: a write that would exceed the limit goes into a
    /// fresh file, and no byte is lost across the generations.
    #[test]
    fn writer_rotates_before_exceeding_the_limit_and_loses_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("agent.log");
        // 13-byte lines, a 27-byte limit: two lines per generation, six lines in total.
        let mut w = RotatingWriter::open(&live, policy(27, 2)).unwrap();
        for i in 0..6 {
            w.write_all(format!("line {i} xxxxx\n").as_bytes()).unwrap();
        }
        w.flush().unwrap();
        let live_text = fs::read_to_string(&live).unwrap();
        let gen1 = fs::read_to_string(rotated_name(&live, 1)).unwrap();
        let gen2 = fs::read_to_string(rotated_name(&live, 2)).unwrap();
        assert!(!rotated_name(&live, 3).exists());
        for text in [&live_text, &gen1, &gen2] {
            assert!(text.len() <= 27, "{text:?} exceeds the limit");
            assert_eq!(text.lines().count(), 2, "{text:?}");
            assert!(
                text.ends_with('\n'),
                "a line straddled a rotation: {text:?}"
            );
        }
        // The two newest generations plus the live file hold the last six lines.
        let all = format!("{gen2}{gen1}{live_text}");
        assert_eq!(all.lines().count(), 6);
        assert!(all.starts_with("line 0"));
        assert!(all.ends_with("line 5 xxxxx\n"));
    }

    #[test]
    fn writer_appends_to_an_existing_file_and_counts_its_size() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("agent.log");
        fs::write(&live, "existing\n").unwrap();
        let mut w = RotatingWriter::open(&live, policy(1 << 20, 1)).unwrap();
        assert_eq!(w.size, 9);
        w.write_all(b"more\n").unwrap();
        assert_eq!(fs::read_to_string(&live).unwrap(), "existing\nmore\n");
    }
}
