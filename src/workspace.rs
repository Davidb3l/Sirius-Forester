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
        // Canonicalize BOTH sides of the home bound, best-effort: `start`
        // comes from getcwd (symlink-resolved) while the home dir is a raw env
        // string — on a symlinked home (Fedora Silverblue's /home →
        // /var/home, or a user-set HOME) the two spell the same directory
        // differently and a lexical compare would never match, silently
        // unbounding the walk. Canonicalization failure falls back to the
        // raw paths (an unreadable home is not worth failing discovery over).
        //
        // `strip_verbatim` after canonicalizing: on Windows `canonicalize`
        // returns `\?\C:\...`, which would otherwise flow into every child
        // env (`SIRIUS_WORKTREE`, ...) and into user-facing output, and which
        // `git -C` rejects. Stripped ONCE here, at the source of `Workspace::root`.
        let home = home_dir()
            .map(|h| h.canonicalize().unwrap_or(h))
            .map(|h| strip_verbatim(&h));
        let start_canon =
            strip_verbatim(&start.canonicalize().unwrap_or_else(|_| start.to_path_buf()));
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

/// The current user's home directory: `$HOME`, else `%USERPROFILE%`, else the
/// platform lookup. Windows shells other than Git Bash (PowerShell, cmd,
/// services) set only `USERPROFILE`; reading `HOME` alone left the discovery
/// fence unset there, so the walk climbed into `C:\Users\<u>\.hayven` (the
/// GLOBAL state dir) and adopted it as the workspace. Empty values count as unset.
pub fn home_dir() -> Option<PathBuf> {
    home_dir_from(
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
        platform_home_dir,
    )
}

/// Pure core of [`home_dir`] (the env values and the platform fallback are
/// injected so every branch is testable without touching the process env).
pub fn home_dir_from(
    home: Option<std::ffi::OsString>,
    userprofile: Option<std::ffi::OsString>,
    platform: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    [home, userprofile]
        .into_iter()
        .flatten()
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(platform)
        .filter(|p| !p.as_os_str().is_empty())
}

#[allow(deprecated)] // un-deprecated (and fixed on Windows) in Rust 1.87; our MSRV is older
fn platform_home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

