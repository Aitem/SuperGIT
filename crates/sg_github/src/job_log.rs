use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;

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

    /// Rerun a terminal failed job in the current attempt. GitHub CLI takes
    /// a database ID, so verify it against the run before dispatching.
    pub fn rerun(&self, path: &Path) -> Result<()> {
        let run = self.run.to_string();
        let snapshot = gh(
            path,
            [
                "run",
                "view",
                &run,
                "--repo",
                &self.repository,
                "--json",
                "status,jobs",
            ],
        )?;
        let job = rerunnable_job(&snapshot, self.job)?.to_string();
        gh(
            path,
            [
                "run",
                "rerun",
                &run,
                "--repo",
                &self.repository,
                "--job",
                &job,
            ],
        )?;
        Ok(())
    }

    /// Read only the failed steps from this exact job and retain a display-sized tail.
    pub fn failed_log(&self, path: &Path) -> Result<JobLog> {
        let run = self.run.to_string();
        let job = self.job.to_string();
        let mut child = crate::gh_command(
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
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
        let stdout = child.stdout.take().context("gh stdout is unavailable")?;
        std::thread::scope(|scope| {
            let reader = scope.spawn(move || JobLog::read_tail(stdout));
            let output = child.wait_with_output()?;
            ensure!(
                output.status.success(),
                "gh run view: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            reader.join().expect("log reader panicked")
        })
    }
}

fn rerunnable_job(output: &[u8], expected: u64) -> Result<u64> {
    #[derive(Deserialize)]
    struct Run {
        status: String,
        jobs: Vec<Job>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Job {
        database_id: u64,
        status: String,
        conclusion: String,
    }
    let run: Run =
        serde_json::from_slice(output).context("Invalid Actions job list from GitHub")?;
    ensure!(
        run.status == "completed",
        "GitHub is still running this workflow"
    );
    let job = run
        .jobs
        .into_iter()
        .find(|job| job.database_id == expected)
        .context("The selected job is no longer in the current workflow attempt")?;
    ensure!(
        job.status == "completed"
            && matches!(
                job.conclusion.as_str(),
                "failure"
                    | "cancelled"
                    | "timed_out"
                    | "startup_failure"
                    | "action_required"
                    | "stale"
            ),
        "Only a completed failed job can be rerun"
    );
    Ok(job.database_id)
}

fn numeric_id(segment: &str) -> Option<u64> {
    if !segment.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    segment.parse::<u64>().ok().filter(|&id| id > 0)
}

impl JobLog {
    fn read_tail(mut reader: impl Read) -> Result<Self> {
        // One preceding byte preserves the partial-line boundary on truncation.
        let capacity = MAX_LOG_BYTES + 1;
        let mut tail = VecDeque::with_capacity(capacity);
        let mut buf = [0; 8192];
        loop {
            let n = match reader.read(&mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n == 0 {
                break;
            }
            tail.drain(..(tail.len() + n).saturating_sub(capacity));
            tail.extend(&buf[..n]);
        }
        Ok(Self::from_output(tail.make_contiguous()))
    }

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
    fn rerun_uses_a_failed_database_id_from_the_completed_attempt() {
        let run = br#"{"status":"completed","jobs":[{"databaseId":1000202,"status":"completed","conclusion":"cancelled"},{"databaseId":1000203,"status":"completed","conclusion":"success"}]}"#;
        assert_eq!(rerunnable_job(run, 1000202).unwrap(), 1000202);
        assert!(rerunnable_job(run, 1000203).is_err());
        assert!(rerunnable_job(run, 2).is_err());
        assert!(rerunnable_job(br#"{"status":"in_progress","jobs":[]}"#, 1000202).is_err());
    }

    #[test]
    fn long_log_keeps_the_end_and_limits_lossy_utf8() {
        let mut output = vec![255; MAX_LOG_BYTES + 80];
        output.extend_from_slice("\nfailed: Русский UTF-8\n".as_bytes());
        let tail = JobLog::read_tail(output.as_slice()).unwrap();
        assert!(tail.truncated);
        assert_eq!(tail.text, "failed: Русский UTF-8\n");
        assert!(JobLog::from_output(&vec![255; MAX_LOG_BYTES]).text.len() <= MAX_LOG_BYTES);
        assert!(!JobLog::from_output(b"").truncated);
    }
}
