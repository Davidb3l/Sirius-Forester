//! Workspace discovery.
//!
//! Sirius sits beside `.ametrite/` and `.hayven/`. We walk up from the cwd to
//! find them, and place `.sirius/` next to `.ametrite/` when it exists (else in
//! the cwd). Sirius does not mint its own registry in v1 (PRD §3).

use std::path::{Path, PathBuf};

/// Locations Sirius cares about, resolved once.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Directory that will hold (or holds) `.sirius/`.
    pub root: PathBuf,
    /// `.ametrite/ametrite.db` if found by walking up.
    pub ametrite_db: Option<PathBuf>,
    /// `.hayven/` directory if found by walking up.
    pub hayven_dir: Option<PathBuf>,
}

impl Workspace {
    /// Discover from a starting directory (usually the cwd).
    pub fn discover(start: &Path) -> Workspace {
        // Canonicalize BOTH sides of the $HOME bound, best-effort: `start`
        // comes from getcwd (symlink-resolved) while $HOME is a raw env
        // string — on a symlinked home (Fedora Silverblue's /home →
        // /var/home, or a user-set HOME) the two spell the same directory
        // differently and a lexical compare would never match, silently
        // unbounding the walk. Canonicalization failure falls back to the
        // raw paths (an unreadable home is not worth failing discovery over).
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.canonicalize().unwrap_or(h));
        let start_canon = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
        Self::discover_bounded(&start_canon, home.as_deref())
    }

    /// Like `discover`, with the walk boundary injected (test seam).
    ///
    /// The walk NEVER examines `$HOME` or anything above it. Both parents keep
    /// their GLOBAL state at `~/.ametrite` and `~/.hayven` — those are tool
    /// installations, not workspace markers. Before this bound existed, any
    /// repo under `$HOME` that lacked its own `.hayven/` silently "discovered"
    /// the home one: doctor reported `.hayven/ present` for a directory the
    /// repo does not have, and the SIRF-10 "daemon serves THIS workspace" gate
    /// (`hayven_dir.is_some()`) was always true, letting a wrong-project or
    /// orphan daemon read as healthy (observed 2026-08-05).
    pub fn discover_bounded(start: &Path, home: Option<&Path>) -> Workspace {
        let ametrite_db = walk_up(start, ".ametrite/ametrite.db", false, home);
        let hayven_dir = walk_up(start, ".hayven", true, home);
        // Root for `.sirius/` = the dir containing `.ametrite/` if we found it,
        // otherwise the starting dir.
        let root = ametrite_db
            .as_ref()
            .and_then(|p| p.parent()) // .ametrite/
            .and_then(|p| p.parent()) // repo root
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| start.to_path_buf());
        Workspace {
            root,
            ametrite_db,
            hayven_dir,
        }
    }

    pub fn sirius_dir(&self) -> PathBuf {
        self.root.join(".sirius")
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.sirius_dir().join("sirius.db")
    }

    pub fn config_path(&self) -> PathBuf {
        self.sirius_dir().join("config.json")
    }
}

/// Walk up from `start`, returning the first `start/.../<rel>` of the right
/// kind (`want_dir` selects directory vs file — a stray FILE named `.hayven`
/// is not a workspace). The walk stops BEFORE examining `home` or anything
/// above it; see `discover_bounded` for why `$HOME` is out of bounds.
fn walk_up(start: &Path, rel: &str, want_dir: bool, home: Option<&Path>) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if home.is_some_and(|h| d == h) {
            return None; // reached $HOME: global tool state, never a workspace
        }
        let candidate = d.join(rel);
        let kind_ok = if want_dir {
            candidate.is_dir()
        } else {
            candidate.is_file()
        };
        if kind_ok {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn discovers_ametrite_and_roots_sirius_beside_it() {
        let tmp = tempdir();
        let repo = tmp.join("repo");
        fs::create_dir_all(repo.join(".ametrite")).unwrap();
        fs::write(repo.join(".ametrite/ametrite.db"), b"x").unwrap();
        let nested = repo.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        // discover_bounded: these tests pin discovery MECHANICS; `discover`
        // itself additionally canonicalizes (macOS /var → /private/var), which
        // would make raw-path equality here compare different spellings.
        let ws = Workspace::discover_bounded(&nested, None);
        assert_eq!(ws.root, repo);
        assert_eq!(ws.sirius_dir(), repo.join(".sirius"));
        assert!(ws.ametrite_db.is_some());
    }

    #[test]
    fn falls_back_to_cwd_without_ametrite() {
        let tmp = tempdir();
        let ws = Workspace::discover_bounded(&tmp, None);
        assert_eq!(ws.root, tmp);
        assert!(ws.ametrite_db.is_none());
    }

    // The 2026-08-05 defect: ~/.ametrite and ~/.hayven are the parents' GLOBAL
    // state dirs. A repo under $HOME without its own markers must NOT discover
    // them — that fabricated ".hayven/ present" and defeated the SIRF-10
    // serving gate.
    #[test]
    fn never_discovers_global_state_in_home() {
        let home = tempdir();
        fs::create_dir_all(home.join(".ametrite")).unwrap();
        fs::write(home.join(".ametrite/ametrite.db"), b"x").unwrap();
        fs::create_dir_all(home.join(".hayven")).unwrap();
        let repo = home.join("Documents/code/repo");
        fs::create_dir_all(&repo).unwrap();

        let ws = Workspace::discover_bounded(&repo, Some(&home));
        assert!(ws.ametrite_db.is_none(), "must not adopt ~/.ametrite");
        assert!(ws.hayven_dir.is_none(), "must not adopt ~/.hayven");
        assert_eq!(ws.root, repo, ".sirius must root at cwd, never $HOME");
    }

    #[test]
    fn repo_local_markers_under_home_still_discovered() {
        let home = tempdir();
        fs::create_dir_all(home.join(".hayven")).unwrap(); // global decoy
        let repo = home.join("code/repo");
        fs::create_dir_all(repo.join(".ametrite")).unwrap();
        fs::write(repo.join(".ametrite/ametrite.db"), b"x").unwrap();
        fs::create_dir_all(repo.join(".hayven")).unwrap();
        let nested = repo.join("src/deep");
        fs::create_dir_all(&nested).unwrap();

        let ws = Workspace::discover_bounded(&nested, Some(&home));
        assert_eq!(ws.hayven_dir, Some(repo.join(".hayven")));
        assert_eq!(ws.root, repo);
    }

    #[test]
    fn a_file_named_hayven_is_not_a_workspace() {
        let tmp = tempdir();
        let repo = tmp.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join(".hayven"), b"not a dir").unwrap();
        let ws = Workspace::discover_bounded(&repo, None);
        assert!(ws.hayven_dir.is_none());
    }

    /// Minimal unique temp dir without pulling in the `tempfile` crate.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("sirius-ws-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
