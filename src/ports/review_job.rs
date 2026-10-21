//! Durable admission and ownership for server review work.
//!
//! The server uses this port as the source of truth for accepted work. Wakeup
//! channels are hints only: a process can stop after persisting a request and
//! before sending its hint, so workers also poll this bounded journal.

use crate::error::Result;

/// Maximum accepted requests waiting or running across all server processes.
pub const MAX_UNFINISHED_REVIEWS: usize = 260;
/// Delivery and head-state reservations within the fixed eight-megabyte BSON
/// cap. Admission reserves dedupe IDs and future current/history head records
/// together, without evicting live tombstones or terminal heads.
pub const MAX_TERMINAL_REVIEW_RESERVATIONS: usize = 18_000;
/// Default maximum reviews running across all server processes; the server
/// passes its operator-configured bound to `claim_next_with_limit`.
pub const MAX_RUNNING_REVIEWS: usize = 4;
/// Paid recovery starts allowed for one accepted review across process restarts.
pub const MAX_REVIEW_PAID_ATTEMPTS: u32 = 5;
/// How long a job owner remains valid without a heartbeat.
pub const REVIEW_OWNER_TTL_MS: i64 = 45_000;
/// Short bounded owner claim for a queued status write, independent of review ownership.
pub const QUEUE_STATUS_CLAIM_TTL_MS: i64 = 300_000;
/// Bound active and detached check identities well below the BSON scheduler cap.
pub const MAX_REVIEW_STATUS_RECORDS: usize = 1_024;
/// How long terminal delivery IDs remain deduped.
pub const REVIEW_TOMBSTONE_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
/// Upper bound for any persisted identifier or descriptive field.
pub const REVIEW_JOB_FIELD_LIMIT: usize = 300;

/// Stable identity for one accepted review request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ReviewJobId(pub String);

/// Whether a recovered review should reuse incremental state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewJobMode {
    /// Reuse prior evidence and finding fingerprints.
    Incremental,
    /// Run a full review that ignores prior state.
    Full,
}

/// Minimal request metadata needed to resume an accepted review.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewRequest {
    /// Stable webhook delivery ID or generated manual-review ID.
    pub id: ReviewJobId,
    /// Repository in owner/name form.
    pub repo: String,
    /// Pull request number.
    pub number: u64,
    /// Contributor login used by trust policy.
    pub author: String,
    /// GitHub App installation ID.
    pub installation: u64,
    /// Revision carried by the request, when known.
    pub head_sha: Option<String>,
    /// Webhook delivery ID, absent for manual jobs.
    pub delivery: Option<String>,
    /// Webhook event type or `manual-review`.
    pub event: String,
    /// Review mode.
    pub mode: ReviewJobMode,
    /// Unix time in milliseconds when the server accepted this work.
    pub accepted_at_ms: i64,
}

/// Atomic admission result for one webhook or an entire manual expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Every request in the batch is durable and may be acknowledged.
    Admitted,
    /// Every request was already known; no duplicate work was added.
    Duplicate,
    /// The bounded unfinished or terminal reservation budget is full.
    Full,
}

/// Result of consuming one durable paid-work attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaidStart {
    /// This owner may proceed to credentials and review work.
    Started,
    /// The retry budget was exhausted; its owner can now conclude status and finish it.
    Exhausted,
    /// This process no longer owns the unexpired generation.
    OwnershipLost,
}

/// A request claimed by one server process and fenced by a generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedReview {
    /// The accepted request metadata.
    pub request: ReviewRequest,
    /// Every accepted delivery coalesced into this review.
    pub accepted_ids: Vec<ReviewJobId>,
    /// Random process owner identity.
    pub owner: String,
    /// Monotonic generation for this job's ownership.
    pub generation: u64,
    /// Durable queued check selected for this owner generation, if published.
    pub review_check: Option<ReviewCheckRecord>,
}

/// Stage of the durable umbrella check associated with one pinned head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCheckStage {
    /// The accepted review is waiting for a review permit.
    Queued,
    /// A worker owns the review and has started it.
    InProgress,
    /// The review check has a terminal conclusion.
    Completed,
}

/// Position in the durable eligible review order at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewQueuePosition {
    /// Eligible work that sorts before this request, including live owners.
    pub reviews_ahead: usize,
    /// Eligible work in the queue, including this request.
    pub eligible_reviews: usize,
}

/// Persisted identity for a status check, kept separate from authoritative
/// paid-head state so status metadata can never authorize model work.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewCheckRecord {
    /// The exact commit this run reports on.
    pub head_sha: String,
    /// Stable key used to recover a POST after a process restart.
    pub external_id: String,
    /// GitHub's run ID, absent until the first successful ensure call.
    #[serde(default)]
    pub check_id: Option<u64>,
    /// Last status successfully written to the forge.
    pub stage: ReviewCheckStage,
    /// Worker generation authorized to perform the latest transition.
    #[serde(default)]
    pub owner_generation: Option<u64>,
    /// Whether GitHub acknowledged the queued publication.
    #[serde(default)]
    pub published: bool,
    /// Set durably immediately before POST so retirement can distinguish
    /// never-attempted work from an ambiguous external write.
    #[serde(default)]
    pub post_attempted: bool,
    /// Durable short claim preventing concurrent retry workers from posting.
    #[serde(default)]
    pub write_claim_until_ms: i64,
    /// Monotonic status-publication claim generation, independent of review ownership.
    #[serde(default)]
    pub write_generation: u64,
}

/// A queued status publication claimed from the durable scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReviewCheck {
    /// Repository whose check status is being changed.
    pub repo: String,
    /// Installation for the server-owned check write.
    pub installation: u64,
    /// Check identity and fenced write claim.
    pub check: ReviewCheckRecord,
    /// Number of eligible earlier accepted jobs at claim time.
    pub reviews_ahead: usize,
    /// True when this identity is detached and needs a terminal status. If its
    /// remote ID cannot be recovered, publish a terminal check directly.
    pub retired: bool,
}

/// A headless accepted review claimed for status-only metadata resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReviewMetadata {
    /// Stable current scheduler request identity.
    pub job_id: ReviewJobId,
    /// Repository whose pull request metadata is being read.
    pub repo: String,
    /// App installation used for the narrow metadata read.
    pub installation: u64,
    /// Pull request number.
    pub number: u64,
    /// Fenced status metadata claim generation.
    pub generation: u64,
    /// Claim expiry in Unix milliseconds.
    pub claim_until_ms: i64,
}

/// A stale check retained until GitHub acknowledges a terminal status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetiredReviewCheck {
    /// Repository in owner/name form.
    pub repo: String,
    /// GitHub App installation that owns the check.
    pub installation: u64,
    /// Check identity and durable retry state.
    pub check: ReviewCheckRecord,
}

/// Result of atomically confirming paid-head state and reconciling status identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciledReviewCheck {
    /// The current live head has a published queued check and may proceed.
    Ready(ReviewCheckRecord),
    /// The current live head has a durable status intent awaiting publication.
    Pending(ReviewCheckRecord),
    /// The bounded status capacity is full; defer without spending paid work.
    CapacityDeferred,
    /// The caller no longer owns the durable review generation.
    OwnershipLost,
}

/// Terminal result retained as a dedupe tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    /// Review was published or reconciled as already published.
    Published,
    /// Trust or live eligibility policy declined the review.
    Declined,
    /// The queued revision was superseded before review.
    Superseded,
    /// A bounded review budget or retry policy ended the request.
    Exhausted,
    /// The wall-clock review deadline was reached for the confirmed head.
    Deadline,
}

