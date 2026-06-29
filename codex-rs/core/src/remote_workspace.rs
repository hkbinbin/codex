//! Remote workspace initialization.
//!
//! When a turn runs against a remote exec-server, the client's local working
//! directory does not exist on the server, so using it verbatim as the command
//! `cwd` fails (the server rejects it with "no such directory"). To make remote
//! execution work transparently, the client mirrors its local working directory
//! into a fresh directory on the server when the environment is first resolved,
//! and then uses that server-side directory as the `cwd` for the turn.
//!
//! The mirror is intentionally shallow on cost: large build/VCS directories are
//! skipped and very large files are not uploaded, so a typical workspace syncs
//! quickly without shipping multi-gigabyte build artifacts over the wire.

use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;

use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::Environment;
use codex_exec_server::ExecServerError;
use codex_utils_path_uri::PathUri;

/// Directory names that are never uploaded to the remote workspace. These are
/// large, machine-specific, or regenerable, so copying them would be slow and
/// pointless.
const SKIPPED_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
];

/// Files larger than this are skipped to avoid shipping huge binaries/artifacts
/// over the connection during workspace initialization.
const MAX_UPLOAD_FILE_BYTES: u64 = 25 * 1024 * 1024;

/// Caps the total number of filesystem entries visited so a pathological
/// workspace cannot stall turn startup indefinitely.
const MAX_ENTRIES: usize = 50_000;

/// Mirrors `local_cwd` into a fresh directory on the remote `environment` and
/// returns the remote workspace directory as a [`PathUri`].
///
/// The remote directory is rooted at the exec-server's own working directory
/// (`server_cwd`) so the client does not need to know any server-specific
/// absolute path. The leaf name is derived from the local workspace path so
/// reconnecting with the same local workspace reuses the same remote directory.
pub(crate) async fn initialize_remote_workspace(
    environment: &Environment,
    server_cwd: &PathUri,
    local_cwd: &PathUri,
) -> Result<PathUri, ExecServerError> {
    let local_root = local_cwd.to_abs_path().map_err(|err| {
        ExecServerError::Protocol(format!("remote workspace: invalid local cwd: {err}"))
    })?;
    let local_root: PathBuf = local_root.into();

    let workspace_dir_name = remote_workspace_dir_name(&local_root);
    let remote_root = server_cwd.join(&workspace_dir_name).map_err(|err| {
        ExecServerError::Protocol(format!(
            "remote workspace: failed to build remote path under `{server_cwd}`: {err}"
        ))
    })?;

    let filesystem = environment.get_filesystem();

    // Create the remote workspace root up front so an empty local workspace
    // still yields a usable cwd.
    filesystem
        .create_directory(
            &remote_root,
            CreateDirectoryOptions { recursive: true },
            /*sandbox*/ None,
        )
        .await
        .map_err(|err| {
            ExecServerError::Protocol(format!(
                "remote workspace: failed to create remote dir `{remote_root}`: {err}"
            ))
        })?;

    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(local_root.clone());
    let mut visited = 0usize;

    while let Some(dir) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!(
                    "remote workspace: skipping unreadable dir `{}`: {err}",
                    dir.display()
                );
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::warn!("remote workspace: skipping unreadable entry: {err}");
                    continue;
                }
            };
            visited += 1;
            if visited > MAX_ENTRIES {
                tracing::warn!(
                    "remote workspace: exceeded {MAX_ENTRIES} entries; remaining files were not uploaded"
                );
                return Ok(remote_root);
            }

            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) => {
                    tracing::warn!(
                        "remote workspace: skipping `{}` (cannot stat): {err}",
                        path.display()
                    );
                    continue;
                }
            };

            let name = entry.file_name();
            let name = name.to_string_lossy();

            let Some(relative) = path
                .strip_prefix(&local_root)
                .ok()
                .and_then(relative_to_unix)
            else {
                continue;
            };
            let remote_path = match remote_root.join(&relative) {
                Ok(remote_path) => remote_path,
                Err(err) => {
                    tracing::warn!(
                        "remote workspace: skipping `{relative}` (bad remote path): {err}"
                    );
                    continue;
                }
            };

            if file_type.is_dir() {
                if SKIPPED_DIR_NAMES.contains(&name.as_ref()) {
                    continue;
                }
                if let Err(err) = filesystem
                    .create_directory(
                        &remote_path,
                        CreateDirectoryOptions { recursive: true },
                        /*sandbox*/ None,
                    )
                    .await
                {
                    tracing::warn!(
                        "remote workspace: failed to create remote dir `{remote_path}`: {err}"
                    );
                    continue;
                }
                queue.push_back(path);
            } else if file_type.is_file() {
                if let Ok(metadata) = entry.metadata()
                    && metadata.len() > MAX_UPLOAD_FILE_BYTES
                {
                    tracing::debug!(
                        "remote workspace: skipping large file `{relative}` ({} bytes)",
                        metadata.len()
                    );
                    continue;
                }
                let contents = match std::fs::read(&path) {
                    Ok(contents) => contents,
                    Err(err) => {
                        tracing::warn!(
                            "remote workspace: skipping `{}` (read failed): {err}",
                            path.display()
                        );
                        continue;
                    }
                };
                if let Err(err) = filesystem
                    .write_file(&remote_path, contents, /*sandbox*/ None)
                    .await
                {
                    tracing::warn!(
                        "remote workspace: failed to write remote file `{remote_path}`: {err}"
                    );
                }
            }
            // Symlinks and other special files are intentionally ignored.
        }
    }

    tracing::info!(
        "remote workspace ready at `{remote_root}` (mirrored from `{}`)",
        local_root.display()
    );
    Ok(remote_root)
}

