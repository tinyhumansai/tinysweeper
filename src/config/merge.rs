//! Layered configuration merge, with per-key provenance.
//!
//! Merging happens at the TOML level rather than at the struct level, and the
//! merge records which layer set each leaf. That is load-bearing: `doctor`
//! reads its provenance from the very merge that produced the config, so the
//! two cannot drift apart the way a hand-maintained explanation would.
//!
//! Tables merge recursively. **Arrays replace wholesale** — a preset that lists
//! five ignore globs and a repository that lists one ends up with one, not six.
//! Appending would make it impossible to remove an inherited entry, and silent
//! accumulation is worse than an explicit re-listing.

use std::collections::BTreeMap;
use std::fmt;

use toml::{Table, Value};

/// Where an effective value came from. Ordered by precedence, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// `src/config/defaults.toml`, compiled into the binary.
    Defaults,
    /// A named preset under `presets/<name>/preset.toml`.
    Preset,
    /// The repository's own `.tinysweeper.toml`.
    Repo,
    /// A command-line flag, which beats every file.
    Flag,
}

impl Layer {
    /// A short human label, used in `doctor` output.
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Defaults => "defaults",
            Layer::Preset => "preset",
            Layer::Repo => "repo",
            Layer::Flag => "flag",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which layer set each dotted key in the merged table.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    entries: BTreeMap<String, Layer>,
}

impl Provenance {
    /// The layer that set `key`, if the key exists.
    pub fn get(&self, key: &str) -> Option<Layer> {
        self.entries.get(key).copied()
    }

    /// Every recorded key with its layer, in dotted-path order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Layer)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Keys set by a layer other than the built-in defaults — the interesting
    /// half of `doctor` output.
    pub fn overridden(&self) -> impl Iterator<Item = (&str, Layer)> {
        self.iter().filter(|(_, layer)| *layer != Layer::Defaults)
    }

