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
        let ametrite_db = walk_up(start, ".ametrite/ametrite.db");
        let hayven_dir = walk_up(start, ".hayven");
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

/// Walk up from `start`, returning the first existing `start/.../<rel>`.
fn walk_up(start: &Path, rel: &str) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(rel);
        if candidate.exists() {
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

        let ws = Workspace::discover(&nested);
        assert_eq!(ws.root, repo);
        assert_eq!(ws.sirius_dir(), repo.join(".sirius"));
        assert!(ws.ametrite_db.is_some());
    }

    #[test]
    fn falls_back_to_cwd_without_ametrite() {
        let tmp = tempdir();
        let ws = Workspace::discover(&tmp);
        assert_eq!(ws.root, tmp);
        assert!(ws.ametrite_db.is_none());
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
