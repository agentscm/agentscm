//! The one client the sync talks through: GitHub or GitLab by the connection's
//! provider. Calls take the connected repository; GitLab is called by its
//! numeric project id so a rename never points at another project.
use serde_json::Value;

use super::connection::ConnectedRepository as Repo;
use super::github::{self, Branch, Gh, Paged, RepoInfo};
use super::gitlab;

#[derive(Clone, Copy)]
pub enum ForgeApi<'a> {
    Github(&'a github::Api),
    Gitlab(&'a gitlab::Api),
}

impl ForgeApi<'_> {
    /// By name, to connect a repository.
    pub async fn repo(&self, token: &str, full_name: &str) -> Gh<Option<RepoInfo>> {
        match self {
            Self::Github(api) => api.repo(token, full_name).await,
            Self::Gitlab(api) => api.repo(token, full_name).await,
        }
    }

    /// Open pull requests; `None` when `etag` still matches (GitHub only).
    pub async fn open_pulls(
        &self,
        token: &str,
        repo: &Repo,
        etag: Option<&str>,
    ) -> Gh<Option<Paged>> {
        match self {
            Self::Github(api) => api.open_pulls(token, &repo.full_name, etag).await,
            Self::Gitlab(api) => api.open_pulls(token, repo.repository_id).await.map(Some),
        }
    }

    pub async fn pull(&self, token: &str, repo: &Repo, number: i64) -> Gh<Option<Value>> {
        match self {
            Self::Github(api) => api.pull(token, &repo.full_name, number).await,
            Self::Gitlab(api) => api.pull(token, repo.repository_id, number).await,
        }
    }

    pub async fn pull_files(
        &self,
        token: &str,
        repo: &Repo,
        number: i64,
    ) -> Gh<(Vec<String>, bool)> {
        match self {
            Self::Github(api) => api.pull_files(token, &repo.full_name, number).await,
            Self::Gitlab(api) => api.pull_files(token, repo.repository_id, number).await,
        }
    }

    pub async fn branch(&self, token: &str, repo: &Repo, name: &str) -> Gh<Option<Branch>> {
        match self {
            Self::Github(api) => api.branch(token, &repo.full_name, name).await,
            Self::Gitlab(api) => api.branch(token, repo.repository_id, name).await,
        }
    }

    pub async fn recent_branches(
        &self,
        token: &str,
        repo: &Repo,
        since: i64,
    ) -> Gh<(Vec<Branch>, bool)> {
        match self {
            Self::Github(api) => api.recent_branches(token, &repo.full_name, since).await,
            Self::Gitlab(api) => api.recent_branches(token, repo.repository_id, since).await,
        }
    }

    pub async fn comments(
        &self,
        token: &str,
        repo: &Repo,
        number: i64,
    ) -> Gh<Vec<(i64, String, String)>> {
        match self {
            Self::Github(api) => api.comments(token, &repo.full_name, number).await,
            Self::Gitlab(api) => api.comments(token, repo.repository_id, number).await,
        }
    }

    pub async fn create_comment(
        &self,
        token: &str,
        repo: &Repo,
        number: i64,
        body: &str,
    ) -> Gh<i64> {
        match self {
            Self::Github(api) => {
                api.create_comment(token, &repo.full_name, number, body)
                    .await
            }
            Self::Gitlab(api) => {
                api.create_comment(token, repo.repository_id, number, body)
                    .await
            }
        }
    }

    /// False when the comment no longer exists. GitLab needs the merge request's number too.
    pub async fn edit_comment(
        &self,
        token: &str,
        repo: &Repo,
        number: i64,
        comment: i64,
        body: &str,
    ) -> Gh<bool> {
        match self {
            Self::Github(api) => {
                api.edit_comment(token, &repo.full_name, comment, body)
                    .await
            }
            Self::Gitlab(api) => {
                api.edit_comment(token, repo.repository_id, number, comment, body)
                    .await
            }
        }
    }

    pub async fn create_status(
        &self,
        token: &str,
        repo: &Repo,
        sha: &str,
        description: &str,
    ) -> Gh<()> {
        match self {
            Self::Github(api) => {
                api.create_status(token, &repo.full_name, sha, description)
                    .await
            }
            Self::Gitlab(api) => {
                api.create_status(token, repo.repository_id, sha, description)
                    .await
            }
        }
    }

    /// The forge's explicit default branch; an error when it is unknown.
    pub async fn default_branch(&self, token: &str, repo: &Repo) -> Gh<String> {
        match self {
            Self::Github(api) => api.default_branch(token, &repo.full_name).await,
            Self::Gitlab(api) => api.default_branch(token, repo.repository_id).await,
        }
    }

    /// The commit a branch points at; None when it does not exist.
    pub async fn ref_sha(&self, token: &str, repo: &Repo, branch: &str) -> Gh<Option<String>> {
        match self {
            Self::Github(api) => api.ref_sha(token, &repo.full_name, branch).await,
            Self::Gitlab(api) => api.ref_sha(token, repo.repository_id, branch).await,
        }
    }

    /// Open pull requests from `branch` in the repository itself (no forks). An error, never a
    /// partial list, past the page cap. GitHub lists by head only (`base` is left to the caller);
    /// GitLab filters by both.
    pub async fn find_open_pulls(
        &self,
        token: &str,
        repo: &Repo,
        branch: &str,
        base: &str,
    ) -> Gh<Vec<Value>> {
        match self {
            Self::Github(api) => {
                let owner = repo.full_name.split('/').next().unwrap_or_default();
                api.find_open_pulls(token, &repo.full_name, owner, branch)
                    .await
            }
            Self::Gitlab(api) => {
                api.find_open_pulls(token, repo.repository_id, branch, base)
                    .await
            }
        }
    }

    /// `Err(status)` is a definitive refusal; any other failure leaves the outcome uncertain.
    pub async fn create_candidate_pull(
        &self,
        token: &str,
        repo: &Repo,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Gh<std::result::Result<Value, u16>> {
        match self {
            Self::Github(api) => {
                api.create_candidate_pull(token, &repo.full_name, title, body, head, base)
                    .await
            }
            Self::Gitlab(api) => {
                api.create_candidate_pull(token, repo.repository_id, title, body, head, base)
                    .await
            }
        }
    }
}
