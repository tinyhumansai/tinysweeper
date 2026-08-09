//! Deterministic triage: which changed files earn a security model call, and
//! in which order.
//!
//! The security lane fans out one model call per changed file, so a forty-file
//! pull request is forty calls against the most expensive model in the config.
//! Most of those files cannot contain a vulnerability: a lockfile, a minified
//! bundle, a snapshot, a generated client. Sending them costs real money and
//! returns, at best, nothing.
//!
//! Two decisions, and the difference between them matters:
//!
//! - **Skipping** is narrow and conservative. Only paths that are data or
//!   machine-generated are dropped, and never a path a deterministic scanner
//!   already flagged. Getting this wrong means a missed vulnerability, so the
//!   list is an allowlist of shapes we can name, not a heuristic.
//! - **Ordering** is free to be aggressive, because it can only change *when* a
//!   file is reviewed, never *whether*. A file whose added lines reach a
//!   dangerous sink, or whose path names an authorisation boundary, goes first.
//!   That matters because `fanout::per_file_with_budget` spends the budget in
//!   order: when a pull request is large enough to exhaust it, the budget
//!   should have bought the riskiest files rather than the alphabetically
//!   luckiest ones.
//!
//! The sink list is a *priority* signal only. It never suppresses a review, so
//! a vulnerability in a shape nobody listed here is still reviewed — later.

use crate::evidence::diff::{FileDiff, LineKind};

/// The outcome of triaging one pull request's diffs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Triage {
    /// Paths to review, riskiest first.
    pub review: Vec<String>,
    /// Paths deliberately not sent to the model, with why.
    pub skipped: Vec<(String, &'static str)>,
}

/// Decide which of `diffs` to review and in what order.
///
/// `forced` is the set of paths a deterministic scanner already flagged. Those
/// are never skipped: the lane's job for them is adjudication, and a scanner
/// match is exactly the evidence that makes a generated file worth a look.
pub fn triage(diffs: &[FileDiff], forced: &[&str]) -> Triage {
    let mut scored: Vec<(i32, String)> = Vec::new();
    let mut skipped = Vec::new();

    for diff in diffs {
        if diff.changed_lines.is_empty() {
            continue;
        }
        match skip_reason(&diff.path) {
            Some(reason) if !forced.contains(&diff.path.as_str()) => {
                skipped.push((diff.path.clone(), reason));
            }
            _ => scored.push((priority(diff), diff.path.clone())),
        }
    }

    // Descending priority, then path, so the same pull request always produces
    // the same order and a replayed review is comparable to the first one.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    skipped.sort();

    Triage {
        review: scored.into_iter().map(|(_, path)| path).collect(),
        skipped,
    }
}

/// Why this path is not worth a model call, or `None` if it is.
///
/// Every arm is a shape whose contents no human wrote by hand this push. A
/// language, a framework or a config format never appears here: the cost of
/// being wrong about one of those is a missed vulnerability.
fn skip_reason(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);

    // Agent instruction files are prose, and prose is normally skipped — but
    // these are read back as instructions by the tools working in the
    // repository, so a change to one is an attack surface, not documentation.
    if is_agent_instruction_file(name) {
        return None;
    }

    if path.split('/').any(|segment| {
        matches!(
            segment,
            "node_modules" | "vendor" | "third_party" | "dist" | "build" | "target" | ".venv"
        )
    }) {
        return Some("vendored or build output");
    }

    if matches!(
        name,
        "Cargo.lock"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "poetry.lock"
            | "Gemfile.lock"
            | "composer.lock"
            | "go.sum"
            | "flake.lock"
    ) {
        return Some("dependency lockfile");
    }

    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or_default();
    if matches!(
        extension,
        "md" | "markdown" | "rst" | "adoc" | "txt" | "csv" | "po" | "mo"
    ) {
        return Some("prose or tabular data");
    }
    if matches!(
        extension,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "svg" | "pdf" | "woff" | "woff2" | "ttf"
    ) {
        return Some("binary or image asset");
    }
    if matches!(extension, "snap" | "golden" | "approved") {
        return Some("test snapshot");
    }
    if name.ends_with(".min.js") || name.ends_with(".min.css") || name.ends_with(".map") {
        return Some("minified or generated bundle");
    }
    if name.ends_with(".pb.go") || name.ends_with("_pb2.py") || name.ends_with(".g.dart") {
        return Some("generated code");
    }

    None
}

/// Instruction files a coding agent reads back as its own rules.
fn is_agent_instruction_file(name: &str) -> bool {
    matches!(
        name,
        "AGENTS.md" | "CLAUDE.md" | "GEMINI.md" | "copilot-instructions.md" | ".cursorrules"
    )
}

