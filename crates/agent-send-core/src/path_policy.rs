//! Filesystem path policy for scoped folder capabilities.

use std::fs;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::FolderDirection;

/// The filesystem operation a caller is requesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathOperation {
    Read,
    Write,
}

/// Errors returned while validating or resolving a capability path.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathPolicyError {
    #[error("path must not be empty")]
    EmptyPath,
    #[error("absolute paths are not allowed")]
    AbsolutePath,
    #[error("path traversal is not allowed")]
    Traversal,
    #[error("path contains an unsupported component")]
    InvalidPath,
    #[error("the configured folder is unavailable")]
    RootUnavailable,
    #[error("the requested path is unavailable")]
    PathUnavailable,
    #[error("the requested path escapes the configured folder through a symlink")]
    SymlinkEscape,
    #[error("the folder does not allow read access")]
    ReadNotAllowed,
    #[error("the folder does not allow write access")]
    WriteNotAllowed,
}

/// A filesystem capability rooted at one configured directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPolicy {
    root: PathBuf,
    direction: FolderDirection,
}

impl PathPolicy {
    pub fn new(root: impl Into<PathBuf>, direction: FolderDirection) -> Self {
        Self {
            root: root.into(),
            direction,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn direction(&self) -> &FolderDirection {
        &self.direction
    }

    /// Validate a path without touching the filesystem.
    pub fn validate_relative(path: &Path) -> Result<(), PathPolicyError> {
        if path.as_os_str().is_empty() {
            return Err(PathPolicyError::EmptyPath);
        }
        if path.to_string_lossy().contains('\0') {
            return Err(PathPolicyError::InvalidPath);
        }

        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(PathPolicyError::AbsolutePath)
                }
                Component::ParentDir => return Err(PathPolicyError::Traversal),
                Component::CurDir | Component::Normal(_) => {}
            }
        }
        Ok(())
    }

    /// Resolve a relative path while enforcing the capability direction and root.
    ///
    /// Reads must name an existing path. Writes may name a new file or directory,
    /// but every existing parent is checked, so symlinks cannot escape the root.
    pub fn resolve(
        &self,
        path: impl AsRef<Path>,
        operation: PathOperation,
    ) -> Result<PathBuf, PathPolicyError> {
        let path = path.as_ref();
        Self::validate_relative(path)?;
        self.check_direction(operation)?;

        let root = fs::canonicalize(&self.root).map_err(|_| PathPolicyError::RootUnavailable)?;
        if !root.is_dir() {
            return Err(PathPolicyError::RootUnavailable);
        }
        let candidate = root.join(path);

        match operation {
            PathOperation::Read => {
                let resolved =
                    fs::canonicalize(&candidate).map_err(|_| PathPolicyError::PathUnavailable)?;
                ensure_beneath(&root, &resolved)?;
                Ok(resolved)
            }
            PathOperation::Write => {
                let existing = nearest_existing(&candidate)?;
                let resolved_existing =
                    fs::canonicalize(existing).map_err(|_| PathPolicyError::PathUnavailable)?;
                ensure_beneath(&root, &resolved_existing)?;
                Ok(candidate)
            }
        }
    }

    fn check_direction(&self, operation: PathOperation) -> Result<(), PathPolicyError> {
        match (operation, &self.direction) {
            (PathOperation::Read, FolderDirection::Write) => Err(PathPolicyError::ReadNotAllowed),
            (PathOperation::Write, FolderDirection::Read) => Err(PathPolicyError::WriteNotAllowed),
            _ => Ok(()),
        }
    }
}

fn ensure_beneath(root: &Path, path: &Path) -> Result<(), PathPolicyError> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(PathPolicyError::SymlinkEscape)
    }
}

fn nearest_existing(path: &Path) -> Result<&Path, PathPolicyError> {
    let mut current = path;
    while !current.exists() {
        current = current.parent().ok_or(PathPolicyError::PathUnavailable)?;
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let id = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-send-path-policy-{}-{id}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preserves_valid_nested_paths_for_reads_and_new_writes() {
        let root = TestRoot::new();
        fs::create_dir(root.0.join("nested")).unwrap();
        fs::write(root.0.join("nested/read.txt"), b"ok").unwrap();
        let policy = PathPolicy::new(&root.0, FolderDirection::ReadWrite);

        assert_eq!(
            policy
                .resolve("nested/read.txt", PathOperation::Read)
                .unwrap(),
            fs::canonicalize(root.0.join("nested/read.txt")).unwrap()
        );
        assert_eq!(
            policy
                .resolve("nested/new.txt", PathOperation::Write)
                .unwrap(),
            root.0.canonicalize().unwrap().join("nested/new.txt")
        );
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert_eq!(
            PathPolicy::validate_relative(Path::new("nested/../secret")),
            Err(PathPolicyError::Traversal)
        );
        assert_eq!(
            PathPolicy::validate_relative(Path::new("/tmp/secret")),
            Err(PathPolicyError::AbsolutePath)
        );
    }

    #[test]
    fn rejects_symlink_escape() {
        let root = TestRoot::new();
        let outside = TestRoot::new();
        fs::write(outside.0.join("secret.txt"), b"secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside.0, root.0.join("link")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&outside.0, root.0.join("link")).unwrap();

        let policy = PathPolicy::new(&root.0, FolderDirection::ReadWrite);
        assert_eq!(
            policy.resolve("link/secret.txt", PathOperation::Read),
            Err(PathPolicyError::SymlinkEscape)
        );
        assert_eq!(
            policy.resolve("link/new.txt", PathOperation::Write),
            Err(PathPolicyError::SymlinkEscape)
        );
    }

    #[test]
    fn rejects_unsupported_directions() {
        let root = TestRoot::new();
        fs::write(root.0.join("file"), b"ok").unwrap();
        assert_eq!(
            PathPolicy::new(&root.0, FolderDirection::Read).resolve("new", PathOperation::Write),
            Err(PathPolicyError::WriteNotAllowed)
        );
        assert_eq!(
            PathPolicy::new(&root.0, FolderDirection::Write).resolve("file", PathOperation::Read),
            Err(PathPolicyError::ReadNotAllowed)
        );
    }
}
