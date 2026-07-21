//! Internal services used by the review engine.
//!
//! All three services are `pub(crate)` — visible within the crate for
//! testing but never exposed through the public API.

pub(crate) mod diff_service;
pub(crate) mod github_service;
pub(crate) mod prompt_builder;
pub(crate) mod sanitize;

pub(crate) use diff_service::DiffService;
pub(crate) use github_service::GithubService;
pub(crate) use prompt_builder::PromptBuilder;
pub(crate) use prompt_builder::PromptContext;
pub(crate) use sanitize::sanitize_output;