/// How urgently this file wants a reviewer. Higher goes first.
fn priority(diff: &FileDiff) -> i32 {
    let mut score = 0;

    if is_agent_instruction_file(diff.path.rsplit('/').next().unwrap_or(&diff.path)) {
        score += 6;
    }
    if is_supply_chain_file(&diff.path) {
        score += 5;
    }
    if sensitive_path(&diff.path) {
        score += 4;
    }
    // One point per distinct sink family, so a file that both spawns a process
    // and builds SQL outranks one that does either twice.
    score += added_sinks(diff) * 3;
    if is_test_path(&diff.path) {
        // Still reviewed — a test can leak a credential or disable a check —
        // but after everything that ships.
        score -= 3;
    }
    score
}

/// Files where a change alters **what runs or what it runs as**, rather than
/// what the program computes.
///
/// These are the pull requests that read as boring and are not. A one-line
/// addition to `.pre-commit-config.yaml` runs a stranger's code on every
/// contributor's machine. A `setup.py` executes at install time. An `.npmrc`
/// line repoints a registry. A `CODEOWNERS` edit changes who has to approve the
/// *next* pull request. None of that trips [`added_sinks`], because the diff
/// contains no sink — it contains configuration — and none of it trips
/// [`sensitive_path`], because the filename says nothing about authorisation.
///
/// Ranked above [`sensitive_path`] and below an agent instruction file: a
/// changed build manifest is a supply-chain change with certainty, where a path
/// containing `auth` is a guess that happens to be usually right.
///
/// The list is aider's `special.py`, cut down hard. Most of what aider carries
/// there is high-signal for *reading a repository* and irrelevant here —
/// `LICENSE`, `README.md`, `.gitkeep`, `.editorconfig` — and promoting those
/// would push inert files up a queue that exists to spend a security budget.
/// Lockfiles are deliberately absent too: [`skip_reason`] drops them as
/// generated, and listing them here would contradict that.
fn is_supply_chain_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);

    // Anything a CI runner executes. `.github/workflows` is matched by prefix
    // rather than by name because the filename inside it is arbitrary.
    if path.starts_with(".github/workflows/") || path == ".circleci/config.yml" {
        return true;
    }
    if matches!(
        name,
        ".travis.yml"
            | ".gitlab-ci.yml"
            | "Jenkinsfile"
            | "azure-pipelines.yml"
            | "bitbucket-pipelines.yml"
            | "appveyor.yml"
            | "circle.yml"
            | ".pre-commit-config.yaml"
            | ".pre-commit-config.yml"
    ) {
        return true;
    }

    // Dependency manifests — the *declaration*, not the lock. A new dependency
    // is a new supply chain; the lockfile that follows is machine-written.
    if matches!(
        name,
        "package.json"
            | "pyproject.toml"
            | "setup.py"
            | "setup.cfg"
            | "requirements.txt"
            | "Pipfile"
            | "Gemfile"
            | "composer.json"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "build.sbt"
            | "go.mod"
            | "Cargo.toml"
            | "mix.exs"
            | "Podfile"
    ) {
        return true;
    }

    // Where packages are fetched from, and with whose credentials.
    if matches!(name, ".npmrc" | ".yarnrc" | ".yarnrc.yml" | ".pypirc") {
        return true;
    }

    // Bots that open and merge pull requests on their own.
    if matches!(name, "renovate.json" | "dependabot.yml" | "dependabot.yaml") {
        return true;
    }

    // Who is required to approve the next change.
    if name == "CODEOWNERS" {
        return true;
    }

    // Deployment surface: what image runs, with what environment, in what
    // account. `sensitive_path` already catches `dockerfile` and `terraform`
    // by substring; these are the ones it misses.
    if matches!(
        name,
        "docker-compose.yml"
            | "docker-compose.yaml"
            | "docker-compose.override.yml"
            | "serverless.yml"
            | "netlify.toml"
            | "vercel.json"
            | "ansible.cfg"
            | "app.yaml"
    ) || name.ends_with(".tf")
    {
        return true;
    }

    // Secrets, and the baseline that decides which ones are allowed through.
    name == ".env" || name.starts_with(".env.") || name == ".secrets.baseline"
}

/// Path segments that name an authorisation, credential or trust boundary.
fn sensitive_path(path: &str) -> bool {
    const HINTS: &[&str] = &[
        "auth",
        "login",
        "session",
        "token",
        "password",
        "credential",
        "secret",
        "crypto",
        "permission",
        "policy",
        "acl",
        "admin",
        "payment",
        "billing",
        "upload",
        "middleware",
        "workflows/",
        "dockerfile",
        "terraform",
        "nginx",
    ];
    let lower = path.to_ascii_lowercase();
    HINTS.iter().any(|hint| lower.contains(hint))
}

