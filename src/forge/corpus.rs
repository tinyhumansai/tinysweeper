//! Reading the tree under review through the forge, with no checkout.
//!
//! Always compiled. The server reviews from the forge API alone — there is no
//! clone on that path, which is why `LaneInput::file_contents` is empty there —
//! so this is what makes [`crate::flows::tools`] work in production: one
//! `contents` request per file a sub-agent asks for, at the exact revision under
//! review.
//!
//! # What it deliberately cannot do
//!
//! Search. A forge offers code search, but not a *consistent* one: the index is
//! updated asynchronously and only for the default branch, so a query about a
//! pull request's head returns results for whatever the default branch looked
//! like some minutes ago. A sub-agent told "this appears nowhere else" on that
//! basis would conclude something false about the change under review, and it
//! would be wrong in the direction that produces confident findings.
//!
//! So [`Corpus::search`] returns `Ok(None)` — "I cannot search here" — which
//! [`crate::flows::tools`] passes to the model in exactly those words. A local
//! checkout can search, and a corpus over one should.

use async_trait::async_trait;

use crate::error::Result;
use crate::forge::types::RepoId;
use crate::ports::corpus::{Corpus, Hit};
use crate::ports::forge::ForgeRead;

/// A [`Corpus`] over a forge, pinned to one revision.
///
/// Borrows the forge rather than owning it because that is how a review already
/// holds it. Pinning the revision at construction is the load-bearing part: a
/// sub-agent that could name its own SHA could read a commit from a different
/// branch and reason about code the pull request never touched.
pub struct ForgeCorpus<'a> {
    forge: &'a dyn ForgeRead,
    repo: &'a RepoId,
    sha: &'a str,
}

impl<'a> ForgeCorpus<'a> {
    /// Read `repo` at `sha`, through `forge`.
    pub fn new(forge: &'a dyn ForgeRead, repo: &'a RepoId, sha: &'a str) -> Self {
        Self { forge, repo, sha }
    }
}

#[async_trait]
impl Corpus for ForgeCorpus<'_> {
    async fn read(&self, path: &str) -> Result<Option<String>> {
        self.forge.file_at(self.repo, path, self.sha).await
    }

    async fn search(&self, _pattern: &str, _limit: usize) -> Result<Option<Vec<Hit>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::forge::mock::{MockForge, MockState};

    fn repo() -> RepoId {
        RepoId::parse("acme/widgets").expect("a valid repo id")
    }

    fn forge_serving(sha: &str, path: &str, content: &str) -> MockForge {
        let mut state = MockState::default();
        state.set_file(sha, path, content);
        MockForge::with_state(state)
    }

    #[tokio::test]
    async fn a_file_is_read_at_the_pinned_revision() {
        let forge = forge_serving("head", "src/a.rs", "fn one() {}");
        let repo = repo();
        let corpus = ForgeCorpus::new(&forge, &repo, "head");

        assert_eq!(
            corpus.read("src/a.rs").await.unwrap(),
            Some("fn one() {}".to_string())
        );
    }

    #[tokio::test]
    async fn a_file_at_another_revision_is_not_reachable() {
        // The revision is fixed at construction and there is no argument that
        // could change it. A sub-agent reasoning about a different commit's code
        // is reasoning about a change nobody proposed.
        let forge = forge_serving("other", "src/a.rs", "fn one() {}");
        let repo = repo();
        let corpus = ForgeCorpus::new(&forge, &repo, "head");

        assert_eq!(corpus.read("src/a.rs").await.unwrap(), None);
    }

    #[tokio::test]
    async fn search_reports_that_it_cannot_search_rather_than_finding_nothing() {
        // `Some(vec![])` would let a sub-agent conclude "this appears nowhere
        // else" from an index that never saw this branch.
        let forge = MockForge::default();
        let repo = repo();

        assert_eq!(
            ForgeCorpus::new(&forge, &repo, "head")
                .search("fn", 10)
                .await
                .unwrap(),
            None
        );
    }
}
