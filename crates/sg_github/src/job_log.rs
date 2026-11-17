use std::path::Path;

use anyhow::Result;

use crate::{gh, repository_identity};

const MAX_LOG_BYTES: usize = 64 * 1024;

/// The Actions job linked by a GitHub check run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsJob {
    repository: String,
    run: u64,
    job: u64,
}

#[derive(Clone, Debug)]
pub struct JobLog {
    pub text: String,
    pub truncated: bool,
}

impl ActionsJob {
    pub fn from_check_url(url: &str) -> Option<Self> {
        let path = url
            .strip_prefix("https://github.com/")?
            .split(['?', '#'])
            .next()?;
        let mut parts = path.trim_end_matches('/').split('/');
        let owner = parts.next()?;
        let name = parts.next()?;
        let repository = repository_identity(&format!("https://github.com/{owner}/{name}"))?;
        let (run, job) = match parts.collect::<Vec<_>>().as_slice() {
            ["actions", "runs", run, "job", job] | ["runs", run, "jobs", job] => {
                (numeric_id(run)?, numeric_id(job)?)
            }
            _ => return None,
        };
        Some(Self {
            repository,
            run,
            job,
        })
    }

    /// Read only the failed steps from this exact job and retain a display-sized tail.
    pub fn failed_log(&self, path: &Path) -> Result<JobLog> {
        let run = self.run.to_string();
        let job = self.job.to_string();
        let output = gh(
            path,
            [
                "run",
                "view",
                &run,
                "--repo",
                &self.repository,
                "--job",
                &job,
                "--log-failed",
            ],
        )?;
        Ok(JobLog::from_output(&output))
    }
}

fn numeric_id(segment: &str) -> Option<u64> {
    if !segment.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    segment.parse::<u64>().ok().filter(|&id| id > 0)
}

impl JobLog {
    fn from_output(output: &[u8]) -> Self {
        let start = output.len().saturating_sub(MAX_LOG_BYTES);
        let mut tail = &output[start..];
        if start > 0
            && output.get(start - 1) != Some(&b'\n')
            && let Some(line) = tail.iter().position(|&byte| byte == b'\n')
            && line + 1 < tail.len()
        {
            tail = &tail[line + 1..];
        }
        let text = String::from_utf8_lossy(tail);
        let cutoff = text.ceil_char_boundary(text.len().saturating_sub(MAX_LOG_BYTES));
        Self {
            text: text[cutoff..].to_owned(),
            truncated: start > 0 || cutoff > 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_job_from_github_actions_links() {
        let current = ActionsJob::from_check_url(
            "https://github.com/Owner/Repo/actions/runs/101/job/202?pr=42",
        )
        .unwrap();
        let legacy =
            ActionsJob::from_check_url("https://github.com/Owner/Repo/runs/101/jobs/202").unwrap();
        assert_eq!(current, legacy);
        assert_eq!(current.repository, "owner/repo");
        assert_eq!((current.run, current.job), (101, 202));
        for url in [
            "https://example.org/owner/repo/actions/runs/101/job/202",
            "https://github.com/owner/repo/actions/runs/101",
            "https://github.com/owner/repo/actions/runs/101/job/0",
            "https://github.com/owner/repo/actions/runs/+101/job/202",
        ] {
            assert!(ActionsJob::from_check_url(url).is_none(), "{url}");
        }
    }

    #[test]
    fn long_log_keeps_the_end_and_limits_lossy_utf8() {
        let mut output = vec![255; MAX_LOG_BYTES + 80];
        output.extend_from_slice("\nfailed: Русский UTF-8\n".as_bytes());
        let tail = JobLog::from_output(&output);
        assert!(tail.truncated);
        assert_eq!(tail.text, "failed: Русский UTF-8\n");
        assert!(JobLog::from_output(&vec![255; MAX_LOG_BYTES]).text.len() <= MAX_LOG_BYTES);
        assert!(!JobLog::from_output(b"").truncated);
    }
}
