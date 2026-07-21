//! Thin wrapper over the GitHub client for PR metadata and diff fetching.

use crate::config::Settings;
use crate::error::Result;
use crate::github::{GitHub, PullRequest, ReviewEvent};

/// Service for fetching PR data from GitHub.
pub(crate) struct GithubService {
    github: GitHub,
}

impl GithubService {
    pub(crate) fn new(settings: &Settings) -> Result<Self> {
        Ok(Self {
            github: GitHub::new(settings)?,
        })
    }

    /// Fetch pull request metadata.
    pub(crate) async fn get_pr_metadata(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest> {
        self.github.get_pr_metadata(owner, repo, number).await
    }

    /// Fetch the raw unified diff for a pull request.
    pub(crate) async fn get_pr_diff(&self, owner: &str, repo: &str, number: u64) -> Result<String> {
        self.github.get_pr_diff(owner, repo, number).await
    }

    /// Post a review comment on a pull request.
    pub(crate) async fn post_review(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        self.github
            .publish_review(owner, repo, number, body, Some(ReviewEvent::Comment))
            .await?;
        Ok(())
    }
}