/// One unfinished scheduler record and its fenced owner, if running.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScheduledReview {
    /// Accepted request metadata.
    pub request: ReviewRequest,
    /// Original durable order, preserved when incremental deliveries coalesce.
    #[serde(default)]
    pub enqueued_at_ms: i64,
    /// Every accepted delivery coalesced into this review and reserved for a
    /// terminal tombstone when the canonical work completes.
    #[serde(default)]
    pub accepted_ids: Vec<ReviewJobId>,
    /// Owner identity, absent while pending.
    pub owner: Option<String>,
    /// Ownership generation, increased on every claim.
    pub generation: u64,
    /// Owner lease expiry in Unix milliseconds.
    pub owner_until_ms: i64,
    /// Earliest retry time in Unix milliseconds.
    pub not_before_ms: i64,
    /// Paid-work starts consumed across process restarts.
    pub paid_attempts: u32,
    /// New incremental delivery arrived while this owner was running. The
    /// owner keeps its global slot until it observes and acknowledges this.
    #[serde(default)]
    pub yield_requested: bool,
    /// Most recently observed webhook head hint. It is never authoritative;
    /// the next owner still resolves the live head from Forge.
    #[serde(default)]
    pub latest_head_hint: Option<String>,
    /// Reserved slots for terminal-head history records this accepted work
    /// could create while resolving successive authoritative heads.
    #[serde(default)]
    pub head_history_reservations: Option<usize>,
    /// Check identities for accepted heads; never used as paid-head authority.
    #[serde(default)]
    pub review_checks: Vec<ReviewCheckRecord>,
    /// One bounded slot for a live SHA discovered after a delivery was accepted.
    #[serde(default = "default_true")]
    pub status_reconcile_reserved: bool,
    /// A newly accepted delivery has no event SHA yet and still needs a narrow
    /// metadata read before it can publish its UI-only queued check.
    #[serde(default)]
    pub metadata_resolution_pending: bool,
    /// Fenced claim for the status-only live metadata lookup.
    #[serde(default)]
    pub metadata_claim_generation: u64,
    /// Expiry for the status-only metadata lookup claim.
    #[serde(default)]
    pub metadata_claim_until_ms: i64,
}

fn default_true() -> bool {
    true
}

/// A completed ID retained to dedupe redeliveries for seven days.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewTombstone {
    /// Original request ID.
    pub id: ReviewJobId,
    /// Durable terminal outcome.
    pub outcome: TerminalOutcome,
    /// Unix millisecond expiration time.
    pub expires_at_ms: i64,
}

/// Current authoritative head and paid budget for one incremental PR.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IncrementalHeadBudget {
    /// Repository in owner/name form.
    pub repo: String,
    /// Pull request number.
    pub number: u64,
    /// Head SHA confirmed from Forge.
    pub head_sha: String,
    /// Paid starts consumed for this exact head across owners.
    pub paid_attempts: u32,
    /// Deadline exhaustion is terminal until an authoritative head change.
    pub deadline_hit: bool,
    /// The terminal result for this exact head, if it already has one.
    #[serde(default)]
    pub terminal_outcome: Option<TerminalOutcome>,
}

/// Recent terminal history preventing delayed delivery of an old head from
/// resetting its budget after the PR advanced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeadTerminalTombstone {
    /// Repository in owner/name form.
    pub repo: String,
    /// Pull request number.
    pub number: u64,
    /// Head SHA with a terminal result.
    pub head_sha: String,
    /// Terminal result for that head.
    pub outcome: TerminalOutcome,
    /// Unix millisecond expiration time.
    pub expires_at_ms: i64,
}

/// Whole single-document Mongo scheduler state, updated by revision CAS.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewSchedulerState {
    /// Monotonic revision used for compare-and-swap writes.
    pub revision: u64,
    /// Pending and running work. This list is never TTL-expired.
    pub unfinished: Vec<ScheduledReview>,
    /// Bounded, expiring delivery dedupe records.
    pub terminal: Vec<ReviewTombstone>,
    /// Authoritative current-head budgets, retained until each PR advances.
    #[serde(default)]
    pub incremental_heads: Vec<IncrementalHeadBudget>,
    /// Bounded recent history of terminal heads.
    #[serde(default)]
    pub head_history: Vec<HeadTerminalTombstone>,
    /// Detached check cleanup that survives removal of its review job.
    #[serde(default)]
    pub retired_checks: Vec<RetiredReviewCheck>,
}

impl ReviewSchedulerState {
    fn status_records_in_use(&self) -> usize {
        self.retired_checks.len()
            + self
                .unfinished
                .iter()
                .map(|record| record.review_checks.len())
                .sum::<usize>()
            + self
                .unfinished
                .iter()
                .filter(|record| record.status_reconcile_reserved)
                .count()
            + self
                .unfinished
                .iter()
                .filter(|record| record.metadata_resolution_pending)
                .count()
    }

    fn detach_checks_except(
        record: &mut ScheduledReview,
        keep_head: Option<&str>,
    ) -> Vec<RetiredReviewCheck> {
        Self::detach_checks_except_preserving(record, keep_head, None)
    }

    fn detach_checks_except_preserving(
        record: &mut ScheduledReview,
        keep_head: Option<&str>,
        preserve_status_head: Option<&str>,
    ) -> Vec<RetiredReviewCheck> {
        let mut keep = Vec::new();
        let mut retired = Vec::new();
        for check in std::mem::take(&mut record.review_checks) {
            if Some(check.head_sha.as_str()) == keep_head {
                keep.push(check);
            } else if Some(check.head_sha.as_str()) == preserve_status_head {
                // The paid review result is already known. A failed status
                // PATCH must not be converted into the opposite NotReviewed
                // conclusion just because a newer advisory head arrived.
            } else if check.stage != ReviewCheckStage::Completed
                && (check.published || check.check_id.is_some() || check.post_attempted)
            {
                let mut check = check;
                check.write_generation = check.write_generation.saturating_add(1);
                check.write_claim_until_ms = 0;
                retired.push(RetiredReviewCheck {
                    repo: record.request.repo.clone(),
                    installation: record.request.installation,
                    check,
                });
            }
        }
        record.review_checks = keep;
        retired
    }

