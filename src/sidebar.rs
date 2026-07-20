use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::protocol::{SessionInfo, Status};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Scope {
        key: String,
        count: usize,
        expanded: bool,
    },
    Session {
        scope: String,
        session: SessionInfo,
    },
}

impl Row {
    pub fn scope(&self) -> &str {
        match self {
            Self::Scope { key, .. } => key,
            Self::Session { scope, .. } => scope,
        }
    }

    pub fn session(&self) -> Option<&SessionInfo> {
        match self {
            Self::Session { session, .. } => Some(session),
            Self::Scope { .. } => None,
        }
    }
}

pub struct Sidebar {
    sessions: Vec<SessionInfo>,
    rows: Vec<Row>,
    cursor: usize,
    selected_session: Option<String>,
    selected_scope: Option<String>,
    current_scope: String,
    expanded: HashSet<String>,
    accordion: bool,
    repo_cache: HashMap<String, String>,
}

impl Sidebar {
    pub fn new(launch_cwd: &Path) -> Self {
        let (common, root) = derive_scope(launch_cwd);
        let current_scope = common.unwrap_or(root).to_string_lossy().into_owned();
        let expanded = HashSet::from([current_scope.clone()]);
        Self {
            sessions: Vec::new(),
            rows: Vec::new(),
            cursor: 0,
            selected_session: None,
            selected_scope: None,
            current_scope,
            expanded,
            accordion: false,
            repo_cache: HashMap::new(),
        }
    }

    pub fn sessions(&self) -> &[SessionInfo] {
        &self.sessions
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn accordion(&self) -> bool {
        self.accordion
    }

    pub fn current_scope(&self) -> &str {
        &self.current_scope
    }

    pub fn current_row(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    pub fn selected_session(&self) -> Option<&SessionInfo> {
        self.current_row().and_then(Row::session)
    }

    pub fn session_by_id(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|session| session.id == id)
    }

    pub fn update(&mut self, sessions: Vec<SessionInfo>) {
        self.sessions = sessions;
        self.rebuild();
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.sync_identity();
    }

    pub fn move_down(&mut self) {
        self.cursor = (self.cursor + 1).min(self.rows.len().saturating_sub(1));
        self.sync_identity();
    }

    pub fn toggle_current_scope(&mut self) -> bool {
        let Some(Row::Scope { key, .. }) = self.current_row() else {
            return false;
        };
        let key = key.clone();
        let expanded = !self.expanded.contains(&key);
        self.set_expanded(&key, expanded);
        true
    }

    pub fn step_right(&mut self) -> bool {
        match self.current_row() {
            Some(Row::Scope {
                key,
                expanded: false,
                ..
            }) => {
                let key = key.clone();
                self.set_expanded(&key, true);
                false
            }
            Some(Row::Scope { .. })
                if self
                    .rows
                    .get(self.cursor + 1)
                    .is_some_and(|row| matches!(row, Row::Session { .. })) =>
            {
                self.cursor += 1;
                self.sync_identity();
                false
            }
            Some(Row::Session { .. }) => true,
            _ => false,
        }
    }

    pub fn step_left(&mut self) {
        let Some(row) = self.current_row() else {
            return;
        };
        let key = row.scope().to_owned();
        let should_collapse = matches!(row, Row::Session { .. }) || self.expanded.contains(&key);
        if should_collapse {
            self.set_expanded(&key, false);
        }
    }

    pub fn toggle_mode(&mut self) {
        self.accordion = !self.accordion;
        if self.accordion {
            self.expanded.insert(self.current_scope.clone());
        }
        self.rebuild();
    }

    pub fn jump_to_attention(&mut self) -> Option<String> {
        let id = latest_attention(&self.sessions)?.to_owned();
        let cwd = self.session_by_id(&id)?.cwd.clone();
        let scope = self.scope_key(&cwd);
        if !self.accordion && scope != self.current_scope {
            self.accordion = true;
        }
        self.expanded.insert(scope.clone());
        self.selected_scope = Some(scope);
        self.selected_session = Some(id.clone());
        self.rebuild();
        Some(id)
    }

    pub fn select_session(&mut self, id: &str) -> bool {
        let Some(cwd) = self.session_by_id(id).map(|session| session.cwd.clone()) else {
            return false;
        };
        let scope = self.scope_key(&cwd);
        if !self.accordion && scope != self.current_scope {
            self.accordion = true;
        }
        self.expanded.insert(scope.clone());
        self.selected_scope = Some(scope);
        self.selected_session = Some(id.to_owned());
        self.rebuild();
        true
    }

    pub fn select_previous_session(&mut self, id: &str) -> bool {
        let Some(current) = self
            .rows
            .iter()
            .position(|row| row.session().is_some_and(|session| session.id == id))
        else {
            return false;
        };
        let previous = self.rows[..current]
            .iter()
            .rposition(|row| row.session().is_some());
        let fallback = self.rows[current + 1..]
            .iter()
            .position(|row| row.session().is_some())
            .map(|index| current + 1 + index);
        let Some(target) = previous.or(fallback) else {
            return false;
        };
        self.cursor = target;
        self.sync_identity();
        true
    }

    fn set_expanded(&mut self, key: &str, expanded: bool) {
        if expanded {
            self.expanded.insert(key.to_owned());
        } else {
            self.expanded.remove(key);
        }
        self.selected_session = None;
        self.selected_scope = Some(key.to_owned());
        self.rebuild();
    }

    fn scope_key(&mut self, cwd: &str) -> String {
        if let Some(cached) = self.repo_cache.get(cwd) {
            return cached.clone();
        }
        let key = scope_key(cwd);
        self.repo_cache.insert(cwd.to_owned(), key.clone());
        key
    }

    fn rebuild(&mut self) {
        let mut groups: HashMap<String, Vec<SessionInfo>> = HashMap::new();
        let sessions = self.sessions.clone();
        for session in sessions {
            let key = self.scope_key(&session.cwd);
            groups.entry(key).or_default().push(session);
        }
        if self.accordion && !self.current_scope.is_empty() {
            groups.entry(self.current_scope.clone()).or_default();
        }
        let mut keys: Vec<String> = groups.keys().cloned().collect();
        keys.sort_by(|left, right| {
            if left == &self.current_scope {
                std::cmp::Ordering::Less
            } else if right == &self.current_scope {
                std::cmp::Ordering::Greater
            } else {
                scope_display_name(left).cmp(&scope_display_name(right))
            }
        });

        let mut rows = Vec::new();
        if self.accordion {
            for key in keys {
                let sessions = &groups[&key];
                let expanded = self.expanded.contains(&key);
                rows.push(Row::Scope {
                    key: key.clone(),
                    count: sessions.len(),
                    expanded,
                });
                if expanded {
                    rows.extend(sessions.iter().cloned().map(|session| Row::Session {
                        scope: key.clone(),
                        session,
                    }));
                }
            }
        } else if let Some(sessions) = groups.get(&self.current_scope) {
            rows.extend(sessions.iter().cloned().map(|session| Row::Session {
                scope: self.current_scope.clone(),
                session,
            }));
        }

        let restored = self
            .selected_session
            .as_ref()
            .and_then(|id| {
                rows.iter()
                    .position(|row| row.session().is_some_and(|session| &session.id == id))
            })
            .or_else(|| {
                self.selected_scope.as_ref().and_then(|scope| {
                    rows.iter()
                        .position(|row| matches!(row, Row::Scope { key, .. } if key == scope))
                })
            });
        self.rows = rows;
        self.cursor = restored
            .unwrap_or(self.cursor)
            .min(self.rows.len().saturating_sub(1));
        self.sync_identity();
    }

    fn sync_identity(&mut self) {
        let identity = self.current_row().map(|row| match row {
            Row::Scope { key, .. } => (None, key.clone()),
            Row::Session { scope, session } => (Some(session.id.clone()), scope.clone()),
        });
        match identity {
            Some((None, scope)) => {
                self.selected_session = None;
                self.selected_scope = Some(scope);
            }
            Some((session, scope)) => {
                self.selected_session = session;
                self.selected_scope = Some(scope);
            }
            None => {
                self.selected_session = None;
                self.selected_scope = None;
            }
        }
    }
}

pub fn scope_display_name(key: &str) -> String {
    let path = Path::new(key);
    if path.file_name().is_some_and(|name| name == ".git") {
        return path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(unknown)".to_owned());
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            if key.is_empty() {
                "(unknown)".to_owned()
            } else {
                key.to_owned()
            }
        })
}

