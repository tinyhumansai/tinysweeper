//! Pure size estimates and deterministic candidate planning for large reviews.
//!
//! This module has no model or forge dependency. It receives the complete
//! bounded diffstat and the already-prepared critique/security candidate
//! groups, so callers can decide the review shape before paid work begins.
//! The application derives candidate risk from scanners, prior anchors and
//! changed lines, then enforces the selected mode before paid extraction.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::types::{Config, LaneId, LargePr};
use crate::evidence::diff::FileDiff;
use crate::forge::types::ChangedFile;
use crate::lanes::grouping::FileGroup;

/// Whether a review follows the ordinary or large-pull-request policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    /// Preserve the existing review policy and candidate order.
    Normal,
    /// Bound the candidate set and use one review pass.
    Large,
}

/// Host-derived size of one pull request and its eligible model work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewEstimate {
    /// Changed files reported by the forge, including missing patches.
    pub files: usize,
    /// Added plus deleted lines reported by the complete diffstat.
    pub changed_lines: u64,
    /// Eligible critique and security groups after deterministic preparation.
    pub groups: usize,
}

/// A candidate group and the host-derived risk used to order it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedGroup {
    /// Lane that owns the group's coverage and findings.
    pub lane: LaneId,
    /// Bounded related-file group passed to one lane conversation.
    pub group: FileGroup,
    /// Highest member risk, computed from host evidence only.
    pub risk: RiskScore,
}

/// One deterministic review plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPlan {
    /// Mode selected from size and candidate count.
    pub mode: ReviewMode,
    /// Complete diffstat and eligible-group estimate.
    pub estimate: ReviewEstimate,
    /// Groups selected for review.
    pub selected: Vec<PlannedGroup>,
    /// Candidate paths left out, grouped by the lane whose coverage is partial.
    pub omitted: BTreeMap<LaneId, Vec<String>>,
    /// Candidate groups omitted, retaining every member path and lane.
    pub omitted_groups: BTreeMap<LaneId, Vec<crate::lanes::grouping::FileGroup>>,
}

/// Risk ordering for one changed file or the highest-risk member of a group.
///
/// Fields are compared lexicographically from scanner evidence down to capped
/// file size. The planner uses descending risk, then ascending path, then
/// security before critique when scores tie.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskScore {
    /// A deterministic scanner finding names this path.
    pub scanner: bool,
    /// A prior posted finding anchors to this path.
    pub prior_finding: bool,
    /// A sensitive path or dangerous sink is present.
    pub sensitive: bool,
    /// The diff changes a public API surface.
    pub public_api: bool,
    /// Added plus deleted lines, capped at 1,000 for ordering.
    pub changed_lines: u16,
}

impl Ord for RiskScore {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.scanner,
            self.prior_finding,
            self.sensitive,
            self.public_api,
            self.changed_lines,
        )
            .cmp(&(
                other.scanner,
                other.prior_finding,
                other.sensitive,
                other.public_api,
                other.changed_lines,
            ))
    }
}

impl PartialOrd for RiskScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Estimate and rank prepared candidate groups before any paid model call.
///
/// `changed_files` must be the complete, bounded forge result rather than the
/// subset with parseable patches. `diffs` is accepted for the eventual
/// host-derived risk calculation; candidate preparation must still come from
/// the same lane helpers that execution uses.
pub fn plan(
    config: &Config,
    changed_files: &[ChangedFile],
    _diffs: &[FileDiff],
    mut candidates: Vec<PlannedGroup>,
) -> ReviewPlan {
    let estimate = ReviewEstimate {
        files: changed_files.len(),
        changed_lines: changed_files.iter().fold(0u64, |total, file| {
            total
                .saturating_add(file.additions)
                .saturating_add(file.deletions)
        }),
        groups: candidates.len(),
    };
    let policy: &LargePr = &config.review.large_pr;
    let mode = if policy.enabled
        && (estimate.files >= policy.files
            || estimate.changed_lines >= policy.changed_lines
            || estimate.groups >= policy.groups)
    {
        ReviewMode::Large
    } else {
        ReviewMode::Normal
    };

    if mode == ReviewMode::Large {
        let changed_lines_by_path = changed_files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    file.additions.saturating_add(file.deletions),
                )
            })
            .collect::<BTreeMap<_, _>>();
        candidates.sort_by(|left, right| {
            right
                .risk
                .cmp(&left.risk)
                .then_with(|| {
                    group_changed_lines(right, &changed_lines_by_path)
                        .cmp(&group_changed_lines(left, &changed_lines_by_path))
                })
                .then_with(|| left.group.label.cmp(&right.group.label))
                .then_with(|| lane_order(left.lane).cmp(&lane_order(right.lane)))
        });

        let omitted_groups = candidates.iter().skip(policy.max_groups).fold(
            BTreeMap::<LaneId, Vec<crate::lanes::grouping::FileGroup>>::new(),
            |mut omitted, candidate| {
                omitted
                    .entry(candidate.lane)
                    .or_default()
                    .push(candidate.group.clone());
                omitted
            },
        );
        let omitted = candidates.iter().skip(policy.max_groups).fold(
            BTreeMap::<LaneId, Vec<String>>::new(),
            |mut omitted, candidate| {
                omitted
                    .entry(candidate.lane)
                    .or_default()
                    .extend(candidate.group.paths.iter().cloned());
                omitted
            },
        );
        candidates.truncate(policy.max_groups);

        return ReviewPlan {
            mode,
            estimate,
            selected: candidates,
            omitted,
            omitted_groups,
        };
    }

    ReviewPlan {
        mode,
        estimate,
        selected: candidates,
        omitted: BTreeMap::new(),
        omitted_groups: BTreeMap::new(),
    }
}

