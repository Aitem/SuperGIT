//! `supergit --dump [path]`: print what the UI would show, as text. Handy for
//! debugging the data layer and for agents working on SuperGIT itself.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Result;
use sg_agents::SessionIndex;
use sg_git::{Repo, relative_time};

use crate::model;

pub fn run(repo: &Repo) -> Result<()> {
    let index = Arc::new(Mutex::new(SessionIndex::new()));
    let worktrees = model::load_worktrees(repo, &index)?;
    let prs = if sg_github::gh_available() {
        sg_github::list_prs(&repo.root, 100, 20).map_err(|e| e.to_string())
    } else {
        Err("gh not installed".into())
    };

    println!("repo: {} ({})", repo.name(), repo.root.display());
    println!("\nworktrees ({}):", worktrees.len());
    for info in &worktrees {
        let st = match &info.status {
            Ok(s) => format!(
                "{} changed, ↑{} ↓{}{}",
                s.files.len(),
                s.ahead,
                s.behind,
                s.upstream
                    .as_deref()
                    .map(|u| format!(" [{u}]"))
                    .unwrap_or_default()
            ),
            Err(e) => format!("status error: {e}"),
        };
        let agent = info
            .agent_state()
            .map(|a| format!("  claude: {}", a.label()))
            .unwrap_or_default();
        println!(
            "  {}{}  {}  ({}){agent}",
            info.wt.display_name(),
            if info.wt.is_main { " [main]" } else { "" },
            st,
            info.wt.path.display()
        );
        if let Ok(Some(fork)) = sg_git::fork(&info.wt.path) {
            let parent = fork
                .parent
                .map(|p| format!(", forked from {} at {}", p.name, &p.merge_base[..8]))
                .unwrap_or_default();
            println!(
                "      ↑{} ↓{} vs {}{parent}",
                fork.commits, fork.behind, fork.base
            );
        }
        for s in info.sessions.iter().take(5) {
            let age = s
                .modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| relative_time(d.as_secs() as i64))
                .unwrap_or_default();
            println!(
                "      · [{:9}] {age:>4}  {}  ({})",
                s.state.label(),
                s.title,
                s.id
            );
        }
        if info.sessions.len() > 5 {
            println!("      · … {} more sessions", info.sessions.len() - 5);
        }
    }

    match prs {
        Ok(prs) => {
            println!("\npull requests ({}):", prs.len());
            for pr in prs.iter().take(20) {
                println!(
                    "  #{:<5} {:7} {:<40} {} → {}  checks ✓{} ✗{} …{}",
                    pr.number,
                    pr.state.label(),
                    truncate(&pr.title, 40),
                    pr.head_branch,
                    pr.base_branch,
                    pr.checks.passed,
                    pr.checks.failed,
                    pr.checks.pending
                );
            }
        }
        Err(e) => println!("\npull requests: unavailable ({e})"),
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s.to_string()
    }
}