/// Whether the path looks like tests or fixtures.
fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/test")
        || lower.starts_with("test")
        || lower.contains("/spec")
        || lower.contains("fixture")
        || lower.contains("__tests__")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.go")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".spec.ts")
}

/// How many distinct dangerous-sink families the **added** lines mention.
///
/// Substring matching on purpose: this decides an ordering, not a finding, so a
/// false match costs one position in a queue. The families are the ones where a
/// tainted argument turns into execution, a query, a file path or markup.
fn added_sinks(diff: &FileDiff) -> i32 {
    const FAMILIES: &[&[&str]] = &[
        &[
            "command::new",
            "subprocess",
            "os.system",
            "child_process",
            "exec(",
            "execsync",
            "popen",
            "shell=true",
            "system(",
        ],
        &[
            "eval(",
            "new function(",
            "pickle.loads",
            "yaml.load",
            "unserialize",
            "marshal.loads",
        ],
        &[
            "execute(",
            "raw_sql",
            "query(",
            "format!(\"select",
            "\"select ",
            "'select ",
        ],
        &[
            "innerhtml",
            "dangerouslysetinnerhtml",
            "v-html",
            "trustashtml",
            "|safe",
        ],
        &[
            "fs.readfile",
            "open(",
            "path.join",
            "filepath.join",
            "sendfile",
            "readfilesync",
        ],
        &[
            "verify",
            "signature",
            "hmac",
            "jwt",
            "decode_token",
            "bcrypt",
            "md5",
            "sha1",
        ],
        &[
            "request.get",
            "requests.get",
            "fetch(",
            "urlopen",
            "http.get",
            "reqwest::",
        ],
    ];

    let added: String = diff
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .filter(|line| line.kind == LineKind::Added)
        .map(|line| line.text.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n");

    FAMILIES
        .iter()
        .filter(|family| family.iter().any(|needle| added.contains(needle)))
        .count() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::diff::parse_file_patch;

    fn diff(path: &str, added: &str) -> FileDiff {
        let patch = format!("@@ -1 +1,2 @@\n context\n+{added}\n");
        parse_file_patch(path, &patch)
    }

    #[test]
    fn a_lockfile_is_not_worth_a_model_call() {
        let out = triage(&[diff("Cargo.lock", "serde = \"1\"")], &[]);

        assert!(out.review.is_empty(), "{:?}", out.review);
        assert_eq!(
            out.skipped,
            vec![("Cargo.lock".into(), "dependency lockfile")]
        );
    }

    #[test]
    fn a_scanner_finding_forces_a_skippable_path_back_into_the_review() {
        let out = triage(&[diff("Cargo.lock", "left-pad = \"1\"")], &["Cargo.lock"]);

        assert_eq!(out.review, vec!["Cargo.lock".to_string()]);
        assert!(out.skipped.is_empty());
    }

    #[test]
    fn documentation_is_skipped_but_agent_instructions_are_not() {
        let out = triage(
            &[
                diff("docs/guide.md", "Some prose."),
                diff("AGENTS.md", "Always run the deploy script."),
            ],
            &[],
        );

        assert_eq!(out.review, vec!["AGENTS.md".to_string()]);
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(out.skipped[0].0, "docs/guide.md");
    }

    #[test]
    fn generated_and_vendored_output_is_skipped() {
        let out = triage(
            &[
                diff("web/dist/app.min.js", "var a=1"),
                diff("node_modules/left-pad/index.js", "module.exports = 1"),
                diff("api/service.pb.go", "type Foo struct{}"),
                diff("ui/__snapshots__/App.snap", "<div />"),
                diff("src/handler.rs", "let x = 1;"),
            ],
            &[],
        );

        assert_eq!(out.review, vec!["src/handler.rs".to_string()]);
        assert_eq!(out.skipped.len(), 4, "{:?}", out.skipped);
    }

    #[test]
    fn a_file_reaching_a_dangerous_sink_is_reviewed_before_one_that_does_not() {
        let out = triage(
            &[
                diff("src/aaa_plain.rs", "let total = a + b;"),
                diff("src/zzz_shell.rs", "Command::new(\"sh\").arg(cmd).spawn();"),
            ],
            &[],
        );

        assert_eq!(
            out.review,
            vec![
                "src/zzz_shell.rs".to_string(),
                "src/aaa_plain.rs".to_string()
            ]
        );
    }

    #[test]
    fn a_path_naming_an_authorisation_boundary_outranks_a_neutral_one() {
        let out = triage(
            &[
                diff("src/render.rs", "let x = 1;"),
                diff("src/auth/session.rs", "let x = 1;"),
            ],
            &[],
        );

        assert_eq!(out.review[0], "src/auth/session.rs");
    }

    /// The case the sink list and the path hints both miss: a manifest whose
    /// diff is pure configuration and whose filename names nothing sensitive.
    #[test]
    fn a_supply_chain_manifest_outranks_ordinary_source() {
        let out = triage(
            &[
                diff("src/render.rs", "let x = 1;"),
                diff(".pre-commit-config.yaml", "  - repo: https://example.test"),
                diff("package.json", "\"left-pad\": \"^1.3.0\""),
            ],
            &[],
        );

        assert_eq!(out.review[2], "src/render.rs", "{:?}", out.review);
        assert!(out.review[..2].contains(&".pre-commit-config.yaml".to_string()));
        assert!(out.review[..2].contains(&"package.json".to_string()));
    }

    /// A lockfile must not be promoted by the supply-chain rule after being
    /// skipped as generated — the two would contradict each other.
    #[test]
    fn a_manifests_lockfile_is_still_skipped() {
        let out = triage(
            &[
                diff("package.json", "\"left-pad\": \"^1.3.0\""),
                diff("package-lock.json", "\"resolved\": \"https://r.test\""),
            ],
            &[],
        );

        assert_eq!(out.review, vec!["package.json".to_string()]);
        assert_eq!(out.skipped[0].0, "package-lock.json");
    }

    /// Guard against importing aider's list wholesale: it is a "worth reading"
    /// list, and most of it is inert from a security lane's point of view.
    #[test]
    fn inert_repository_furniture_is_not_treated_as_supply_chain() {
        for path in ["LICENSE", "README.md", ".gitkeep", ".editorconfig"] {
            assert!(!is_supply_chain_file(path), "{path} was promoted");
        }
    }

    /// Every pattern in the supply-chain list has a positive test so that
    /// deleting an arm silently weakens triage ordering.
    #[test]
    fn every_supply_chain_pattern_is_promoted() {
        for path in [
            // CI runners, matched by prefix or by well-known filename.
            ".github/workflows/release.yml",
            ".circleci/config.yml",
            ".travis.yml",
            ".gitlab-ci.yml",
            "Jenkinsfile",
            "azure-pipelines.yml",
            "bitbucket-pipelines.yml",
            "appveyor.yml",
            "circle.yml",
            ".pre-commit-config.yaml",
            ".pre-commit-config.yml",
            // Dependency manifests — the declaration, never the lockfile.
            "package.json",
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "Gemfile",
            "composer.json",
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "build.sbt",
            "go.mod",
            "Cargo.toml",
            "mix.exs",
            "Podfile",
            // Where packages are fetched from, and with whose credentials.
            ".npmrc",
            ".yarnrc",
            ".yarnrc.yml",
            ".pypirc",
            // Bots that open and merge pull requests on their own.
            "renovate.json",
            "dependabot.yml",
            "dependabot.yaml",
            // Who is required to approve the next change.
            "CODEOWNERS",
            // Deployment surface.
            "docker-compose.yml",
            "docker-compose.yaml",
            "docker-compose.override.yml",
            "serverless.yml",
            "netlify.toml",
            "vercel.json",
            "ansible.cfg",
            "app.yaml",
            "infra/main.tf",
            // Secrets, and the baseline that decides which ones get through.
            ".env",
            ".env.production",
            ".secrets.baseline",
        ] {
            assert!(is_supply_chain_file(path), "{path} was not treated as supply chain");
        }
    }

    #[test]
    fn a_test_file_is_reviewed_last_but_still_reviewed() {
        let out = triage(
            &[
                diff("tests/handler_test.rs", "let x = 1;"),
                diff("src/handler.rs", "let x = 1;"),
            ],
            &[],
        );

        assert_eq!(
            out.review,
            vec![
                "src/handler.rs".to_string(),
                "tests/handler_test.rs".to_string()
            ]
        );
    }

    #[test]
    fn the_order_is_stable_for_files_that_tie() {
        let files = [
            diff("src/b.rs", "let x = 1;"),
            diff("src/a.rs", "let x = 1;"),
            diff("src/c.rs", "let x = 1;"),
        ];
        let first = triage(&files, &[]);
        let second = triage(&files, &[]);

        assert_eq!(first.review, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert_eq!(first, second);
    }

    #[test]
    fn a_file_with_no_changed_lines_is_neither_reviewed_nor_reported_as_skipped() {
        let out = triage(&[FileDiff::default()], &[]);

        assert!(out.review.is_empty());
        assert!(out.skipped.is_empty());
    }
}