    /// Claim a bounded batch of queued-check publications without claiming review work.
    pub fn claim_pending_checks(&mut self, now_ms: i64, limit: usize) -> Vec<PendingReviewCheck> {
        let mut ordered = self
            .unfinished
            .iter()
            .enumerate()
            .map(|(index, job)| (index, job.enqueued_at_ms, job.request.id.0.clone()))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(_, accepted_at, id)| (*accepted_at, id.clone()));
        let eligible = ordered
            .iter()
            .filter(|(index, _, _)| {
                let job = &self.unfinished[*index];
                let Some(head) = job.request.head_sha.as_deref() else {
                    return false;
                };
                job.not_before_ms <= now_ms
                    && job.review_checks.iter().any(|check| check.head_sha == head)
                    && (job.request.mode != ReviewJobMode::Incremental
                        || !self.incremental_heads.iter().any(|budget| {
                            budget.repo == job.request.repo
                                && budget.number == job.request.number
                                && budget.head_sha == head
                                && (budget.deadline_hit || budget.terminal_outcome.is_some())
                        }))
            })
            .map(|(index, _, _)| self.unfinished[*index].request.id.clone())
            .collect::<Vec<_>>();
        let mut claims = Vec::new();
        for (index, _, _) in ordered {
            if claims.len() >= limit {
                break;
            }
            let Some(check_index) = self.unfinished[index]
                .review_checks
                .iter()
                .position(|check| {
                    self.unfinished[index].request.head_sha.as_deref()
                        == Some(check.head_sha.as_str())
                        && !check.published
                        && check.write_claim_until_ms <= now_ms
                })
            else {
                continue;
            };
            let check = &mut self.unfinished[index].review_checks[check_index];
            check.write_generation = check.write_generation.saturating_add(1);
            check.write_claim_until_ms = now_ms.saturating_add(QUEUE_STATUS_CLAIM_TTL_MS);
            let check = check.clone();
            let reviews_ahead = eligible
                .iter()
                .position(|id| *id == self.unfinished[index].request.id)
                .unwrap_or(0);
            claims.push(PendingReviewCheck {
                repo: self.unfinished[index].request.repo.clone(),
                installation: self.unfinished[index].request.installation,
                check,
                reviews_ahead,
                retired: false,
            });
        }
        for retired in &mut self.retired_checks {
            if claims.len() >= limit {
                break;
            }
            if retired.check.write_claim_until_ms > now_ms {
                continue;
            }
            retired.check.write_generation = retired.check.write_generation.saturating_add(1);
            retired.check.write_claim_until_ms = now_ms.saturating_add(QUEUE_STATUS_CLAIM_TTL_MS);
            claims.push(PendingReviewCheck {
                repo: retired.repo.clone(),
                installation: retired.installation,
                check: retired.check.clone(),
                reviews_ahead: 0,
                retired: true,
            });
        }
        claims
    }

    /// Claim headless accepted jobs for narrow metadata resolution, without
    /// acquiring review ownership or consuming paid-work capacity.
    pub fn claim_pending_metadata(
        &mut self,
        now_ms: i64,
        limit: usize,
    ) -> Vec<PendingReviewMetadata> {
        let mut indices = self
            .unfinished
            .iter()
            .enumerate()
            .filter(|(_, job)| {
                job.metadata_resolution_pending
                    && job.request.head_sha.is_none()
                    && job.owner.is_none()
                    && job.not_before_ms <= now_ms
                    && job.metadata_claim_until_ms <= now_ms
            })
            .map(|(index, job)| (index, job.enqueued_at_ms, job.request.id.0.clone()))
            .collect::<Vec<_>>();
        indices.sort_by_key(|(_, accepted_at, id)| (*accepted_at, id.clone()));
        indices
            .into_iter()
            .take(limit)
            .map(|(index, _, _)| {
                let job = &mut self.unfinished[index];
                job.metadata_claim_generation = job.metadata_claim_generation.saturating_add(1);
                job.metadata_claim_until_ms = now_ms.saturating_add(QUEUE_STATUS_CLAIM_TTL_MS);
                PendingReviewMetadata {
                    job_id: job.request.id.clone(),
                    repo: job.request.repo.clone(),
                    installation: job.request.installation,
                    number: job.request.number,
                    generation: job.metadata_claim_generation,
                    claim_until_ms: job.metadata_claim_until_ms,
                }
            })
            .collect()
    }

    /// Revalidate a headless metadata claim at an external I/O boundary.
    pub fn metadata_claim_current(&self, pending: &PendingReviewMetadata, now_ms: i64) -> bool {
        self.unfinished.iter().any(|job| {
            job.request.id == pending.job_id
                && job.metadata_resolution_pending
                && job.request.head_sha.is_none()
                && job.owner.is_none()
                && job.metadata_claim_generation == pending.generation
                && job.metadata_claim_until_ms > now_ms
        })
    }

    /// Resolve a headless accepted request after a narrow metadata GET. The
    /// SHA is only an advisory queue hint; live review ownership confirms it
    /// again before paid work. Closed and draft PRs are tombstoned without a
    /// check write or model call.
    pub fn resolve_pending_metadata(
        &mut self,
        pending: &PendingReviewMetadata,
        head_sha: &str,
        open: bool,
        draft: bool,
        now_ms: i64,
    ) -> bool {
        let Some(index) = self.unfinished.iter().position(|job| {
            job.request.id == pending.job_id
                && job.metadata_resolution_pending
                && job.request.head_sha.is_none()
                && job.owner.is_none()
                && job.metadata_claim_generation == pending.generation
                && job.metadata_claim_until_ms > now_ms
        }) else {
            return false;
        };
        if !open || draft {
            let record = self.unfinished.remove(index);
            self.terminal
                .extend(record.accepted_ids.into_iter().map(|id| ReviewTombstone {
                    id,
                    outcome: TerminalOutcome::Declined,
                    expires_at_ms: now_ms.saturating_add(REVIEW_TOMBSTONE_TTL_MS),
                }));
            return true;
        }
        let job = &mut self.unfinished[index];
        let head = head_sha.to_owned();
        let external_id = queued_check_external_id(&job.request.id, &head);
        job.request.head_sha = Some(head.clone());
        job.latest_head_hint = Some(head.clone());
        job.metadata_resolution_pending = false;
        job.metadata_claim_until_ms = 0;
        if !job.review_checks.iter().any(|check| check.head_sha == head) {
            job.review_checks.push(ReviewCheckRecord {
                head_sha: head,
                external_id,
                check_id: None,
                stage: ReviewCheckStage::Queued,
                owner_generation: None,
                published: false,
                post_attempted: false,
                write_claim_until_ms: 0,
                write_generation: 0,
            });
        }
        true
    }

    /// Persist the GitHub check ID only while the same queued identity and write claim remain current.
    pub fn record_queued_check_id(
        &mut self,
        pending: &PendingReviewCheck,
        check_id: u64,
        now_ms: i64,
    ) -> bool {
        if pending.retired {
            let Some(check) = self
                .retired_checks
                .iter_mut()
                .find(|entry| entry.check.external_id == pending.check.external_id)
            else {
                return false;
            };
            if check.check.write_generation != pending.check.write_generation
                || check.check.write_claim_until_ms <= now_ms
            {
                return false;
            }
            check.check.check_id = Some(check_id);
            check.check.published = true;
            return true;
        }
        let Some(job) = self.unfinished.iter_mut().find(|job| {
            job.review_checks
                .iter()
                .any(|check| check.external_id == pending.check.external_id)
        }) else {
            return false;
        };
        let Some(check) = job.review_checks.iter_mut().find(|check| {
            check.head_sha == pending.check.head_sha
                && check.external_id == pending.check.external_id
        }) else {
            return false;
        };
        if job.request.head_sha.as_deref() != Some(check.head_sha.as_str())
            || check.published
            || check.write_generation != pending.check.write_generation
            || check.write_claim_until_ms <= now_ms
        {
            return false;
        }
        check.check_id = Some(check_id);
        check.published = true;
        check.write_claim_until_ms = 0;
        true
    }

    /// Verify a queued status claim immediately before an external read or write.
    pub fn queued_check_claim_current(&self, pending: &PendingReviewCheck, now_ms: i64) -> bool {
        if pending.retired {
            return self.retired_checks.iter().any(|entry| {
                entry.check.external_id == pending.check.external_id
                    && entry.check.write_generation == pending.check.write_generation
                    && entry.check.write_claim_until_ms > now_ms
            });
        }
        self.unfinished.iter().any(|job| {
            job.review_checks.iter().any(|check| {
                check.head_sha == pending.check.head_sha
                    && check.external_id == pending.check.external_id
                    && job.request.head_sha.as_deref() == Some(check.head_sha.as_str())
                    && !check.published
                    && check.write_generation == pending.check.write_generation
                    && check.write_claim_until_ms > now_ms
            })
        })
    }

    /// Persist that an active queued-check POST is about to cross the network boundary.
    pub fn mark_queue_post_attempted(&mut self, pending: &PendingReviewCheck, now_ms: i64) -> bool {
        if pending.retired || !self.queued_check_claim_current(pending, now_ms) {
            return false;
        }
        let Some(check) = self
            .unfinished
            .iter_mut()
            .flat_map(|job| job.review_checks.iter_mut())
            .find(|check| check.external_id == pending.check.external_id)
        else {
            return false;
        };
        check.post_attempted = true;
        true
    }

    /// Acknowledge a detached check's terminal write and remove its outbox entry.
    pub fn complete_retired_check(&mut self, pending: &PendingReviewCheck, now_ms: i64) -> bool {
        if !pending.retired {
            return false;
        }
        let Some(index) = self.retired_checks.iter().position(|entry| {
            entry.check.external_id == pending.check.external_id
                && entry.check.write_generation == pending.check.write_generation
                && entry.check.write_claim_until_ms > now_ms
                && entry.check.check_id.is_some()
        }) else {
            return false;
        };
        self.retired_checks.remove(index);
        true
    }

    /// Advance a check stage only for the live review owner generation.
    pub fn set_check_stage(
        &mut self,
        owned: &OwnedReview,
        head_sha: &str,
        check_id: u64,
        stage: ReviewCheckStage,
        now_ms: i64,
    ) -> bool {
        let Some(job) = self
            .unfinished
            .iter_mut()
            .find(|job| owns(job, owned) && job.owner_until_ms > now_ms)
        else {
            return false;
        };
        let Some(check) = job.review_checks.iter_mut().find(|check| {
            check.head_sha == head_sha
                && check.check_id == Some(check_id)
                && check.owner_generation == Some(owned.generation)
        }) else {
            return false;
        };
        check.stage = stage;
        true
    }

    /// Check the current owner, exact head, and check identity before a status transition.
    pub fn check_transition_current(
        &self,
        owned: &OwnedReview,
        head_sha: &str,
        check_id: u64,
        now_ms: i64,
    ) -> bool {
        self.unfinished.iter().any(|job| {
            owns(job, owned)
                && job.owner_until_ms > now_ms
                && job.request.head_sha.as_deref() == Some(head_sha)
                && job.review_checks.iter().any(|check| {
                    check.head_sha == head_sha
                        && check.check_id == Some(check_id)
                        && check.owner_generation == Some(owned.generation)
                        && check.stage == ReviewCheckStage::Queued
                })
        })
    }

    /// Atomically admit a whole batch in memory before the Mongo CAS persists it.
    pub fn admit_batch(&mut self, requests: Vec<ReviewRequest>, now_ms: i64) -> Admission {
        if requests.is_empty() {
            return Admission::Duplicate;
        }
        self.normalize_legacy_records();
        self.terminal.retain(|record| record.expires_at_ms > now_ms);
        self.head_history
            .retain(|record| record.expires_at_ms > now_ms);
        let mut incoming = std::collections::HashSet::new();
        if requests
            .iter()
            .any(|request| !incoming.insert(request.id.clone()) || !metadata_is_bounded(request))
        {
            return Admission::Full;
        }
        let status_records = self.status_records_in_use();
        let additional_status_records = requests
            .iter()
            .filter(|request| {
                let existing = (request.mode == ReviewJobMode::Incremental)
                    .then(|| {
                        self.unfinished.iter().find(|record| {
                            record.request.mode == ReviewJobMode::Incremental
                                && record.request.repo == request.repo
                                && record.request.number == request.number
                        })
                    })
                    .flatten();
                match (existing, request.head_sha.as_deref()) {
                    (Some(record), Some(head)) => !record
                        .review_checks
                        .iter()
                        .any(|check| check.head_sha == head),
                    (Some(_), None) => false,
                    (None, None) => true,
                    (None, Some(_)) => true,
                }
            })
            .count();
        let new_jobs = new_uncoalesced_count(&self.unfinished, &requests);
        let replenished_reservations = requests
            .iter()
            .filter(|request| {
                request.mode == ReviewJobMode::Incremental
                    && self.unfinished.iter().any(|record| {
                        record.request.mode == ReviewJobMode::Incremental
                            && record.request.repo == request.repo
                            && record.request.number == request.number
                            && !record.status_reconcile_reserved
                    })
            })
            .count();
        if status_records + additional_status_records + new_jobs + replenished_reservations
            > MAX_REVIEW_STATUS_RECORDS
        {
            return Admission::Full;
        }
        let already = requests
            .iter()
            .filter(|request| {
                self.unfinished
                    .iter()
                    .any(|record| record.accepted_ids.contains(&request.id))
                    || self.terminal.iter().any(|record| record.id == request.id)
            })
            .count();
        if already == requests.len() {
            return Admission::Duplicate;
        }
        let pending_prs = self
            .unfinished
            .iter()
            .filter(|record| record.request.mode == ReviewJobMode::Incremental)
            .map(|record| (record.request.repo.clone(), record.request.number))
            .collect::<std::collections::HashSet<_>>();
        let new_prs_without_head = requests
            .iter()
            .filter(|request| request.mode == ReviewJobMode::Incremental)
            .map(|request| (request.repo.clone(), request.number))
            .filter(|key| {
                !pending_prs.contains(key)
                    && !self
                        .incremental_heads
                        .iter()
                        .any(|budget| budget.repo == key.0 && budget.number == key.1)
            })
            .collect::<std::collections::HashSet<_>>()
            .len();
        let incoming_history_reservations = requests
            .iter()
            .filter(|request| request.mode == ReviewJobMode::Incremental)
            .count();
        if already != 0
            || self.unfinished.len() + new_jobs > MAX_UNFINISHED_REVIEWS
            || self.reserved_capacity()
                + requests.len()
                + incoming_history_reservations
                + new_prs_without_head
                > MAX_TERMINAL_REVIEW_RESERVATIONS
        {
            return Admission::Full;
        }
        for request in requests {
            let mut detached_checks = Vec::new();
            let initial_check = request.head_sha.as_ref().map(|head_sha| ReviewCheckRecord {
                head_sha: head_sha.clone(),
                external_id: queued_check_external_id(&request.id, head_sha),
                check_id: None,
                stage: ReviewCheckStage::Queued,
                owner_generation: None,
                published: false,
                post_attempted: false,
                write_claim_until_ms: 0,
                write_generation: 0,
            });
            let coalesce = request.mode == ReviewJobMode::Incremental;
            if let Some(record) = coalesce
                .then(|| {
                    self.unfinished.iter_mut().find(|record| {
                        record.request.mode == ReviewJobMode::Incremental
                            && record.request.repo == request.repo
                            && record.request.number == request.number
                    })
                })
                .flatten()
            {
                record.accepted_ids.push(request.id.clone());
                if request.head_sha.is_none() && record.request.head_sha.is_some() {
                    // A headless comment must not erase an already accepted
                    // advisory head or its queued check identity.
                    continue;
                }
                record.latest_head_hint = request.head_sha.clone();
                let new_head = request.head_sha.clone();
                let is_unowned_head_change =
                    record.owner.is_none() && record.request.head_sha != request.head_sha;
                if let Some(check) = initial_check
                    && !record
                        .review_checks
                        .iter()
                        .any(|existing| existing.head_sha == check.head_sha)
                {
                    record.review_checks.push(check);
                }
                record.head_history_reservations =
                    Some(record.head_history_reservations.unwrap_or_default() + 1);
                record.status_reconcile_reserved = true;
                record.metadata_resolution_pending = request.head_sha.is_none();
                record.metadata_claim_generation =
                    record.metadata_claim_generation.saturating_add(1);
                record.metadata_claim_until_ms = 0;
                if record.owner.is_some() {
                    if record.request.head_sha != request.head_sha {
                        record.yield_requested = true;
                    }
                } else {
                    if is_unowned_head_change {
                        detached_checks = Self::detach_checks_except(record, new_head.as_deref());
                    }
                    record.request = request;
                }
                self.retired_checks.extend(detached_checks);
                continue;
            }
            let metadata_resolution_pending =
                request.mode == ReviewJobMode::Incremental && request.head_sha.is_none();
            self.unfinished.push(ScheduledReview {
                enqueued_at_ms: request.accepted_at_ms,
                accepted_ids: vec![request.id.clone()],
                latest_head_hint: request.head_sha.clone(),
                head_history_reservations: Some(usize::from(
                    request.mode == ReviewJobMode::Incremental,
                )),
                request,
                owner: None,
                generation: 0,
                owner_until_ms: 0,
                not_before_ms: 0,
                paid_attempts: 0,
                yield_requested: false,
                review_checks: initial_check.into_iter().collect(),
                status_reconcile_reserved: true,
                metadata_resolution_pending,
                metadata_claim_generation: 0,
                metadata_claim_until_ms: 0,
            });
        }
        Admission::Admitted
    }

    /// Count persisted records and all capacity promised to accepted work.
    /// The reservation makes later live-head transitions fit without evicting
    /// delivery tombstones or terminal-head history.
    pub fn reserved_capacity(&self) -> usize {
        let pending_prs = self
            .unfinished
            .iter()
            .filter(|record| record.request.mode == ReviewJobMode::Incremental)
            .map(|record| (record.request.repo.clone(), record.request.number))
            .collect::<std::collections::HashSet<_>>();
        let pending_head_slots = pending_prs
            .iter()
            .filter(|(repo, number)| {
                !self
                    .incremental_heads
                    .iter()
                    .any(|budget| budget.repo == *repo && budget.number == *number)
            })
            .count();
        self.terminal.len()
            + self.retired_checks.len()
            + self
                .unfinished
                .iter()
                .map(|record| record.accepted_ids.len().max(1))
                .sum::<usize>()
            + self.incremental_heads.len()
            + self.head_history.len()
            + self
                .unfinished
                .iter()
                .map(|record| record.review_checks.len())
                .sum::<usize>()
            + self
                .unfinished
                .iter()
                .filter(|record| record.status_reconcile_reserved)
                .count()
            + self
                .unfinished
                .iter()
                .map(|record| {
                    if record.request.mode == ReviewJobMode::Incremental {
                        record
                            .head_history_reservations
                            .unwrap_or_else(|| record.accepted_ids.len().max(1))
                    } else {
                        0
                    }
                })
                .sum::<usize>()
            + pending_head_slots
    }

    /// Fill fields added after the durable scheduler was first deployed.
    /// Missing aggregate IDs mean the record predates supersession support and
    /// therefore reserves its original request ID.
    pub fn normalize_legacy_records(&mut self) {
        for record in &mut self.unfinished {
            if record.accepted_ids.is_empty() {
                record.accepted_ids.push(record.request.id.clone());
            }
            if record.latest_head_hint.is_none() {
                record.latest_head_hint = record.request.head_sha.clone();
            }
            if record.request.mode == ReviewJobMode::Incremental
                && record.head_history_reservations.is_none()
            {
                record.head_history_reservations = Some(record.accepted_ids.len());
            }
        }
    }

    /// Claim the oldest eligible record within the configured global bound.
    pub fn claim_next_with_limit(
        &mut self,
        owner: &str,
        now_ms: i64,
        limit: usize,
    ) -> Option<OwnedReview> {
        let running = self
            .unfinished
            .iter()
            .filter(|record| record.owner.is_some() && record.owner_until_ms > now_ms)
            .count();
        if running >= limit {
            return None;
        }
        let record = self.unfinished.iter_mut().find(|record| {
            record.not_before_ms <= now_ms
                && (record.owner.is_none() || record.owner_until_ms <= now_ms)
                && !record.metadata_resolution_pending
                && (record.review_checks.is_empty()
                    || record.request.head_sha.as_deref().is_none_or(|head| {
                        record.review_checks.iter().any(|check| {
                            check.head_sha == head && check.published && check.check_id.is_some()
                        })
                    }))
        })?;
        record.generation += 1;
        record.owner = Some(owner.to_owned());
        record.owner_until_ms = now_ms + REVIEW_OWNER_TTL_MS;
        let generation = record.generation;
        let review_check = record
            .request
            .head_sha
            .as_deref()
            .and_then(|head| {
                record
                    .review_checks
                    .iter_mut()
                    .find(|check| check.head_sha == head)
            })
            .map(|check| {
                check.owner_generation = Some(generation);
                check.clone()
            });
        Some(OwnedReview {
            request: record.request.clone(),
            accepted_ids: record.accepted_ids.clone(),
            owner: owner.to_owned(),
            generation,
            review_check,
        })
    }

    /// Claim using the default global review bound.
    pub fn claim_next(&mut self, owner: &str, now_ms: i64) -> Option<OwnedReview> {
        self.claim_next_with_limit(owner, now_ms, MAX_RUNNING_REVIEWS)
    }

    /// Consume one paid attempt, leaving exhausted work owned for status cleanup.
    pub fn begin_paid(&mut self, owned: &OwnedReview, now_ms: i64) -> PaidStart {
        let Some(index) = self
            .unfinished
            .iter()
            .position(|record| owns(record, owned) && record.owner_until_ms > now_ms)
        else {
            return PaidStart::OwnershipLost;
        };
        if self.unfinished[index].request.mode == ReviewJobMode::Incremental {
            let request = &self.unfinished[index].request;
            if !self
                .incremental_heads
                .iter()
                .any(|budget| budget.repo == request.repo && budget.number == request.number)
            {
                // A webhook SHA is only a hint. Production must confirm a live
                // head from Forge before paid work can start.
                return PaidStart::OwnershipLost;
            }
            let Some(budget) = self
                .incremental_heads
                .iter_mut()
                .find(|budget| budget.repo == request.repo && budget.number == request.number)
            else {
                return PaidStart::OwnershipLost;
            };
            if budget.terminal_outcome.is_some()
                || budget.deadline_hit
                || self.head_history.iter().any(|record| {
                    record.repo == request.repo
                        && record.number == request.number
                        && record.head_sha == budget.head_sha
                        && record.expires_at_ms > now_ms
                })
            {
                return PaidStart::Exhausted;
            }
            if budget.paid_attempts >= MAX_REVIEW_PAID_ATTEMPTS {
                return PaidStart::Exhausted;
            }
            budget.paid_attempts += 1;
            return PaidStart::Started;
        }
        if self.unfinished[index].paid_attempts >= MAX_REVIEW_PAID_ATTEMPTS {
            return PaidStart::Exhausted;
        }
        self.unfinished[index].paid_attempts += 1;
        PaidStart::Started
    }

    /// Bind an owned incremental request to the authoritative live head. A
    /// webhook SHA hint cannot reset a deadline marker or paid-start count.
    pub fn confirm_head(&mut self, owned: &OwnedReview, head_sha: &str, now_ms: i64) -> bool {
        let Some(index) = self
            .unfinished
            .iter()
            .position(|record| owns(record, owned) && record.owner_until_ms > now_ms)
        else {
            return false;
        };
        if self.unfinished[index].request.mode != ReviewJobMode::Incremental {
            return true;
        }
        let repo = self.unfinished[index].request.repo.clone();
        let number = self.unfinished[index].request.number;
        self.head_history
            .retain(|record| record.expires_at_ms > now_ms);
        if let Some(current) = self
            .incremental_heads
            .iter_mut()
            .find(|budget| budget.repo == repo && budget.number == number)
        {
            if current.head_sha == head_sha {
                self.unfinished[index].request.head_sha = Some(head_sha.to_owned());
                return true;
            }
            if current.terminal_outcome.is_some() || current.deadline_hit {
                if self.unfinished[index]
                    .head_history_reservations
                    .unwrap_or_default()
                    == 0
                {
                    return false;
                }
                self.unfinished[index].head_history_reservations = Some(
                    self.unfinished[index]
                        .head_history_reservations
                        .unwrap_or_default()
                        - 1,
                );
                self.head_history.push(HeadTerminalTombstone {
                    repo: repo.clone(),
                    number,
                    head_sha: current.head_sha.clone(),
                    outcome: current
                        .terminal_outcome
                        .unwrap_or(TerminalOutcome::Deadline),
                    expires_at_ms: now_ms + REVIEW_TOMBSTONE_TTL_MS,
                });
            }
            current.head_sha = head_sha.to_owned();
            let historical_outcome = self
                .head_history
                .iter()
                .find(|record| {
                    record.repo == repo
                        && record.number == number
                        && record.head_sha == head_sha
                        && record.expires_at_ms > now_ms
                })
                .map(|record| record.outcome);
            let historical_deadline = historical_outcome == Some(TerminalOutcome::Deadline);
            current.paid_attempts = if historical_deadline {
                MAX_REVIEW_PAID_ATTEMPTS
            } else {
                0
            };
            current.deadline_hit = historical_deadline;
            current.terminal_outcome = historical_outcome;
            self.unfinished[index].request.head_sha = Some(head_sha.to_owned());
            return true;
        }
        if self.unfinished[index].request.mode != ReviewJobMode::Incremental {
            return false;
        }
        self.incremental_heads.push(IncrementalHeadBudget {
            repo,
            number,
            head_sha: head_sha.to_owned(),
            // Legacy records counted paid starts before authoritative head
            // confirmation existed. Preserve that conservative count on the
            // first confirmed head; only a later confirmed transition resets it.
            paid_attempts: self.unfinished[index].paid_attempts,
            deadline_hit: false,
            terminal_outcome: None,
        });
        self.unfinished[index].request.head_sha = Some(head_sha.to_owned());
        true
    }

    /// Confirm the live head, retire stale status identities, and ensure its
    /// queued check in one scheduler mutation. Paid-head authority remains in
    /// `confirm_head`; status identity is maintained separately.
    pub fn reconcile_live_check(
        &mut self,
        owned: &OwnedReview,
        head_sha: &str,
        now_ms: i64,
    ) -> ReconciledReviewCheck {
        let Some(index) = self
            .unfinished
            .iter()
            .position(|record| owns(record, owned) && record.owner_until_ms > now_ms)
        else {
            return ReconciledReviewCheck::OwnershipLost;
        };
        let has_check = self.unfinished[index]
            .review_checks
            .iter()
            .any(|check| check.head_sha == head_sha);
        let status_count = self.status_records_in_use();
        if !has_check
            && !self.unfinished[index].status_reconcile_reserved
            && status_count >= MAX_REVIEW_STATUS_RECORDS
        {
            return ReconciledReviewCheck::CapacityDeferred;
        }
        if !self.confirm_head(owned, head_sha, now_ms) {
            return ReconciledReviewCheck::OwnershipLost;
        }
        let record = &mut self.unfinished[index];
        record.request.head_sha = Some(head_sha.to_owned());
        let detached = Self::detach_checks_except(record, Some(head_sha));
        let check = if let Some(existing) = record
            .review_checks
            .iter_mut()
            .find(|check| check.head_sha == head_sha)
        {
            existing.owner_generation = Some(owned.generation);
            existing.clone()
        } else {
            record.status_reconcile_reserved = false;
            let check = ReviewCheckRecord {
                head_sha: head_sha.to_owned(),
                external_id: queued_check_external_id(&record.request.id, head_sha),
                check_id: None,
                stage: ReviewCheckStage::Queued,
                owner_generation: Some(owned.generation),
                published: false,
                post_attempted: false,
                write_claim_until_ms: 0,
                write_generation: 0,
            };
            record.review_checks.push(check.clone());
            check
        };
        let ready = check.published && check.check_id.is_some();
        self.retired_checks.extend(detached);
        if ready {
            ReconciledReviewCheck::Ready(check)
        } else {
            ReconciledReviewCheck::Pending(check)
        }
    }

    /// Whether a confirmed incremental head already has a terminal result.
    pub fn head_is_terminal(&self, owned: &OwnedReview, now_ms: i64) -> bool {
        self.unfinished
            .iter()
            .find(|record| owns(record, owned) && record.owner_until_ms > now_ms)
            .filter(|record| record.request.mode == ReviewJobMode::Incremental)
            .and_then(|record| {
                self.incremental_heads.iter().find(|budget| {
                    budget.repo == record.request.repo && budget.number == record.request.number
                })
            })
            .is_some_and(|budget| budget.terminal_outcome.is_some() || budget.deadline_hit)
    }

    /// Record an owned deadline and persist a marker for the confirmed SHA
    /// beyond delivery tombstones. If a push raced the deadline, preserve the
    /// accepted job for the next owner while keeping that head terminal.
    pub fn finish_deadline(&mut self, owned: &OwnedReview, now_ms: i64) -> bool {
        self.finish_with_yield(owned, TerminalOutcome::Deadline, now_ms)
    }

    /// Complete a terminal result while preserving a superseding delivery.
    /// The terminal marker belongs to the confirmed head; accepted IDs stay
    /// attached to the queued job so the replacement owner resolves Forge's
    /// actual head before deciding whether any work remains.
    pub fn finish_with_yield(
        &mut self,
        owned: &OwnedReview,
        outcome: TerminalOutcome,
        now_ms: i64,
    ) -> bool {
        let Some(index) = self
            .unfinished
            .iter()
            .position(|record| owns(record, owned) && record.owner_until_ms > now_ms)
        else {
            return false;
        };
        let request = self.unfinished[index].request.clone();
        let mut existing_terminal = None;
        if request.mode == ReviewJobMode::Incremental {
            let Some(budget) = self
                .incremental_heads
                .iter_mut()
                .find(|budget| budget.repo == request.repo && budget.number == request.number)
            else {
                return false;
            };
            existing_terminal = budget
                .terminal_outcome
                .or_else(|| budget.deadline_hit.then_some(TerminalOutcome::Deadline));
            if existing_terminal.is_none()
                && matches!(
                    outcome,
                    TerminalOutcome::Published
                        | TerminalOutcome::Declined
                        | TerminalOutcome::Exhausted
                        | TerminalOutcome::Deadline
                )
            {
                budget.terminal_outcome = Some(outcome);
            }
            if existing_terminal.is_none() && outcome == TerminalOutcome::Deadline {
                budget.deadline_hit = true;
            }
        }
        if self.unfinished[index].yield_requested {
            let head = self.unfinished[index].latest_head_hint.clone();
            let preserve_status_head = (outcome == TerminalOutcome::Published)
                .then(|| self.unfinished[index].request.head_sha.clone())
                .flatten();
            let detached = {
                let record = &mut self.unfinished[index];
                record.owner = None;
                record.owner_until_ms = 0;
                record.not_before_ms = now_ms;
                record.request.head_sha = head.clone();
                record.yield_requested = false;
                Self::detach_checks_except_preserving(
                    record,
                    head.as_deref(),
                    preserve_status_head.as_deref(),
                )
            };
            self.retired_checks.extend(detached);
            return true;
        }
        if outcome != TerminalOutcome::Deadline {
            return self.finish(owned, outcome, now_ms, now_ms);
        }
        let record = self.unfinished.remove(index);
        self.retire_removed_record_checks(
            &record,
            existing_terminal.unwrap_or(TerminalOutcome::Deadline),
        );
        self.terminal
            .extend(record.accepted_ids.into_iter().map(|id| ReviewTombstone {
                id,
                outcome: existing_terminal.unwrap_or(TerminalOutcome::Deadline),
                expires_at_ms: now_ms + REVIEW_TOMBSTONE_TTL_MS,
            }));
        true
    }

    /// Extend the current unexpired owner's lease.
    pub fn renew(&mut self, owned: &OwnedReview, now_ms: i64) -> bool {
        let Some(record) = self
            .unfinished
            .iter_mut()
            .find(|record| owns(record, owned))
        else {
            return false;
        };
        if record.owner_until_ms <= now_ms {
            return false;
        }
        record.owner_until_ms = now_ms + REVIEW_OWNER_TTL_MS;
        true
    }

    /// Check the current unexpired claim before paid work or publication.
    pub fn owns(&self, owned: &OwnedReview, now_ms: i64) -> bool {
        self.unfinished
            .iter()
            .find(|record| owns(record, owned))
            .is_some_and(|record| record.owner_until_ms > now_ms)
    }

    /// Whether an owner may begin or continue paid model work. A superseding
    /// delivery only asks the owner to yield; ownership stays fenced until it
    /// acknowledges, so the global running count cannot be exceeded.
    pub fn should_continue(&self, owned: &OwnedReview, now_ms: i64) -> bool {
        self.unfinished
            .iter()
            .find(|record| owns(record, owned))
            .is_some_and(|record| record.owner_until_ms > now_ms && !record.yield_requested)
    }

    /// Acknowledge an advisory supersession and return this same accepted work
    /// to the queue with the latest hint. Accepted IDs and paid-start history
    /// remain attached to it.
    pub fn defer_yield(&mut self, owned: &OwnedReview, now_ms: i64) -> bool {
        let Some(index) = self.unfinished.iter().position(|record| {
            owns(record, owned) && record.owner_until_ms > now_ms && record.yield_requested
        }) else {
            return false;
        };
        let head = self.unfinished[index].latest_head_hint.clone();
        let detached = {
            let record = &mut self.unfinished[index];
            record.owner = None;
            record.owner_until_ms = 0;
            record.not_before_ms = now_ms;
            record.request.head_sha = head.clone();
            record.yield_requested = false;
            Self::detach_checks_except(record, head.as_deref())
        };
        self.retired_checks.extend(detached);
        true
    }

    /// Return the current owner's request to pending with bounded backoff.
    pub fn defer(&mut self, owned: &OwnedReview, now_ms: i64, not_before_ms: i64) -> bool {
        let Some(index) = self
            .unfinished
            .iter()
            .position(|record| owns(record, owned))
        else {
            return false;
        };
        if self.unfinished[index].owner_until_ms <= now_ms {
            return false;
        }
        let head = self.unfinished[index]
            .yield_requested
            .then(|| self.unfinished[index].latest_head_hint.clone());
        let detached = {
            let record = &mut self.unfinished[index];
            record.owner = None;
            record.owner_until_ms = 0;
            record.not_before_ms = not_before_ms;
            if let Some(head) = head {
                record.request.head_sha = head.clone();
                record.yield_requested = false;
                Self::detach_checks_except(record, head.as_deref())
            } else {
                Vec::new()
            }
        };
        self.retired_checks.extend(detached);
        true
    }

    /// Complete the current owner's request and reserve its dedupe tombstone.
    pub fn finish(
        &mut self,
        owned: &OwnedReview,
        outcome: TerminalOutcome,
        now_ms: i64,
        completed_at_ms: i64,
    ) -> bool {
        if self.unfinished.iter().any(|record| {
            owns(record, owned) && record.owner_until_ms > now_ms && record.yield_requested
        }) {
            return self.finish_with_yield(owned, outcome, now_ms);
        }
        let Some(index) = self.unfinished.iter().position(|record| {
            owns(record, owned) && record.owner_until_ms > now_ms && !record.yield_requested
        }) else {
            return false;
        };
        let record = self.unfinished.remove(index);
        let existing_terminal = if record.request.mode == ReviewJobMode::Incremental {
            self.incremental_heads
                .iter()
                .find(|budget| {
                    budget.repo == record.request.repo && budget.number == record.request.number
                })
                .and_then(|budget| {
                    budget
                        .terminal_outcome
                        .or_else(|| budget.deadline_hit.then_some(TerminalOutcome::Deadline))
                })
        } else {
            None
        };
        self.retire_removed_record_checks(&record, existing_terminal.unwrap_or(outcome));
        if existing_terminal.is_none()
            && record.request.mode == ReviewJobMode::Incremental
            && matches!(
                outcome,
                TerminalOutcome::Published
                    | TerminalOutcome::Declined
                    | TerminalOutcome::Exhausted
                    | TerminalOutcome::Deadline
            )
            && let Some(budget) = self.incremental_heads.iter_mut().find(|budget| {
                budget.repo == record.request.repo && budget.number == record.request.number
            })
        {
            budget.terminal_outcome = Some(outcome);
            if outcome == TerminalOutcome::Deadline {
                budget.deadline_hit = true;
            }
        }
        let dedupe_outcome = existing_terminal.unwrap_or(outcome);
        self.terminal
            .extend(record.accepted_ids.into_iter().map(|id| ReviewTombstone {
                id,
                outcome: dedupe_outcome,
                expires_at_ms: completed_at_ms + REVIEW_TOMBSTONE_TTL_MS,
            }));
        true
    }

    fn retire_removed_record_checks(
        &mut self,
        record: &ScheduledReview,
        terminal_outcome: TerminalOutcome,
    ) {
        for mut check in record.review_checks.iter().cloned() {
            let superseded =
                check.head_sha != record.request.head_sha.as_deref().unwrap_or_default();
            let never_started = check.stage == ReviewCheckStage::Queued
                && terminal_outcome != TerminalOutcome::Published;
            let superseded_or_never_started = superseded || never_started;
            if !superseded_or_never_started
                || check.stage == ReviewCheckStage::Completed
                || (!check.published && check.check_id.is_none() && !check.post_attempted)
            {
                continue;
            }
            check.write_generation = check.write_generation.saturating_add(1);
            check.write_claim_until_ms = 0;
            if !self
                .retired_checks
                .iter()
                .any(|entry| entry.check.external_id == check.external_id)
            {
                self.retired_checks.push(RetiredReviewCheck {
                    repo: record.request.repo.clone(),
                    installation: record.request.installation,
                    check,
                });
            }
        }
    }
}

