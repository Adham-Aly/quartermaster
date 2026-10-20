//! A commit's checks: every check run and every commit status on exactly that SHA, and the required
//! checks of the branch that counts.

use crate::client::{GithubClient, Method, encode_path};
use crate::error::GithubError;
use crate::repo::RepoRef;
use qm_types::ids::CommitSha;
use serde::Deserialize;

/// Results per page (GitHub's maximum).
const PER_PAGE: usize = 100;
/// The most pages read of one list (10,000 entries).
const MAX_PAGES: usize = 100;

/// Everything GitHub reports on one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckSnapshot {
    pub head: CommitSha,
    pub runs: Vec<CheckRun>,
    pub statuses: Vec<CommitStatus>,
    pub required: RequiredChecks,
}

/// A check run's progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    InProgress,
    Completed,
}

/// One check run (`filter=latest`: the newest run of each name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRun {
    pub id: u64,
    pub name: String,
    /// The app's slug, e.g. `github-actions`.
    pub app: Option<String>,
    pub status: RunStatus,
    pub conclusion: Option<String>,
    pub details_url: Option<String>,
    /// The Actions job (a run's ID is its job ID) when the app is GitHub Actions.
    pub actions_job: Option<u64>,
    pub suite: Option<u64>,
    /// Another app's output: title, summary and text.
    pub output: Option<String>,
}

/// One commit status (the newest per context).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitStatus {
    pub context: String,
    /// `success`, `pending`, `failure` or `error`.
    pub state: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

/// The branch's required check names. `complete` is false when either source couldn't be read
/// (unreadable doesn't count as none).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequiredChecks {
    pub names: Vec<String>,
    pub complete: bool,
}

/// Check conclusions that count as passing (GitHub's own meaning of success, neutral and
/// skipped).
pub const PASSING_CONCLUSIONS: &[&str] = &["success", "neutral", "skipped"];

#[derive(Deserialize)]
struct RawApp {
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Deserialize)]
struct RawSuite {
    id: u64,
}

#[derive(Deserialize)]
struct RawOutput {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct RawRun {
    id: u64,
    name: String,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    details_url: Option<String>,
    #[serde(default)]
    app: Option<RawApp>,
    #[serde(default)]
    check_suite: Option<RawSuite>,
    #[serde(default)]
    output: Option<RawOutput>,
}

#[derive(Deserialize)]
struct RawRuns {
    #[serde(default)]
    total_count: usize,
    #[serde(default)]
    check_runs: Vec<RawRun>,
}

#[derive(Deserialize)]
struct RawStatus {
    context: String,
    state: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
}

#[derive(Deserialize)]
struct RawCombined {
    #[serde(default)]
    total_count: usize,
    #[serde(default)]
    statuses: Vec<RawStatus>,
}

impl RawRun {
    fn into_run(self) -> CheckRun {
        let app = self.app.and_then(|a| a.slug);
        let actions = app.as_deref() == Some("github-actions");
        let output = self.output.map(|o| {
            [o.title, o.summary, o.text]
                .into_iter()
                .flatten()
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        });
        CheckRun {
            id: self.id,
            name: self.name,
            status: match self.status.as_str() {
                "completed" => RunStatus::Completed,
                "in_progress" => RunStatus::InProgress,
                _ => RunStatus::Queued,
            },
            conclusion: self.conclusion,
            details_url: self.details_url,
            actions_job: actions.then_some(self.id),
            app,
            suite: self.check_suite.map(|s| s.id),
            output: output.filter(|o| !o.is_empty()),
        }
    }
}

impl GithubClient {
    /// Every check run and commit status on exactly `head` (never by branch or ref), and the
    /// required checks of `base_branch`.
    pub async fn checks(
        &self,
        r: &RepoRef,
        head: &CommitSha,
        base_branch: &str,
    ) -> Result<CheckSnapshot, GithubError> {
        let runs = self.check_runs(r, head).await?;
        let statuses = self.statuses(r, head).await?;
        let required = self.required_checks(r, base_branch).await;
        Ok(CheckSnapshot {
            head: head.clone(),
            runs,
            statuses,
            required,
        })
    }

