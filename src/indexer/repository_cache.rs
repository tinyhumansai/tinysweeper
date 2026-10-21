//! A server-owned checkout cache shared by indexing and reviews.
//!
//! The stable path is selected by the server and is deliberately separate
//! from pull-request input. Repository identifiers must be canonical before
//! they are used as cache keys; remote URLs and credentials remain per-call
//! inputs to the hardened Git runner and are never persisted in the cache.
//!
//! A cache implementation must serialize operations for one repository,
//! fetch and verify the requested immutable commit before checking it out, and
//! keep each review's worktree disposable. Cancellation may drop a checkout
//! while Git is running, so subprocesses must remain kill-on-drop and stale
//! worktree registrations must be pruned before reuse. Submodule objects are
//! separate repositories: they are fetched only after a changed gitlink or an
//! actual lane read requires them, under the same host and allowlist checks as
//! the existing `Checkout` path.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Mutex as AsyncMutex;

use crate::error::Result;
use crate::indexer::fetch::Checkout;

/// Shared entry point for repository checkouts used by indexing and reviews.
///
/// Each canonical repository gets a private bare mirror. Immutable revisions
/// already present there are reused; each consumer receives a separate,
/// disposable exact-head worktree. Per-repository locking keeps fetch, prune,
/// and worktree registration changes from racing one another.
#[derive(Debug, Clone)]
pub struct RepositoryCache {
    root: PathBuf,
    locks: Arc<Mutex<std::collections::HashMap<PathBuf, Weak<AsyncMutex<()>>>>>,
}