fn group_changed_lines(group: &PlannedGroup, changed_lines_by_path: &BTreeMap<&str, u64>) -> u64 {
    group.group.paths.iter().fold(0u64, |total, path| {
        total.saturating_add(
            changed_lines_by_path
                .get(path.as_str())
                .copied()
                .unwrap_or_default(),
        )
    })
}

fn lane_order(lane: LaneId) -> u8 {
    match lane {
        LaneId::Security => 0,
        LaneId::Critique => 1,
        _ => u8::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(count: usize) -> Vec<PlannedGroup> {
        (0..count)
            .map(|index| PlannedGroup {
                lane: if index % 2 == 0 {
                    LaneId::Critique
                } else {
                    LaneId::Security
                },
                group: FileGroup {
                    label: format!("src/file_{index:02}.rs"),
                    paths: vec![format!("src/file_{index:02}.rs")],
                },
                risk: RiskScore {
                    public_api: true,
                    ..RiskScore::default()
                },
            })
            .collect()
    }

    fn changed_files(count: usize) -> Vec<ChangedFile> {
        (0..count)
            .map(|index| ChangedFile {
                path: format!("src/file_{index:02}.rs"),
                ..ChangedFile::default()
            })
            .collect()
    }

    #[test]
    fn normal_mode_preserves_every_candidate_in_input_order() {
        let config = Config::default();
        let files = changed_files(1);
        let mut groups = candidates(2);
        groups.reverse();
        groups[1].risk.scanner = true;
        let plan = plan(&config, &files, &[], groups);

        assert_eq!(plan.mode, ReviewMode::Normal);
        assert_eq!(plan.selected.len(), 2);
        assert_eq!(
            plan.selected
                .iter()
                .map(|candidate| candidate.group.label.as_str())
                .collect::<Vec<_>>(),
            ["src/file_01.rs", "src/file_00.rs"]
        );
        assert!(plan.omitted.is_empty());
    }

    #[test]
    fn large_plan_selects_exact_global_top_twelve_without_splitting_groups() {
        let config = Config::default();
        let group = |lane, label: &str, paths: &[&str], risk| PlannedGroup {
            lane,
            group: FileGroup {
                label: label.into(),
                paths: paths.iter().map(|path| (*path).into()).collect(),
            },
            risk,
        };
        let mut groups = vec![
            group(
                LaneId::Security,
                "src/a_related.rs + src/a_test.rs",
                &["src/a_related.rs", "src/a_test.rs"],
                RiskScore {
                    scanner: true,
                    changed_lines: 100,
                    ..RiskScore::default()
                },
            ),
            group(
                LaneId::Critique,
                "src/z_scan.rs",
                &["src/z_scan.rs"],
                RiskScore {
                    scanner: true,
                    ..RiskScore::default()
                },
            ),
            group(
                LaneId::Critique,
                "src/a_prior.rs",
                &["src/a_prior.rs"],
                RiskScore {
                    prior_finding: true,
                    ..RiskScore::default()
                },
            ),
            group(
                LaneId::Security,
                "src/auth/session.rs",
                &["src/auth/session.rs"],
                RiskScore {
                    sensitive: true,
                    ..RiskScore::default()
                },
            ),
            group(
                LaneId::Critique,
                "src/api.rs",
                &["src/api.rs"],
                RiskScore {
                    public_api: true,
                    ..RiskScore::default()
                },
            ),
        ];
        groups.extend((0..20).map(|index| {
            let path = format!("src/file_{index:02}.rs");
            group(LaneId::Security, &path, &[&path], RiskScore::default())
        }));
        groups.push(group(
            LaneId::Critique,
            "src/zz_related.rs + src/zz_test.rs",
            &["src/zz_related.rs", "src/zz_test.rs"],
            RiskScore::default(),
        ));
        let files = groups
            .iter()
            .flat_map(|candidate| candidate.group.paths.iter())
            .enumerate()
            .map(|(index, path)| ChangedFile {
                path: path.clone(),
                additions: if index == 0 { 1 } else { 0 },
                deletions: if index == 0 { 1 } else { 0 },
                patch: (path.as_str() != "src/zz_test.rs").then_some("untrusted patch text".into()),
                ..ChangedFile::default()
            })
            .collect::<Vec<_>>();
        // Deliberately scramble input order: large selection must rank across
        // lanes, not keep a per-lane prefix or inherit preparation order.
        groups.reverse();
        let plan = plan(&config, &files, &[], groups);

        assert_eq!(plan.mode, ReviewMode::Large);
        assert_eq!(plan.estimate.files, 28);
        assert_eq!(plan.estimate.changed_lines, 2);
        assert_eq!(plan.estimate.groups, 26);
        assert_eq!(
            plan.selected
                .iter()
                .map(|candidate| candidate.group.label.as_str())
                .collect::<Vec<_>>(),
            [
                "src/a_related.rs + src/a_test.rs",
                "src/z_scan.rs",
                "src/a_prior.rs",
                "src/auth/session.rs",
                "src/api.rs",
                "src/file_00.rs",
                "src/file_01.rs",
                "src/file_02.rs",
                "src/file_03.rs",
                "src/file_04.rs",
                "src/file_05.rs",
                "src/file_06.rs",
            ]
        );
        assert_eq!(
            plan.selected[0]
                .group
                .paths
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["src/a_related.rs", "src/a_test.rs"]
        );
        let omitted_security = (7..20)
            .map(|index| format!("src/file_{index:02}.rs"))
            .collect::<std::collections::BTreeSet<_>>();
        let omitted_critique = ["src/zz_related.rs", "src/zz_test.rs"]
            .into_iter()
            .map(str::to_string)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            plan.omitted
                .get(&LaneId::Security)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            omitted_security
        );
        assert_eq!(
            plan.omitted
                .get(&LaneId::Critique)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            omitted_critique
        );
        assert_eq!(
            plan.omitted_groups[&LaneId::Critique]
                .iter()
                .map(|group| group.paths.as_slice())
                .collect::<Vec<_>>(),
            [&[
                "src/zz_related.rs".to_string(),
                "src/zz_test.rs".to_string()
            ][..]],
            "the omission record retains the original multi-file group"
        );
        assert_eq!(
            plan.selected
                .iter()
                .filter(|g| g.lane == LaneId::Security)
                .count(),
            9
        );
        assert_eq!(
            plan.selected
                .iter()
                .filter(|g| g.lane == LaneId::Critique)
                .count(),
            3
        );
    }

    #[test]
    fn disabled_large_mode_keeps_normal_group_order() {
        let mut config = Config::default();
        config.review.large_pr.enabled = false;
        let files = changed_files(60);
        let mut groups = candidates(2);
        groups.reverse();
        groups[1].risk.scanner = true;
        let plan = plan(&config, &files, &[], groups);

        assert_eq!(plan.mode, ReviewMode::Normal);
        assert_eq!(
            plan.selected
                .iter()
                .map(|candidate| candidate.group.label.as_str())
                .collect::<Vec<_>>(),
            ["src/file_01.rs", "src/file_00.rs"]
        );
    }

    #[test]
    fn each_size_threshold_switches_at_its_inclusive_boundary() {
        let mut config = Config::default();
        config.review.large_pr.changed_lines = u64::MAX;
        config.review.large_pr.groups = usize::MAX;
        assert_eq!(
            plan(&config, &changed_files(29), &[], Vec::new()).mode,
            ReviewMode::Normal
        );
        assert_eq!(
            plan(&config, &changed_files(30), &[], Vec::new()).mode,
            ReviewMode::Large
        );

        config.review.large_pr.files = usize::MAX;
        config.review.large_pr.changed_lines = 2_000;
        for (lines, expected) in [(1_999, ReviewMode::Normal), (2_000, ReviewMode::Large)] {
            let files = [ChangedFile {
                additions: lines,
                ..ChangedFile::default()
            }];
            assert_eq!(plan(&config, &files, &[], Vec::new()).mode, expected);
        }

        config.review.large_pr.changed_lines = u64::MAX;
        config.review.large_pr.groups = 24;
        for (count, expected) in [(23, ReviewMode::Normal), (24, ReviewMode::Large)] {
            assert_eq!(plan(&config, &[], &[], candidates(count)).mode, expected);
        }
    }

    #[test]
    fn deletion_only_missing_patch_diffstat_triggers_large_mode() {
        let mut config = Config::default();
        config.review.large_pr.files = usize::MAX;
        config.review.large_pr.groups = usize::MAX;
        let files = [ChangedFile {
            path: "src/removed.rs".into(),
            additions: 0,
            deletions: 2_000,
            patch: None,
            ..ChangedFile::default()
        }];
        let plan = plan(&config, &files, &[], Vec::new());

        assert_eq!(plan.mode, ReviewMode::Large);
        assert_eq!(plan.estimate.changed_lines, 2_000);
    }

    #[test]
    fn changed_line_estimates_saturate_at_u64_max() {
        let mut config = Config::default();
        config.review.large_pr.files = usize::MAX;
        config.review.large_pr.changed_lines = u64::MAX;
        config.review.large_pr.groups = usize::MAX;
        let files = [ChangedFile {
            additions: u64::MAX,
            deletions: u64::MAX,
            ..ChangedFile::default()
        }];
        let plan = plan(&config, &files, &[], Vec::new());

        assert_eq!(plan.estimate.changed_lines, u64::MAX);
        assert_eq!(plan.mode, ReviewMode::Large);
    }

    #[test]
    fn supplied_host_risk_flags_rank_ahead_of_large_docs() {
        // This injects precomputed risk flags. Deriving them from scanner
        // results, prior anchors, sensitive paths and source text remains
        // outside this source-only scaffold.
        let config = Config::default();
        let files = [
            ChangedFile {
                path: "docs/reference.md".into(),
                additions: 5_000,
                ..ChangedFile::default()
            },
            ChangedFile {
                path: "src/auth/session.rs".into(),
                additions: 1,
                ..ChangedFile::default()
            },
        ];
        let group = |lane, path: &str, risk| PlannedGroup {
            lane,
            group: FileGroup {
                label: path.into(),
                paths: vec![path.into()],
            },
            risk,
        };
        let plan = plan(
            &config,
            &files,
            &[],
            vec![
                group(
                    LaneId::Critique,
                    "docs/reference.md",
                    RiskScore {
                        changed_lines: 1_000,
                        ..RiskScore::default()
                    },
                ),
                group(
                    LaneId::Critique,
                    "src/auth/session.rs",
                    RiskScore {
                        sensitive: true,
                        changed_lines: 1,
                        ..RiskScore::default()
                    },
                ),
                group(
                    LaneId::Security,
                    "src/scan.rs",
                    RiskScore {
                        scanner: true,
                        ..RiskScore::default()
                    },
                ),
                group(
                    LaneId::Critique,
                    "src/prior.rs",
                    RiskScore {
                        prior_finding: true,
                        ..RiskScore::default()
                    },
                ),
            ],
        );

        assert_eq!(plan.mode, ReviewMode::Large);
        assert_eq!(plan.selected[0].group.label, "src/scan.rs");
        assert_eq!(plan.selected[1].group.label, "src/prior.rs");
        assert_eq!(plan.selected[2].group.label, "src/auth/session.rs");
        assert_eq!(plan.selected[3].group.label, "docs/reference.md");
    }

    #[test]
    fn exact_risk_ties_put_security_before_critique() {
        let config = Config::default();
        let files = [ChangedFile {
            additions: 2_000,
            ..ChangedFile::default()
        }];
        let group = |lane| PlannedGroup {
            lane,
            group: FileGroup {
                label: "src/shared.rs".into(),
                paths: vec!["src/shared.rs".into()],
            },
            risk: RiskScore::default(),
        };
        let plan = plan(
            &config,
            &files,
            &[],
            vec![group(LaneId::Critique), group(LaneId::Security)],
        );

        assert_eq!(plan.selected[0].lane, LaneId::Security);
    }

    #[test]
    fn equal_highest_member_risk_uses_total_group_lines_before_path() {
        let mut config = Config::default();
        config.review.large_pr.files = 1;
        let files = [
            ChangedFile {
                path: "src/a_small.rs".into(),
                additions: 5,
                ..ChangedFile::default()
            },
            ChangedFile {
                path: "src/z_large.rs".into(),
                additions: 200,
                ..ChangedFile::default()
            },
        ];
        let planned = |path: &str| PlannedGroup {
            lane: LaneId::Critique,
            group: FileGroup {
                label: path.into(),
                paths: vec![path.into()],
            },
            risk: RiskScore {
                changed_lines: 50,
                ..RiskScore::default()
            },
        };
        let plan = plan(
            &config,
            &files,
            &[],
            vec![planned("src/z_large.rs"), planned("src/a_small.rs")],
        );

        assert_eq!(plan.mode, ReviewMode::Large);
        assert_eq!(plan.selected[0].group.label, "src/z_large.rs");
    }
}
