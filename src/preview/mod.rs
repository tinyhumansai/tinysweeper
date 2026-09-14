//! UI previews: what a pull request changes, shown as a user would see it.
//!
//! Always compiled. The reference for the output is a two-column gallery per
//! pull request — for each user flow the change touches, a clip, a screenshot
//! with numbered callouts on the elements that changed, a crop of that
//! region, a title and a one-line caption. Producing that needs a running
//! copy of the application and a browser, and tinysweeper never runs
//! contributor code (`AGENTS.md`), so the work is split in two:
//!
//! - **The brain, here and in `src/server/preview.rs`.** Plans the flows from
//!   the diff, chooses each next browser action from an accessibility
//!   snapshot, names the elements to call out, writes the captions, and
//!   publishes. Holds the model key and the GitHub write token.
//! - **The hands, `actions/ui-preview/` in the reviewed repository's own CI.**
//!   Builds and serves the base and head commits, runs Playwright, executes
//!   what the brain says, draws the callouts, records the clip, uploads to an
//!   object store the operator owns. Holds the bucket credential and a bearer
//!   for the `/preview` routes, and nothing else.
//!
//! Everything the hands send back is untrusted — a same-repository pull
//! request can edit the job that runs them — and is handled like a diff:
//! [`manifest`] validates before anything is published and [`step`] fences a
//! snapshot as data before a model sees it. The server accepts no URL from
//! the wire; every published image URL is composed from the operator's
//! `preview.public_base_url`.
//!
//! See `docs/modules/preview/README.md`.

pub mod apply;
pub mod caption;
pub mod manifest;
pub mod plan;
pub mod render;
pub mod step;
pub mod types;

pub use crate::preview::render::{CHECK_NAME, MARKER};
