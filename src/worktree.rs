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

/// The name of the `sprout` worktree manager binary. Creating a worktree from
/// the picker delegates to it, so the "new worktree" option is gated on it.
pub const SPROUT: &str = "sprout";

/// Whether the `sprout` worktree CLI is installed. `sprout new` creates a git
/// worktree and copy-on-write clones the repo's git-ignored working state into
/// it, which is why cb requires it rather than shelling out to `git worktree`
/// directly.
pub fn sprout_available() -> bool {
    executable_on_path(SPROUT)
}

/// Creates a worktree named `name` via `sprout new` (run inside `repo`, which
/// must be within the target repository) and returns its path, resolved with
/// `sprout path`. `sprout` also CoW-clones the repo's git-ignored files into the
/// new worktree, so it manages both the worktree and its `.sprout` home.
pub fn create(repo: &Path, name: &str) -> io::Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return Err(io::Error::other("worktree name required"));
    }
    let created = Command::new(SPROUT)
        .current_dir(repo)
        .args(["new", name])
        .output()?;
    if !created.status.success() {
        return Err(io::Error::other(sprout_error(
            &created.stderr,
            "sprout new failed",
        )));
    }
    let located = Command::new(SPROUT)
        .current_dir(repo)
        .args(["path", name])
        .output()?;
    if !located.status.success() {
        return Err(io::Error::other(sprout_error(
            &located.stderr,
            "sprout path failed",
        )));
    }
    let path = String::from_utf8_lossy(&located.stdout).trim().to_owned();
    if path.is_empty() {
        return Err(io::Error::other("sprout returned an empty worktree path"));
    }
    Ok(PathBuf::from(path))
}

/// Distils a `sprout` failure into a one-line message, falling back to
/// `fallback` when stderr is empty.
fn sprout_error(stderr: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let text = text.strip_prefix("error: ").unwrap_or(text);
    let first = text.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        fallback.to_owned()
    } else {
        first.to_owned()
    }
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

    #[test]
    fn create_rejects_blank_name_before_shelling_out() {
        assert!(create(Path::new("/"), "   ").is_err());
    }

    #[test]
    fn sprout_error_takes_first_line_and_strips_prefix() {
        assert_eq!(
            sprout_error(b"error: branch 'x' already exists\nhint: ...", "fallback"),
            "branch 'x' already exists"
        );
        assert_eq!(sprout_error(b"   \n", "fallback"), "fallback");
    }
}