impl RepositoryCache {
    /// Use `root` for this process's per-repository object mirrors.
    ///
    /// The directory is created with owner-only permissions on Unix. The path
    /// is supplied by trusted startup configuration, never repository config.
    pub fn at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|err| {
            crate::error::Error::Forge(format!("could not create repository cache: {err}"))
        })?;
        if std::fs::symlink_metadata(&root)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            return Err(crate::error::Error::config(
                "repository cache root must be a real directory",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).map_err(
                |err| {
                    crate::error::Error::Forge(format!("could not protect repository cache: {err}"))
                },
            )?;
        }
        Ok(Self {
            root,
            locks: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Resolve the trusted cache root from `TINYSWEEPER_REPOSITORY_CACHE_DIR`
    /// or the current user's private cache directory.
    pub fn from_env() -> Result<Self> {
        let configured = std::env::var_os("TINYSWEEPER_REPOSITORY_CACHE_DIR");
        let home = std::env::var_os("HOME");
        let root = cache_root(configured, home)?;
        Self::at(root)
    }

    /// The server-selected directory holding repository mirrors.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Check out one immutable commit for an indexing or review read.
    pub async fn checkout(
        &self,
        host: &str,
        repo: &str,
        revision: &str,
        token: &str,
    ) -> Result<Checkout> {
        let url = format!("https://{host}/{repo}.git");
        self.checkout_from_url_locked(repo, revision, None, &url, token, None)
            .await
    }

    /// Check out one exact head and cache its base commit when needed for
    /// changed-gitlink detection.
    pub async fn checkout_for_diff(
        &self,
        host: &str,
        repo: &str,
        revision: &str,
        base_revision: &str,
        token: &str,
    ) -> Result<Checkout> {
        validate_host(host)?;
        let url = format!("https://{host}/{repo}.git");
        self.checkout_from_url_locked(repo, revision, Some(base_revision), &url, token, None)
            .await
    }

    #[cfg(test)]
    async fn checkout_from_url(
        &self,
        repo: &str,
        revision: &str,
        url: &str,
        token: &str,
        fetch_count: &std::sync::atomic::AtomicUsize,
    ) -> Result<Checkout> {
        self.checkout_from_url_locked(repo, revision, None, url, token, Some(fetch_count))
            .await
    }

    async fn checkout_from_url_locked(
        &self,
        repo: &str,
        revision: &str,
        base_revision: Option<&str>,
        url: &str,
        token: &str,
        fetch_count: Option<&std::sync::atomic::AtomicUsize>,
    ) -> Result<Checkout> {
        validate_repo(repo)?;
        if let Some(base_revision) = base_revision {
            validate_revision(base_revision)?;
        }
        let mirror = self.mirror_path(repo)?;
        self.validate_existing_paths(repo, &mirror)?;
        let lock = self.lock_for(&mirror);
        let guard = lock.lock_owned().await;
        self.validate_existing_paths(repo, &mirror)?;
        let mut missing = Vec::new();
        if !Checkout::mirror_has_revision(&mirror, revision).await? {
            missing.push(revision);
        }
        if let Some(base_revision) = base_revision
            && base_revision != revision
            && !Checkout::mirror_has_revision(&mirror, base_revision).await?
        {
            missing.push(base_revision);
        }
        if !missing.is_empty() {
            if let Some(parent) = mirror.parent() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    crate::error::Error::Forge(format!("could not create cache owner path: {err}"))
                })?;
                protect_directory(parent)?;
            }
            std::fs::create_dir_all(&mirror).map_err(|err| {
                crate::error::Error::Forge(format!("could not create repository mirror: {err}"))
            })?;
            protect_directory(&mirror)?;
            Checkout::fetch_into_mirror(&mirror, url, &missing, token, fetch_count).await?;
        }
        let checkout = Checkout::from_mirror(&mirror, revision, guard).await?;
        Ok(checkout)
    }

    fn mirror_path(&self, repo: &str) -> Result<PathBuf> {
        let (owner, name) = repo.split_once('/').expect("validated owner/name");
        Ok(self.root.join(owner).join(format!("{name}.git")))
    }

    fn validate_existing_paths(&self, repo: &str, mirror: &Path) -> Result<()> {
        let owner = repo.split_once('/').expect("validated owner/name").0;
        let owner_path = self.root.join(owner);
        for path in [self.root.as_path(), owner_path.as_path(), mirror] {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(crate::error::Error::config(
                        "repository cache path contains a symlink or non-directory",
                    ));
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(crate::error::Error::Forge(format!(
                        "could not inspect repository cache path: {err}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn lock_for(&self, path: &Path) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().expect("repository cache lock map");
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
        lock
    }
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
    {
        return Err(crate::error::Error::config("invalid Git host"));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<()> {
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(crate::error::Error::config(format!(
            "`{revision}` is not a commit sha"
        )));
    }
    Ok(())
}

fn validate_repo(repo: &str) -> Result<()> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(crate::error::Error::config(format!(
            "`{repo}` is not owner/name"
        )));
    };
    let component = |value: &str| {
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if repo.starts_with('-') || !component(owner) || !component(name) || name.ends_with(".git") {
        return Err(crate::error::Error::config(format!(
            "`{repo}` is not owner/name"
        )));
    }
    Ok(())
}

fn protect_directory(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(crate::error::Error::config(
            "repository cache path contains a symlink or non-directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|err| {
            crate::error::Error::Forge(format!("could not protect repository cache path: {err}"))
        })?;
    }
    Ok(())
}

fn cache_root(configured: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(configured) = configured.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(configured));
    }
    let home = home.filter(|path| !path.is_empty()).ok_or_else(|| {
        crate::error::Error::config(
            "HOME or TINYSWEEPER_REPOSITORY_CACHE_DIR must be set for repository caching",
        )
    })?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("tinysweeper")
        .join("repositories"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::tree::TreeReader;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn git(args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .output()
            .expect("run fixture Git command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn local_bare_repository(root: &Path) -> (PathBuf, String) {
        let source = root.join("source");
        let bare = root.join("owner-repo.git");
        std::fs::create_dir_all(&source).expect("create fixture repository");
        git(&["init", "--quiet", source.to_str().unwrap()]);
        std::fs::write(source.join("README.md"), "fixture head\n").expect("write fixture tree");
        git(&["-C", source.to_str().unwrap(), "add", "README.md"]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture head",
        ]);
        let revision = git(&["-C", source.to_str().unwrap(), "rev-parse", "HEAD"]);
        git(&[
            "clone",
            "--quiet",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ]);
        (bare, revision)
    }

    #[tokio::test]
    async fn two_reviews_of_the_same_repository_fetch_shared_objects_once() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let (remote, revision) = local_bare_repository(fixture.path());
        let cache = RepositoryCache::at(fixture.path().join("cache")).expect("cache root");
        let remote = remote.to_str().unwrap();
        let fetches = AtomicUsize::new(0);

        let first = cache
            .checkout_from_url("acme/widgets", &revision, remote, "", &fetches)
            .await
            .expect("first review checkout");
        let second = cache
            .checkout_from_url("acme/widgets", &revision, remote, "", &fetches)
            .await
            .expect("second review checkout");

        assert_eq!(first.revision(), revision);
        assert_eq!(second.revision(), revision);
        assert_eq!(
            std::fs::read(first.path().join("README.md")).unwrap(),
            b"fixture head\n"
        );
        assert_eq!(
            std::fs::read(second.path().join("README.md")).unwrap(),
            b"fixture head\n"
        );
        assert!(first.path().join(".git").is_file());
        let tree = crate::ports::tree::DirTree::new(first.path());
        assert_eq!(
            tree.lookup(&crate::ports::tree::Lookup::Read {
                path: ".git".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap(),
            crate::ports::tree::Found::NotFound
        );
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "the production Git fetch command should contact the local bare remote once"
        );
    }

    #[tokio::test]
    async fn concurrent_reviews_of_the_same_repository_share_one_fetch() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let (remote, revision) = local_bare_repository(fixture.path());
        let cache = RepositoryCache::at(fixture.path().join("cache")).expect("cache root");
        let remote = remote.to_str().unwrap();
        let fetches = AtomicUsize::new(0);

        let (first, second) = tokio::join!(
            cache.checkout_from_url("acme/widgets", &revision, remote, "", &fetches),
            cache.checkout_from_url("acme/widgets", &revision, remote, "", &fetches),
        );
        let first = first.expect("first concurrent review checkout");
        let second = second.expect("second concurrent review checkout");

        assert_eq!(first.revision(), revision);
        assert_eq!(second.revision(), revision);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_new_head_is_fetched_and_checked_out_from_the_existing_mirror() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let (remote, first_revision) = local_bare_repository(fixture.path());
        let source = fixture.path().join("source");
        std::fs::write(source.join("README.md"), "new fixture head\n").unwrap();
        git(&["-C", source.to_str().unwrap(), "add", "README.md"]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "new fixture head",
        ]);
        let second_revision = git(&["-C", source.to_str().unwrap(), "rev-parse", "HEAD"]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "push",
            "--quiet",
            remote.to_str().unwrap(),
            "HEAD",
        ]);
        let cache_root = fixture.path().join("cache");
        let cache = RepositoryCache::at(&cache_root).expect("cache root");
        let remote = remote.to_str().unwrap();
        let fetches = AtomicUsize::new(0);
        let old = cache
            .checkout_from_url("acme/widgets", &first_revision, remote, "", &fetches)
            .await
            .expect("original head checkout");
        let refreshed = cache
            .checkout_from_url("acme/widgets", &second_revision, remote, "", &fetches)
            .await
            .expect("new head checkout");

        assert_eq!(old.revision(), first_revision);
        assert_eq!(refreshed.revision(), second_revision);
        assert_eq!(
            std::fs::read(refreshed.path().join("README.md")).unwrap(),
            b"new fixture head\n"
        );
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn read_credentials_are_not_persisted_in_the_mirror_configuration() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let (remote, revision) = local_bare_repository(fixture.path());
        let root = fixture.path().join("cache");
        let cache = RepositoryCache::at(&root).expect("cache root");
        let token = "cache-token-must-not-persist";
        cache
            .checkout_from_url(
                "acme/widgets",
                &revision,
                remote.to_str().unwrap(),
                token,
                &AtomicUsize::new(0),
            )
            .await
            .expect("checkout with ephemeral read credential");

        let config = std::fs::read_to_string(root.join("acme/widgets.git/config")).unwrap();
        assert!(!config.contains(token));
        assert!(!config.contains("http.extraHeader"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_cache_owner_and_mirror_symlinks_are_refused_before_git_access() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let (external_mirror, revision) = local_bare_repository(fixture.path());
        let external_owner = fixture.path().join("external-owner");
        std::fs::create_dir_all(&external_owner).unwrap();
        let owner_mirror = external_owner.join("widgets.git");
        std::fs::rename(&external_mirror, &owner_mirror).unwrap();

        let owner_cache_root = fixture.path().join("owner-cache");
        std::fs::create_dir_all(&owner_cache_root).unwrap();
        std::os::unix::fs::symlink(&external_owner, owner_cache_root.join("acme")).unwrap();
        let owner_cache = RepositoryCache::at(&owner_cache_root).unwrap();
        assert!(
            owner_cache
                .checkout_from_url(
                    "acme/widgets",
                    &revision,
                    owner_mirror.to_str().unwrap(),
                    "",
                    &AtomicUsize::new(0),
                )
                .await
                .is_err()
        );
        assert_eq!(
            git(&[
                "--git-dir",
                owner_mirror.to_str().unwrap(),
                "worktree",
                "list",
                "--porcelain"
            ])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
            1,
            "refusing an owner symlink must not register a worktree externally"
        );

        let mirror_cache_root = fixture.path().join("mirror-cache");
        let mirror_owner = mirror_cache_root.join("acme");
        std::fs::create_dir_all(&mirror_owner).unwrap();
        std::os::unix::fs::symlink(&owner_mirror, mirror_owner.join("widgets.git")).unwrap();
        let mirror_cache = RepositoryCache::at(&mirror_cache_root).unwrap();
        assert!(
            mirror_cache
                .checkout_from_url(
                    "acme/widgets",
                    &revision,
                    owner_mirror.to_str().unwrap(),
                    "",
                    &AtomicUsize::new(0),
                )
                .await
                .is_err()
        );
        assert_eq!(
            git(&[
                "--git-dir",
                owner_mirror.to_str().unwrap(),
                "worktree",
                "list",
                "--porcelain"
            ])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
            1,
            "refusing a mirror symlink must not prune or register external admin state"
        );
    }

    #[test]
    fn cache_root_override_wins_and_empty_override_uses_the_private_user_cache() {
        assert_eq!(
            cache_root(
                Some("/var/cache/tinysweeper".into()),
                Some("/home/test".into())
            )
            .unwrap(),
            PathBuf::from("/var/cache/tinysweeper")
        );
        assert_eq!(
            cache_root(Some("".into()), Some("/home/test".into())).unwrap(),
            PathBuf::from("/home/test/.cache/tinysweeper/repositories")
        );
        assert!(cache_root(None, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_root_is_private_to_the_server_user() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp directory");
        let root = temp.path().join("cache");
        RepositoryCache::at(&root).expect("cache root");
        assert_eq!(
            std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