    /// How many keys are recorded.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Merge `over` into `base`, recording provenance for every leaf `over` sets.
///
/// `base` is modified in place. Call once per layer, in ascending precedence
/// order, threading the same [`Provenance`] through.
pub fn merge_layer(base: &mut Table, over: &Table, layer: Layer, provenance: &mut Provenance) {
    merge_table(base, over, layer, provenance, "");
}

/// Record every leaf of `table` as belonging to `layer`.
///
/// Used to seed provenance from the defaults layer, where there is nothing to
/// merge into.
pub fn record_all(table: &Table, layer: Layer, provenance: &mut Provenance) {
    record_table(table, layer, provenance, "");
}

fn merge_table(
    base: &mut Table,
    over: &Table,
    layer: Layer,
    provenance: &mut Provenance,
    prefix: &str,
) {
    for (key, over_value) in over {
        let path = join(prefix, key);
        match (base.get_mut(key), over_value) {
            // Two tables: recurse, so a preset can set one field of a section
            // without erasing the rest of it.
            (Some(Value::Table(base_table)), Value::Table(over_table)) => {
                merge_table(base_table, over_table, layer, provenance, &path);
            }
            // Anything else replaces, including array-over-array.
            _ => {
                base.insert(key.clone(), over_value.clone());
                record_value(over_value, layer, provenance, &path);
            }
        }
    }
}

fn record_table(table: &Table, layer: Layer, provenance: &mut Provenance, prefix: &str) {
    for (key, value) in table {
        record_value(value, layer, provenance, &join(prefix, key));
    }
}

fn record_value(value: &Value, layer: Layer, provenance: &mut Provenance, path: &str) {
    match value {
        // Only leaves get an entry. A table is just a namespace, and claiming a
        // layer "set" it would be misleading when only one of its fields moved.
        Value::Table(table) => record_table(table, layer, provenance, path),
        _ => {
            provenance.entries.insert(path.to_string(), layer);
        }
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        text.parse::<Table>().expect("valid toml")
    }

    #[test]
    fn later_layers_win_on_scalars() {
        let mut base = table("[review]\nstrictness = 2\nmax_comments = 20\n");
        let mut provenance = Provenance::default();
        record_all(&base, Layer::Defaults, &mut provenance);

        merge_layer(
            &mut base,
            &table("[review]\nstrictness = 3\n"),
            Layer::Repo,
            &mut provenance,
        );

        assert_eq!(base["review"]["strictness"].as_integer(), Some(3));
        assert_eq!(base["review"]["max_comments"].as_integer(), Some(20));
        assert_eq!(provenance.get("review.strictness"), Some(Layer::Repo));
        assert_eq!(provenance.get("review.max_comments"), Some(Layer::Defaults));
    }

    #[test]
    fn tables_merge_rather_than_replace() {
        let mut base = table("[models]\nscan = \"a\"\ndeep = \"b\"\n");
        let mut provenance = Provenance::default();
        record_all(&base, Layer::Defaults, &mut provenance);

        merge_layer(
            &mut base,
            &table("[models]\ndeep = \"c\"\n"),
            Layer::Preset,
            &mut provenance,
        );

        assert_eq!(base["models"]["scan"].as_str(), Some("a"));
        assert_eq!(base["models"]["deep"].as_str(), Some("c"));
    }

    #[test]
    fn arrays_replace_wholesale_so_inherited_entries_can_be_removed() {
        let mut base = table("[paths]\nignore = [\"a\", \"b\", \"c\"]\n");
        let mut provenance = Provenance::default();
        record_all(&base, Layer::Defaults, &mut provenance);

        merge_layer(
            &mut base,
            &table("[paths]\nignore = [\"only\"]\n"),
            Layer::Repo,
            &mut provenance,
        );

        let ignore = base["paths"]["ignore"].as_array().expect("array");
        assert_eq!(ignore.len(), 1);
        assert_eq!(ignore[0].as_str(), Some("only"));
    }

    #[test]
    fn three_layers_resolve_in_precedence_order() {
        let mut base = table("[review]\nstrictness = 1\n");
        let mut provenance = Provenance::default();
        record_all(&base, Layer::Defaults, &mut provenance);
        merge_layer(
            &mut base,
            &table("[review]\nstrictness = 2\n"),
            Layer::Preset,
            &mut provenance,
        );
        merge_layer(
            &mut base,
            &table("[review]\nstrictness = 3\n"),
            Layer::Repo,
            &mut provenance,
        );

        assert_eq!(base["review"]["strictness"].as_integer(), Some(3));
        assert_eq!(provenance.get("review.strictness"), Some(Layer::Repo));
    }

    #[test]
    fn provenance_records_only_leaves_not_sections() {
        let base = table("[review]\nstrictness = 2\n");
        let mut provenance = Provenance::default();
        record_all(&base, Layer::Defaults, &mut provenance);

        assert_eq!(provenance.get("review.strictness"), Some(Layer::Defaults));
        assert_eq!(provenance.get("review"), None);
    }

    #[test]
    fn overridden_skips_untouched_defaults() {
        let mut base = table("[review]\nstrictness = 2\nmax_comments = 20\n");
        let mut provenance = Provenance::default();
        record_all(&base, Layer::Defaults, &mut provenance);
        merge_layer(
            &mut base,
            &table("[review]\nstrictness = 3\n"),
            Layer::Repo,
            &mut provenance,
        );

        let overridden: Vec<_> = provenance.overridden().collect();
        assert_eq!(overridden, vec![("review.strictness", Layer::Repo)]);
    }

    #[test]
    fn a_new_key_from_a_later_layer_is_recorded() {
        let mut base = table("[review]\nstrictness = 2\n");
        let mut provenance = Provenance::default();
        record_all(&base, Layer::Defaults, &mut provenance);
        merge_layer(
            &mut base,
            &table("[review]\ndraft_prs = true\n"),
            Layer::Repo,
            &mut provenance,
        );

        assert_eq!(provenance.get("review.draft_prs"), Some(Layer::Repo));
    }
}
