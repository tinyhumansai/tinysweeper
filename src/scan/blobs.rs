//! The committed-junk scanner: large blobs, build output and vendored noise.
//!
//! Deterministic, offline, and concerned only with files this pull request
//! **added**. A repository that already vendors its dependencies has made that
//! choice; flagging it on every unrelated pull request would be nagging, not
//! reviewing. What matters is the moment junk *enters* the history, because
//! that is the only moment removing it is cheap.

use crate::config::types::Severity;
use crate::forge::types::{ChangedFile, FileStatus};
use crate::scan::types::{Finding, ScanKind};

/// Path fragments that mean "this is build output or a dependency tree".
///
/// Matched as path components so `src/nodes/` does not trip `node_modules`.
const JUNK_DIRECTORIES: &[(&str, &str)] = &[
    ("node_modules", "an npm dependency tree"),
    ("target", "Rust build output"),
    ("__pycache__", "Python bytecode"),
    (".venv", "a Python virtual environment"),
    ("venv", "a Python virtual environment"),
    (".terraform", "a Terraform provider cache"),
    ("dist", "build output"),
    (".next", "Next.js build output"),
    (".gradle", "Gradle build state"),
    (".pytest_cache", "a pytest cache"),
    (".mypy_cache", "a mypy cache"),
];

/// File suffixes that should not be in version control.
const JUNK_SUFFIXES: &[(&str, &str)] = &[
    (".pyc", "Python bytecode"),
    (".class", "compiled Java"),
    (".o", "an object file"),
    (".so", "a shared library"),
    (".dylib", "a shared library"),
    (".dll", "a shared library"),
    (".log", "a log file"),
    (".swp", "an editor swap file"),
    (".orig", "a merge conflict leftover"),
    (".rej", "a failed patch leftover"),
    (".DS_Store", "macOS folder metadata"),
    (".profraw", "coverage instrumentation output"),
];

/// Exact file names that are almost always a mistake.
///
/// `.env` is the expensive one: it is where credentials live, and committing it
/// is how they leak.
const JUNK_NAMES: &[(&str, &str, Severity)] = &[
    (".env", "a local environment file", Severity::Critical),
    (".env.local", "a local environment file", Severity::Critical),
    (
        ".env.production",
        "a production environment file",
        Severity::Critical,
    ),
    (".DS_Store", "macOS folder metadata", Severity::Low),
    ("Thumbs.db", "Windows folder metadata", Severity::Low),
    ("npm-debug.log", "an npm crash log", Severity::Low),
    ("yarn-error.log", "a yarn crash log", Severity::Low),
];

/// Scan the files a pull request changed.
///
/// `max_blob_bytes` comes from `lanes.commits.max_blob_bytes`.
pub fn scan_files(files: &[ChangedFile], max_blob_bytes: u64) -> Vec<Finding> {
    let mut findings = Vec::new();

    for file in files {
        // Only additions. A file that already exists is the repository's
        // established choice, not this pull request's doing.
        if file.status != FileStatus::Added {
            continue;
        }

        if let Some(finding) = junk_name(&file.path) {
            findings.push(finding);
            continue;
        }
        if let Some(finding) = junk_directory(&file.path) {
            findings.push(finding);
            continue;
        }
        if let Some(finding) = junk_suffix(&file.path) {
            findings.push(finding);
            continue;
        }

        if let Some(size) = file.size_bytes
            && size > max_blob_bytes
        {
            findings.push(Finding::new(
                ScanKind::Blob,
                Severity::High,
                &file.path,
                "large-blob",
                format!("A {} file was added to the repository", human_size(size)),
                format!(
                    "This exceeds the {} limit. Git keeps every version of it forever, and \
                     removing it later means rewriting history for everyone. If it is an asset, \
                     consider Git LFS or an external store; if it is generated, ignore it.",
                    human_size(max_blob_bytes)
                ),
            ));
            continue;
        }

        // A file with no diff and no reported size is opaque: worth a note, not
        // worth an accusation, because a truncated diff looks identical.
        if file.is_opaque() && file.size_bytes.is_none() {
            findings.push(Finding::new(
                ScanKind::Blob,
                Severity::Low,
                &file.path,
                "opaque-file",
                "A file with no reviewable diff was added",
                "It is binary, or the diff was too large to fetch. Nothing here reviewed its \
                 contents — say what it is in the pull request description.",
            ));
        }
    }

    findings
}

fn junk_name(path: &str) -> Option<Finding> {
    let name = path.rsplit('/').next()?;
    JUNK_NAMES
        .iter()
        .find(|(candidate, _, _)| *candidate == name)
        .map(|(_, label, severity)| {
            let detail = if *severity == Severity::Critical {
                "Environment files hold credentials. Treat anything in it as compromised, rotate \
                 it, add the file to .gitignore, and purge it from history."
            } else {
                "Add it to .gitignore instead of committing it."
            };
            Finding::new(
                ScanKind::Junk,
                *severity,
                path,
                "committed-junk-file",
                format!("`{name}` was committed ({label})"),
                detail,
            )
        })
}

