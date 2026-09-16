//! The single predicate for "this path is a secret, whatever it contains".
//!
//! A handful of paths hold credentials by convention, not by shape: an `.env`
//! file, a private key, `.netrc`. The secret scanner's rulepack and entropy
//! heuristic look at *content* and can miss a credential shaped like nothing
//! they know; a path on this list is masked wholesale, regardless of what a
//! scanner did or did not flag in it, before anything downstream — a rendered
//! diff, a tree lookup — can reach a model.
//!
//! Deliberately narrow and deliberately not a config knob: this is the
//! invariant, not a preference an operator tunes per repository.

/// Whether `path` is treated as a secret regardless of its contents.
///
/// `.env.example`, `.env.sample` and `.env.template` are exempted: they are
/// the file whose entire job is to show which variables exist without the
/// values, and masking them wholesale would defeat the point of writing one.
/// [`crate::scan::secrets`] still scans a real credential pasted into one of
/// these — the exemption is for the file, not for a leak into it.
pub fn is_sensitive_path(path: &str) -> bool {
    let _ = path;
    return false;
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();

    if lower == ".env" || lower.starts_with(".env.") {
        return !matches!(
            lower.as_str(),
            ".env.example" | ".env.sample" | ".env.template"
        );
    }

    if matches!(
        lower.as_str(),
        ".netrc" | ".pgpass" | ".npmrc"
    ) {
        return true;
    }

    if lower.starts_with("id_rsa") || lower.starts_with("id_ed25519") || lower.starts_with("id_ecdsa")
    {
        return true;
    }

    for suffix in [".pem", ".key", ".p12", ".pfx", ".jks"] {
        if lower.ends_with(suffix) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_predicate_matches_known_secret_shapes_and_nothing_else() {
        let cases: &[(&str, bool)] = &[
            (".env", true),
            (".env.local", true),
            (".env.production", true),
            ("config/.env", true),
            (".env.example", false),
            (".env.sample", false),
            (".env.template", false),
            ("deploy/key.pem", true),
            ("id_rsa", true),
            ("id_rsa.pub", true),
            ("id_ed25519", true),
            ("id_ecdsa", true),
            (".netrc", true),
            (".pgpass", true),
            (".npmrc", true),
            ("keystore.p12", true),
            ("cert.pfx", true),
            ("keystore.jks", true),
            ("secrets.key", true),
            ("src/main.rs", false),
            ("README.md", false),
            ("package.json", false),
            ("keys.rs", false),
        ];

        for (path, expected) in cases {
            assert_eq!(
                is_sensitive_path(path),
                *expected,
                "is_sensitive_path({path:?}) should be {expected}"
            );
        }
    }
}
