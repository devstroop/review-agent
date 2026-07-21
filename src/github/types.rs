use serde::{Deserialize, Serialize};

/// A GitHub pull request as returned by the API.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    pub state: String,
    #[serde(default)]
    pub user: Option<GitHubUser>,
    pub head: PrBranch,
    pub base: PrBranch,
    pub merged: Option<bool>,
    pub draft: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    #[serde(default)]
    pub r#type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrBranch {
    pub label: String,
    pub r#ref: String,
    pub sha: String,
    pub repo: Option<RepoInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoInfo {
    pub full_name: Option<String>,
    pub language: Option<String>,
    pub default_branch: Option<String>,
}

/// A file changed in a pull request, as returned by the PR Files API.
#[derive(Debug, Clone, Deserialize)]
pub struct PrFile {
    pub filename: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub changes: u64,
    #[serde(default)]
    pub patch: Option<String>,
    #[serde(default)]
    pub contents_url: Option<String>,
    pub raw_url: Option<String>,
}

/// A review to post on a pull request.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewBody {
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
}

/// The GitHub API response after posting a review.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewResponse {
    pub id: u64,
    pub state: String,
    pub html_url: Option<String>,
}

/// An issue (or PR) comment.
#[derive(Debug, Clone, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub user: Option<GitHubUser>,
}

/// Language breakdown for a repository.
pub type LanguageBreakdown = std::collections::HashMap<String, u64>;