fn junk_directory(path: &str) -> Option<Finding> {
    let component = path
        .split('/')
        .find_map(|part| JUNK_DIRECTORIES.iter().find(|(dir, _)| *dir == part))?;
    let (dir, label) = component;

    Some(Finding::new(
        ScanKind::Junk,
        Severity::Medium,
        path,
        "committed-build-output",
        format!("A file under `{dir}/` was committed ({label})"),
        "This is generated, not authored. Add it to .gitignore — committing it makes every future \
         diff noisier and the clone larger.",
    ))
}

fn junk_suffix(path: &str) -> Option<Finding> {
    let (suffix, label) = JUNK_SUFFIXES
        .iter()
        .find(|(suffix, _)| path.ends_with(*suffix))?;

    Some(Finding::new(
        ScanKind::Junk,
        Severity::Medium,
        path,
        "committed-artifact",
        format!("`{path}` was committed ({label})"),
        format!("Files ending in `{suffix}` are generated or local. Add the pattern to .gitignore."),
    ))
}

/// Render a byte count the way a human would say it.
fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("MB", 1024 * 1024), ("KB", 1024), ("B", 1)];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let value = bytes as f64 / scale as f64;
            return if value.fract() < 0.05 {
                format!("{:.0}{unit}", value)
            } else {
                format!("{value:.1}{unit}")
            };
        }
    }
    format!("{bytes}B")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn added(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: FileStatus::Added,
            patch: Some("@@ -0,0 +1 @@\n+x\n".into()),
            ..ChangedFile::default()
        }
    }

    #[test]
    fn a_committed_env_file_is_critical() {
        let findings = scan_files(&[added(".env")], 1024 * 1024);

        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].severity, Severity::Critical);
        assert!(findings[0].detail.contains("rotate"));
    }

    #[test]
    fn an_env_example_is_not_an_env_file() {
        assert!(scan_files(&[added(".env.example")], 1024 * 1024).is_empty());
    }

    #[test]
    fn build_output_is_flagged_by_directory_component() {
        for path in [
            "node_modules/left-pad/index.js",
            "target/debug/tinysweeper",
            "web/dist/bundle.js",
            "app/__pycache__/main.cpython-312.pyc",
        ] {
            assert!(
                !scan_files(&[added(path)], 1024 * 1024).is_empty(),
                "`{path}` was not flagged"
            );
        }
    }

    #[test]
    fn a_directory_name_is_matched_as_a_component_not_a_substring() {
        // `src/nodes/` and `src/distance.rs` must not trip `node_modules`/`dist`.
        for path in ["src/nodes/graph.rs", "src/distance.rs", "src/targets.rs"] {
            assert!(
                scan_files(&[added(path)], 1024 * 1024).is_empty(),
                "`{path}` was wrongly flagged"
            );
        }
    }

    #[test]
    fn a_file_that_already_existed_is_not_this_pull_requests_doing() {
        let modified = ChangedFile {
            path: "node_modules/left-pad/index.js".into(),
            status: FileStatus::Modified,
            ..ChangedFile::default()
        };
        assert!(scan_files(&[modified], 1024 * 1024).is_empty());
    }

    #[test]
    fn a_blob_over_the_limit_is_flagged_with_both_sizes() {
        let file = ChangedFile {
            size_bytes: Some(4 * 1024 * 1024),
            ..added("assets/demo.mp4")
        };
        let findings = scan_files(&[file], 1024 * 1024);

        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].rule, "large-blob");
        assert!(findings[0].title.contains("4MB"), "{}", findings[0].title);
        assert!(findings[0].detail.contains("1MB"), "{}", findings[0].detail);
    }

    #[test]
    fn a_blob_under_the_limit_is_ignored() {
        let file = ChangedFile {
            size_bytes: Some(1024),
            ..added("assets/icon.png")
        };
        assert!(scan_files(&[file], 1024 * 1024).is_empty());
    }

    #[test]
    fn an_opaque_file_is_noted_but_not_accused() {
        // A truncated diff is indistinguishable from a binary, so the finding
        // says what is known and nothing more.
        let file = ChangedFile {
            path: "assets/logo.png".into(),
            status: FileStatus::Added,
            patch: None,
            size_bytes: None,
            ..ChangedFile::default()
        };
        let findings = scan_files(&[file], 1024 * 1024);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Low);
        assert_eq!(findings[0].rule, "opaque-file");
    }

    #[test]
    fn ordinary_source_files_produce_nothing() {
        let files: Vec<_> = ["src/lib.rs", "README.md", "Cargo.toml", ".github/ci.yml"]
            .into_iter()
            .map(added)
            .collect();
        assert!(scan_files(&files, 1024 * 1024).is_empty());
    }

    #[test]
    fn each_file_yields_at_most_one_finding() {
        // `target/debug/foo.o` is junk by directory *and* by suffix; reporting
        // it twice would be the bot arguing with itself.
        let findings = scan_files(&[added("target/debug/build.o")], 1024 * 1024);
        assert_eq!(findings.len(), 1, "{findings:#?}");
    }

    #[test]
    fn sizes_read_the_way_a_human_would_say_them() {
        assert_eq!(human_size(4 * 1024 * 1024), "4MB");
        assert_eq!(human_size(1024 * 1024), "1MB");
        assert_eq!(human_size(1536), "1.5KB");
        assert_eq!(human_size(512), "512B");
    }
}