fn queued_check_external_id(job: &ReviewJobId, head_sha: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{}\0{head_sha}", job.0).as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("tinysweeper:review:{hex}")
}

fn new_uncoalesced_count(records: &[ScheduledReview], requests: &[ReviewRequest]) -> usize {
    let mut keys = records
        .iter()
        .filter(|record| record.request.mode == ReviewJobMode::Incremental)
        .map(|record| (record.request.repo.clone(), record.request.number))
        .collect::<std::collections::HashSet<_>>();
    requests
        .iter()
        .filter(|request| {
            request.mode != ReviewJobMode::Incremental
                || keys.insert((request.repo.clone(), request.number))
        })
        .count()
}

fn metadata_is_bounded(request: &ReviewRequest) -> bool {
    request.id.0.len() <= REVIEW_JOB_FIELD_LIMIT
        && request.repo.len() <= REVIEW_JOB_FIELD_LIMIT
        && request.author.len() <= REVIEW_JOB_FIELD_LIMIT
        && request
            .head_sha
            .as_ref()
            .is_none_or(|value| value.len() <= REVIEW_JOB_FIELD_LIMIT)
        && request
            .delivery
            .as_ref()
            .is_none_or(|value| value.len() <= REVIEW_JOB_FIELD_LIMIT)
        && request.event.len() <= REVIEW_JOB_FIELD_LIMIT
}

