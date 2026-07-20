use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub detached: bool,
    pub bare: bool,
    pub main: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    pub binary: &'static str,
    pub label: &'static str,
}

pub const CANDIDATE_AGENTS: &[Agent] = &[
    Agent {
        binary: "claude",
        label: "claude code",
    },
    Agent {
        binary: "codex",
        label: "codex",
    },
    Agent {
        binary: "opencode",
        label: "opencode",
    },
];

pub fn available_agents() -> Vec<Agent> {
    CANDIDATE_AGENTS
        .iter()
        .copied()
        .filter(|agent| executable_on_path(agent.binary))
        .collect()
}

pub fn list(dir: &Path) -> io::Result<Vec<Worktree>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["worktree", "list", "--porcelain"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("no git worktrees here"));
    }
    Ok(parse(&output.stdout))
}

pub fn parse(output: &[u8]) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in String::from_utf8_lossy(output).lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            current = Some(Worktree {
                path: PathBuf::from(path),
                branch: String::new(),
                detached: false,
                bare: false,
                main: false,
            });
        } else if let Some(worktree) = current.as_mut() {
            if let Some(branch) = line.strip_prefix("branch ") {
                worktree.branch = branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_owned();
            } else if line == "detached" {
                worktree.detached = true;
            } else if line == "bare" {
                worktree.bare = true;
            }
        }
    }
    if let Some(worktree) = current {
        worktrees.push(worktree);
    }
    if let Some(main) = worktrees.first_mut() {
        main.main = true;
    }
    worktrees
}

pub fn tag(worktree: &Worktree) -> String {
    if worktree.main {
        "(main)".to_owned()
    } else if worktree.bare {
        "(bare)".to_owned()
    } else if worktree.detached {
        "(detached)".to_owned()
    } else if worktree.branch.is_empty() {
        String::new()
    } else {
        format!("⎇ {}", worktree.branch)
    }
}

fn executable_on_path(binary: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            let candidate = path.join(binary);
            candidate.is_file()
                && candidate
                    .metadata()
                    .map(|metadata| {
                        use std::os::unix::fs::PermissionsExt;
                        metadata.permissions().mode() & 0o111 != 0
                    })
                    .unwrap_or(false)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_records_and_tags_main_checkout() {
        let worktrees = parse(
            b"worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
              worktree /repo-feature\nHEAD def\nbranch refs/heads/feature/x\n\n\
              worktree /repo-detached\nHEAD 123\ndetached\n",
        );
        assert_eq!(worktrees.len(), 3);
        assert!(worktrees[0].main);
        assert_eq!(worktrees[0].branch, "main");
        assert_eq!(tag(&worktrees[0]), "(main)");
        assert_eq!(tag(&worktrees[1]), "⎇ feature/x");
        assert_eq!(tag(&worktrees[2]), "(detached)");
    }
}