/// Builds a stable, filesystem-safe directory name for the remote workspace
/// derived from the local workspace path. Reusing the same name for the same
/// local path lets reconnects reuse the previously uploaded directory.
fn remote_workspace_dir_name(local_root: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hash;
    use std::hash::Hasher;

    let mut hasher = DefaultHasher::new();
    local_root.hash(&mut hasher);
    let hash = hasher.finish();

    let leaf = local_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    let sanitized: String = leaf
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    format!("codex-workspace-{sanitized}-{hash:016x}")
}

/// Converts a relative path into a forward-slash-joined string suitable for
/// [`PathUri::join`], which treats `/` as the segment separator regardless of
/// the host convention.
fn relative_to_unix(relative: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            // Anything that is not a plain name (root, prefix, `.`, `..`) should
            // not appear in a relative workspace path; bail out to be safe.
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_workspace_dir_name_is_stable_and_sanitized() {
        let path = Path::new("/home/user/My Project!");
        let first = remote_workspace_dir_name(path);
        let second = remote_workspace_dir_name(path);
        assert_eq!(first, second, "name should be deterministic");
        assert!(first.starts_with("codex-workspace-"));
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "name should be filesystem-safe: {first}"
        );
        assert!(
            !first.contains(' ') && !first.contains('!'),
            "special characters should be sanitized: {first}"
        );
    }

    #[test]
    fn remote_workspace_dir_name_differs_per_path() {
        let a = remote_workspace_dir_name(Path::new("/home/user/project-a"));
        let b = remote_workspace_dir_name(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn relative_to_unix_joins_with_forward_slashes() {
        let joined = relative_to_unix(Path::new("src").join("inner").join("file.rs").as_path());
        assert_eq!(joined.as_deref(), Some("src/inner/file.rs"));
    }

    #[test]
    fn relative_to_unix_rejects_parent_traversal() {
        assert_eq!(
            relative_to_unix(Path::new("..").join("escape").as_path()),
            None
        );
    }

    #[test]
    fn relative_to_unix_rejects_empty() {
        assert_eq!(relative_to_unix(Path::new("")), None);
    }
}