fn owns(record: &ScheduledReview, owned: &OwnedReview) -> bool {
    record.request.id == owned.request.id
        && record.owner.as_deref() == Some(owned.owner.as_str())
        && record.generation == owned.generation
}

/// Atomic journal for accepted server reviews.
#[async_trait::async_trait]
pub trait ReviewJobStore: Send + Sync {
    /// Cheaply reject manual work when the durable scheduler has no room.
    /// This is only a preflight; `admit_batch` remains authoritative and atomic.
    async fn has_capacity_for(&self, count: usize) -> Result<bool>;

    /// Atomically admit all requests or none, deduping against unfinished and
    /// unexpired terminal IDs in the same scheduler document.
    async fn admit_batch(&self, requests: Vec<ReviewRequest>) -> Result<Admission>;

    /// Claim queued-check publication work independently from review permits and owners.
    async fn claim_pending_checks(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<PendingReviewCheck>>;

    /// Claim headless accepted work for status-only live metadata resolution.
    async fn claim_pending_metadata(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<PendingReviewMetadata>>;

    /// Revalidate a metadata claim after the narrow Forge read.
    async fn metadata_claim_current(
        &self,
        pending: &PendingReviewMetadata,
        now_ms: i64,
    ) -> Result<bool>;

    /// Resolve a metadata claim, producing a queue hint or terminally declining
    /// a still-current closed/draft job.
    async fn resolve_pending_metadata(
        &self,
        pending: &PendingReviewMetadata,
        head_sha: &str,
        open: bool,
        draft: bool,
        now_ms: i64,
    ) -> Result<bool>;

    /// Record an acknowledged queued check only if its durable write claim is still current.
    async fn record_queued_check_id(
        &self,
        pending: &PendingReviewCheck,
        check_id: u64,
        now_ms: i64,
    ) -> Result<bool>;

    /// Revalidate a status claim at an external-I/O boundary.
    async fn queued_check_claim_current(
        &self,
        pending: &PendingReviewCheck,
        now_ms: i64,
    ) -> Result<bool>;

    /// Persist a POST-attempt marker before sending an ambiguous external write.
    async fn mark_queue_post_attempted(
        &self,
        pending: &PendingReviewCheck,
        now_ms: i64,
    ) -> Result<bool>;

    /// Remove a retired status only after its terminal GitHub write succeeds.
    async fn complete_retired_check(
        &self,
        pending: &PendingReviewCheck,
        now_ms: i64,
    ) -> Result<bool>;

    /// Persist a queued/in-progress/completed transition under the current review owner fence.
    async fn set_check_stage(
        &self,
        owned: &OwnedReview,
        head_sha: &str,
        check_id: u64,
        stage: ReviewCheckStage,
        now_ms: i64,
    ) -> Result<bool>;

    /// Verify a queued-to-in-progress mutation is still owned and bound to this head.
    async fn check_transition_current(
        &self,
        owned: &OwnedReview,
        head_sha: &str,
        check_id: u64,
        now_ms: i64,
    ) -> Result<bool>;

    /// Claim one eligible request if fewer than four are running globally.
    /// The implementation must never take an unexpired foreign owner.
    async fn claim_next(&self, owner: &str, now_ms: i64) -> Result<Option<OwnedReview>>;

    /// Claim using the trusted deployment pool size across all server processes.
    async fn claim_next_with_limit(
        &self,
        owner: &str,
        now_ms: i64,
        limit: usize,
    ) -> Result<Option<OwnedReview>> {
        let _ = limit;
        self.claim_next(owner, now_ms).await
    }

    /// Consume a paid attempt, retaining exhausted work until its owner concludes status.
    async fn begin_paid(&self, owned: &OwnedReview, now_ms: i64) -> Result<PaidStart>;

    /// Confirm the live head fetched from Forge after the review gate and lease.
    async fn confirm_head(&self, owned: &OwnedReview, head_sha: &str, now_ms: i64) -> Result<bool>;

    /// Reconcile a live SHA with its durable status identity under the same owner fence.
    async fn reconcile_live_check(
        &self,
        owned: &OwnedReview,
        head_sha: &str,
        now_ms: i64,
    ) -> Result<ReconciledReviewCheck>;

    /// Whether the confirmed head already has a terminal result.
    async fn head_is_terminal(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool>;

    /// Extend a live claim. A stale generation can never renew another
    /// process's job.
    async fn renew(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool>;

    /// Verify the claim is still current and unexpired before paid work or
    /// publication.
    async fn owns(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool>;

    /// Check ownership and the durable cooperative-yield flag before model
    /// dispatch. False means this owner must stop and return its job to pending.
    async fn should_continue(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool>;

    /// Return an owned request to pending state, optionally with backoff.
    /// The owner and generation must fence this update.
    async fn defer(&self, owned: &OwnedReview, now_ms: i64, not_before_ms: i64) -> Result<bool>;

    /// Release a live owner only after it has observed an advisory yield.
    async fn defer_yield(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool>;

    /// Finish an owned request and retain a bounded dedupe tombstone.
    /// The owner and generation must fence this update.
    async fn finish(
        &self,
        owned: &OwnedReview,
        outcome: TerminalOutcome,
        now_ms: i64,
        completed_at_ms: i64,
    ) -> Result<bool>;

    /// Finish a deadline outcome and persist the confirmed-head no-retry marker.
    async fn finish_deadline(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool>;
}

#[cfg(test)]
#[path = "review_job_test.rs"]
pub(crate) mod tests;