/// Scope key for a working directory: the git common dir when inside a repo
/// (so a checkout and its linked worktrees collapse into one scope), otherwise
/// the canonical directory. Shared with the daemon so auto-recorded sessions
/// group under the same scope the sidebar renders.
pub fn scope_key(cwd: &str) -> String {
    git_common_dir(Path::new(cwd))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            canonical_or_owned(Path::new(cwd))
                .to_string_lossy()
                .into_owned()
        })
}

pub fn derive_scope(cwd: &Path) -> (Option<PathBuf>, PathBuf) {
    let cwd = canonical_or_owned(cwd);
    match git_common_dir(&cwd) {
        Some(common) => {
            let root = common.parent().unwrap_or(&cwd).to_path_buf();
            (Some(common), root)
        }
        None => (None, cwd),
    }
}

pub fn git_common_dir(dir: &Path) -> Option<PathBuf> {
    let mut current = canonical_or_owned(dir);
    let git_path = loop {
        let candidate = current.join(".git");
        if candidate.exists() {
            break candidate;
        }
        if !current.pop() {
            return None;
        }
    };
    if git_path.is_dir() {
        return Some(canonical_or_owned(&git_path));
    }
    let contents = fs::read_to_string(&git_path).ok()?;
    let git_dir = contents.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = canonical_or_owned(&if Path::new(git_dir).is_absolute() {
        PathBuf::from(git_dir)
    } else {
        git_path.parent()?.join(git_dir)
    });
    if let Ok(contents) = fs::read_to_string(git_dir.join("commondir")) {
        let common = contents.trim();
        return Some(canonical_or_owned(&if Path::new(common).is_absolute() {
            PathBuf::from(common)
        } else {
            git_dir.join(common)
        }));
    }
    Some(git_dir)
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn latest_attention(sessions: &[SessionInfo]) -> Option<&str> {
    sessions
        .iter()
        .filter(|session| session.status == Status::NeedsApproval)
        .max_by_key(|session| session.status_since_unix_ms)
        .or_else(|| {
            sessions
                .iter()
                .filter(|session| session.status == Status::WaitingUser)
                .max_by_key(|session| session.status_since_unix_ms)
        })
        .map(|session| session.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cb-{name}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn repo_layout() -> (PathBuf, PathBuf) {
        let base = temp_dir("scope");
        let main = base.join("main");
        let worktree = base.join("feature");
        let main_git = main.join(".git");
        let worktree_git = main_git.join("worktrees/feature");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&worktree_git).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .unwrap();
        fs::write(worktree_git.join("commondir"), "../..\n").unwrap();
        (main, worktree)
    }

    fn session(id: &str, cwd: &Path, status: Status, since: u64) -> SessionInfo {
        SessionInfo {
            id: id.to_owned(),
            name: String::new(),
            argv: vec!["claude".to_owned()],
            cwd: cwd.to_string_lossy().into_owned(),
            status,
            last_message: String::new(),
            harness_session_id: String::new(),
            exited: false,
            status_since_unix_ms: since,
        }
    }

    #[test]
    fn common_dir_is_shared_by_main_and_linked_worktree() {
        let (main, worktree) = repo_layout();
        let common = git_common_dir(&main).unwrap();
        assert_eq!(git_common_dir(&worktree), Some(common.clone()));
        let (_, root) = derive_scope(&worktree);
        assert_eq!(root, main.canonicalize().unwrap());
    }

    #[test]
    fn accordion_groups_scopes_and_pins_launch_scope_first() {
        let (main, worktree) = repo_layout();
        let other = temp_dir("other");
        let mut sidebar = Sidebar::new(&main);
        sidebar.update(vec![
            session("x", &other, Status::Idle, 0),
            session("a", &main, Status::Idle, 0),
            session("b", &worktree, Status::Idle, 0),
        ]);
        assert_eq!(sidebar.rows.len(), 2);
        sidebar.toggle_mode();
        assert_eq!(sidebar.rows.len(), 4);
        assert!(matches!(&sidebar.rows[0], Row::Scope { count: 2, .. }));
        assert!(matches!(&sidebar.rows[1], Row::Session { session, .. } if session.id == "a"));
        assert!(matches!(&sidebar.rows[2], Row::Session { session, .. } if session.id == "b"));
        assert!(matches!(
            &sidebar.rows[3],
            Row::Scope {
                count: 1,
                expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn collapse_anchors_cursor_to_scope_header() {
        let (main, _) = repo_layout();
        let mut sidebar = Sidebar::new(&main);
        sidebar.update(vec![session("a", &main, Status::Idle, 0)]);
        sidebar.toggle_mode();
        sidebar.move_down();
        sidebar.step_left();
        assert!(matches!(sidebar.current_row(), Some(Row::Scope { .. })));
    }

    #[test]
    fn updates_preserve_logical_session_selection() {
        let cwd = temp_dir("selection");
        let mut sidebar = Sidebar::new(&cwd);
        sidebar.update(vec![
            session("a", &cwd, Status::Idle, 0),
            session("b", &cwd, Status::Working, 1),
        ]);
        sidebar.move_down();
        sidebar.update(vec![
            session("new", &cwd, Status::Idle, 2),
            session("a", &cwd, Status::Idle, 0),
            session("b", &cwd, Status::Working, 1),
        ]);
        assert_eq!(sidebar.selected_session().map(|s| s.id.as_str()), Some("b"));
    }

    #[test]
    fn killing_a_session_selects_the_previous_visible_session() {
        let cwd = temp_dir("kill-selection");
        let mut sidebar = Sidebar::new(&cwd);
        sidebar.update(vec![
            session("a", &cwd, Status::Idle, 0),
            session("b", &cwd, Status::Idle, 0),
            session("c", &cwd, Status::Idle, 0),
        ]);
        assert!(sidebar.select_session("b"));
        assert!(sidebar.select_previous_session("b"));
        assert_eq!(sidebar.selected_session().map(|s| s.id.as_str()), Some("a"));
    }

    #[test]
    fn attention_prefers_latest_approval_then_waiting() {
        let current = temp_dir("current");
        let other = temp_dir("attention");
        let mut sidebar = Sidebar::new(&current);
        sidebar.update(vec![
            session("waiting", &current, Status::WaitingUser, 50),
            session("approval-old", &other, Status::NeedsApproval, 10),
            session("approval-new", &other, Status::NeedsApproval, 20),
        ]);
        assert_eq!(sidebar.jump_to_attention().as_deref(), Some("approval-new"));
        assert!(sidebar.accordion());
        assert_eq!(
            sidebar.selected_session().map(|s| s.id.as_str()),
            Some("approval-new")
        );
    }
}