/// Drop the Windows extended-length ("verbatim") prefix that
/// `std::fs::canonicalize` adds: `\\?\C:\x` → `C:\x`, `\\?\UNC\srv\share` →
/// `\\srv\share`. Anything else — unix paths, `\\?\Volume{..}\`, device
/// paths, and paths past `MAX_PATH` (where the verbatim form is what makes
/// them usable at all) — is returned untouched.
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let stripped = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        Some(format!(r"\\{rest}"))
    } else {
        s.strip_prefix(r"\\?\")
            .filter(|rest| {
                let b = rest.as_bytes();
                b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
            })
            .map(str::to_string)
    };
    match stripped {
        Some(t) if t.len() < 260 => PathBuf::from(t),
        _ => p.to_path_buf(),
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
        // The bound is the test's own temp dir — see `tempdir` for why a
        // `None` bound here is a test-isolation trap, not a stricter test.
        let ws = Workspace::discover_bounded(&nested, Some(&tmp));
        assert_eq!(ws.root, repo);
        assert_eq!(ws.sirius_dir(), repo.join(".sirius"));
        assert!(ws.ametrite_db.is_some());
        // Nothing in the fixture has a `.hayven/`; with the walk fenced at the
        // fixture root this is genuinely absent rather than "whatever the
        // developer's real home happens to contain".
        assert!(ws.hayven_dir.is_none());
    }

    #[test]
    fn falls_back_to_cwd_without_ametrite() {
        let tmp = tempdir();
        // Start one level BELOW the bound so the walk actually runs (and finds
        // nothing) instead of returning at the first `d == home` check.
        let repo = tmp.join("repo");
        fs::create_dir_all(&repo).unwrap();

        let ws = Workspace::discover_bounded(&repo, Some(&tmp));
        assert_eq!(ws.root, repo);
        assert!(ws.ametrite_db.is_none());
        assert!(ws.hayven_dir.is_none());
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

    // B2: PowerShell / cmd / services set only USERPROFILE. With HOME unset the
    // fence vanished and the walk adopted `C:\Users\<u>\.hayven`.
    #[test]
    fn home_dir_falls_back_to_userprofile_then_platform() {
        use std::ffi::OsString;
        let os = |s: &str| Some(OsString::from(s));
        let never = || -> Option<PathBuf> { panic!("platform lookup must not run") };
        // HOME wins when set.
        assert_eq!(
            home_dir_from(os("/h"), os(r"C:\Users\u"), never),
            Some(PathBuf::from("/h"))
        );
        // HOME unset or empty -> USERPROFILE.
        assert_eq!(
            home_dir_from(None, os(r"C:\Users\u"), never),
            Some(PathBuf::from(r"C:\Users\u"))
        );
        assert_eq!(
            home_dir_from(os(""), os(r"C:\Users\u"), never),
            Some(PathBuf::from(r"C:\Users\u"))
        );
        // Neither -> the platform lookup; nothing at all -> None.
        assert_eq!(
            home_dir_from(None, os(""), || Some(PathBuf::from("/plat"))),
            Some(PathBuf::from("/plat"))
        );
        assert_eq!(home_dir_from(None, None, || None), None);
    }

    // B2 end to end: a home derived from USERPROFILE alone fences the walk, so
    // the global `.hayven` there is never adopted as the workspace.
    #[test]
    fn userprofile_only_home_still_fences_the_walk() {
        let home = tempdir();
        fs::create_dir_all(home.join(".hayven")).unwrap();
        let repo = home.join("Documents/repo");
        fs::create_dir_all(&repo).unwrap();
        let fence = home_dir_from(None, Some(home.clone().into_os_string()), || None);
        let ws = Workspace::discover_bounded(&repo, fence.as_deref());
        assert!(ws.hayven_dir.is_none(), "adopted the global ~/.hayven");
        assert_eq!(ws.root, repo);
    }

    // B3: `\\?\` must not leak out of discovery.
    #[test]
    fn strip_verbatim_unwraps_drive_and_unc_paths_only() {
        let s = |p: &str| strip_verbatim(Path::new(p));
        assert_eq!(s(r"\\?\C:\Users\u\repo"), PathBuf::from(r"C:\Users\u\repo"));
        assert_eq!(
            s(r"\\?\UNC\srv\share\repo"),
            PathBuf::from(r"\\srv\share\repo")
        );
        // Untouched: ordinary, unix, volume-GUID and device paths.
        assert_eq!(s(r"C:\Users\u"), PathBuf::from(r"C:\Users\u"));
        assert_eq!(s("/home/u/repo"), PathBuf::from("/home/u/repo"));
        assert_eq!(
            s(r"\\?\Volume{1234}\x"),
            PathBuf::from(r"\\?\Volume{1234}\x")
        );
        assert_eq!(s(r"\\.\COM1"), PathBuf::from(r"\\.\COM1"));
        // Past MAX_PATH the verbatim form is what makes the path usable.
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(s(&long), PathBuf::from(&long));
        // Idempotent.
        assert_eq!(strip_verbatim(&s(r"\\?\C:\x")), PathBuf::from(r"C:\x"));
    }

    #[cfg(windows)]
    #[test]
    fn discover_returns_no_verbatim_root_on_windows() {
        let tmp = tempdir();
        let repo = tmp.join("repo");
        fs::create_dir_all(repo.join(".ametrite")).unwrap();
        fs::write(repo.join(".ametrite/ametrite.db"), b"x").unwrap();
        let nested = repo.join("src");
        fs::create_dir_all(&nested).unwrap();
        let ws = Workspace::discover(&nested);
        let shown = ws.root.display().to_string();
        assert!(!shown.starts_with(r"\\?\"), "verbatim root leaked: {shown}");
        assert!(!ws.sirius_dir().display().to_string().starts_with(r"\\?\"));
        // git must accept the path exactly as sirius hands it over.
        assert_eq!(
            crate::gitrange::git_path(&ws.root),
            shown,
            "git_path is a no-op on an already-plain root"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_file_named_hayven_is_not_a_workspace() {
        let tmp = tempdir();
        let repo = tmp.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join(".hayven"), b"not a dir").unwrap();

        // Bound at the fixture root: `repo` itself IS still examined (the
        // `d == home` check fires one level up), so the file-vs-dir rejection
        // is what this asserts — not the fence. Unbounded, this test walked out
        // of the temp dir and adopted the developer's real `~/.hayven`, which
        // the hayven daemon creates for multi-repo serving: it failed for every
        // user who had ever started the daemon while the product code was
        // correct all along.
        let ws = Workspace::discover_bounded(&repo, Some(&tmp));
        assert!(
            ws.hayven_dir.is_none(),
            "a FILE named .hayven must never be adopted as a workspace"
        );

        // Positive control, same fence, same starting dir: swap the file for a
        // directory and discovery must find it. Without this, the assertion
        // above could pass for the wrong reason (a fence that hid everything)
        // and the test would silently stop testing `want_dir` at all.
        fs::remove_file(repo.join(".hayven")).unwrap();
        fs::create_dir_all(repo.join(".hayven")).unwrap();
        let ws = Workspace::discover_bounded(&repo, Some(&tmp));
        assert_eq!(ws.hayven_dir, Some(repo.join(".hayven")));
    }

    /// Minimal unique temp dir without pulling in the `tempfile` crate.
    ///
    /// Every test here passes this directory as the `$HOME` bound. That is not
    /// decoration: `walk_up` climbs until it hits the bound, so a `None` bound
    /// lets a test escape `%TEMP%`/`/tmp` and inspect whatever the machine
    /// happens to have above it — and `~/.hayven` and `~/.ametrite` are exactly
    /// what real users accumulate. Fencing at the fixture root makes these
    /// tests depend only on files the test itself created.
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