    async fn check_runs(
        &self,
        r: &RepoRef,
        head: &CommitSha,
    ) -> Result<Vec<CheckRun>, GithubError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = format!(
                "{}/commits/{}/check-runs?filter=latest&per_page={PER_PAGE}&page={page}",
                r.path(),
                head.as_str()
            );
            let raw: RawRuns = self
                .get_json(&path, &format!("the check runs on {}", head.short()))
                .await?;
            let n = raw.check_runs.len();
            out.extend(raw.check_runs.into_iter().map(RawRun::into_run));
            if n < PER_PAGE || out.len() >= raw.total_count {
                break;
            }
        }
        Ok(out)
    }

    async fn statuses(
        &self,
        r: &RepoRef,
        head: &CommitSha,
    ) -> Result<Vec<CommitStatus>, GithubError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = format!(
                "{}/commits/{}/status?per_page={PER_PAGE}&page={page}",
                r.path(),
                head.as_str()
            );
            let raw: RawCombined = self
                .get_json(&path, &format!("the commit statuses on {}", head.short()))
                .await?;
            let n = raw.statuses.len();
            out.extend(raw.statuses.into_iter().map(|s| CommitStatus {
                context: s.context,
                state: s.state,
                description: s.description,
                target_url: s.target_url,
            }));
            if n < PER_PAGE || out.len() >= raw.total_count {
                break;
            }
        }
        Ok(out)
    }

    /// The union of the branch's rulesets and its classic protection. Only an explicit answer
    /// counts as "none required" (rules: 200 with no `required_status_checks`; classic: 404 "Branch
    /// not protected"); any other answer, a bare 404 included, makes the list incomplete.
    pub async fn required_checks(&self, r: &RepoRef, branch: &str) -> RequiredChecks {
        #[derive(Deserialize)]
        struct RawCheck {
            context: String,
        }
        #[derive(Deserialize)]
        struct RawParameters {
            #[serde(default)]
            required_status_checks: Vec<RawCheck>,
        }
        #[derive(Deserialize)]
        struct RawRule {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            parameters: Option<RawParameters>,
        }
        #[derive(Deserialize)]
        struct RawClassic {
            #[serde(default)]
            contexts: Vec<String>,
            #[serde(default)]
            checks: Vec<RawCheck>,
        }
        let mut names: Vec<String> = Vec::new();
        let mut complete = true;
        let branch = encode_path(branch);
        match self
            .send(
                Method::Get,
                &format!("{}/rules/branches/{branch}", r.path()),
                None,
            )
            .await
        {
            Ok(response) if response.ok() => match response.json::<Vec<RawRule>>() {
                Ok(rules) => {
                    for rule in rules {
                        if rule.kind == "required_status_checks"
                            && let Some(p) = rule.parameters
                        {
                            names.extend(p.required_status_checks.into_iter().map(|c| c.context));
                        }
                    }
                }
                Err(_) => complete = false,
            },
            // The rules endpoint answers 200 with an empty list when nothing is required; a 404
            // means the repository can't be seen with this login: unknown.
            _ => complete = false,
        }
        match self
            .send(
                Method::Get,
                &format!(
                    "{}/branches/{branch}/protection/required_status_checks",
                    r.path()
                ),
                None,
            )
            .await
        {
            Ok(response) if response.ok() => match response.json::<RawClassic>() {
                Ok(classic) => {
                    names.extend(classic.contexts);
                    names.extend(classic.checks.into_iter().map(|c| c.context));
                }
                Err(_) => complete = false,
            },
            // Only GitHub's explicit "Branch not protected" means none required; a bare 404 is also
            // what a login without admin rights on the branch gets, so it's unknown.
            Ok(response)
                if response.status == 404
                    && response.message().to_lowercase().contains("not protected") => {}
            _ => complete = false,
        }
        names.sort();
        names.dedup();
        RequiredChecks { names, complete }
    }

    /// Reruns one check run: an Actions job is rerun; another app's suite is re-requested. A
    /// commit status can't be rerun.
    pub async fn rerun(&self, r: &RepoRef, run: &CheckRun) -> Result<(), GithubError> {
        let path = match (run.actions_job, run.suite) {
            (Some(job), _) => format!("{}/actions/jobs/{job}/rerun", r.path()),
            (None, Some(suite)) => format!("{}/check-suites/{suite}/rerequest", r.path()),
            (None, None) => {
                return Err(GithubError::Http {
                    status: 422,
                    message: format!("check {} can't be rerun", run.name),
                });
            }
        };
        let response = self.send(Method::Post, &path, None).await?;
        if response.ok() {
            Ok(())
        } else {
            Err(response.error(&format!("check {}", run.name), self.rate()))
        }
    }

    /// Whether check runs on the default branch can be read with this login (the delivery
    /// probe).
    pub async fn can_read_checks(&self, r: &RepoRef, branch: &str) -> Result<bool, GithubError> {
        let path = format!(
            "{}/commits/{}/check-runs?per_page=1",
            r.path(),
            encode_path(branch)
        );
        let response = self.send(Method::Get, &path, None).await?;
        Ok(response.ok())
    }
}
