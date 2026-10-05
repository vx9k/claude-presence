//! Read repository facts straight from `.git` — no `git` process spawned.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GitInfo {
    /// Top-level directory of the work tree.
    pub root: Option<PathBuf>,
    pub branch: String,
    /// `https://github.com/owner/repo` when origin points at GitHub.
    pub github: Option<String>,
}

/// Find the enclosing repository of `cwd` and read branch + origin.
pub fn inspect(cwd: &Path) -> GitInfo {
    let mut info = GitInfo::default();
    let Some((root, git_dir)) = find_git_dir(cwd) else {
        return info;
    };
    info.root = Some(root);
    if let Ok(head) = fs::read_to_string(git_dir.join("HEAD"))
        && let Some(r) = head.trim().strip_prefix("ref: refs/heads/")
    {
        info.branch = r.to_owned();
    }
    // Linked worktrees keep `config` in the common dir.
    let common =
        fs::read_to_string(git_dir.join("commondir")).ok().map(|c| absolutize(&git_dir, c.trim())).unwrap_or(git_dir);
    if let Ok(cfg) = fs::read_to_string(common.join("config")) {
        info.github = origin_url(&cfg).and_then(github_https);
    }
    info
}

fn absolutize(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
}

fn find_git_dir(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let dot = d.join(".git");
        if let Ok(meta) = fs::metadata(&dot) {
            if meta.is_dir() {
                return Some((d.to_path_buf(), dot));
            }
            // Worktrees and submodules: `.git` is a file `gitdir: <path>`.
            if let Ok(s) = fs::read_to_string(&dot)
                && let Some(g) = s.lines().find_map(|l| l.strip_prefix("gitdir:"))
            {
                return Some((d.to_path_buf(), absolutize(d, g.trim())));
            }
        }
        dir = d.parent();
    }
    None
}

/// The `url` of `[remote "origin"]` in a git config file.
fn origin_url(cfg: &str) -> Option<&str> {
    let mut in_origin = false;
    for line in cfg.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_origin = l.replace(' ', "").eq_ignore_ascii_case("[remote\"origin\"]");
        } else if in_origin
            && let Some((k, v)) = l.split_once('=')
            && k.trim().eq_ignore_ascii_case("url")
        {
            return Some(v.trim());
        }
    }
    None
}

fn github_https(url: &str) -> Option<String> {
    let path = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_origin() {
        let cfg = "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = git@github.com:a/b.git\n[remote \"origin\"]\n\turl = git@github.com:vx9k/claude-presence.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n";
        let u = origin_url(cfg).unwrap();
        assert_eq!(github_https(u).unwrap(), "https://github.com/vx9k/claude-presence");
        assert_eq!(github_https("https://github.com/a/b").unwrap(), "https://github.com/a/b");
        assert_eq!(github_https("https://gitlab.com/a/b"), None);
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cp-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn github_url_forms() {
        let ok = "https://github.com/o/r";
        for u in [
            "git@github.com:o/r.git",
            "git@github.com:o/r",
            "git@github.com:o/r/",
            "ssh://git@github.com/o/r.git",
            "https://github.com/o/r.git",
            "https://github.com/o/r.git/",
            "http://github.com/o/r",
        ] {
            assert_eq!(github_https(u).as_deref(), Some(ok), "{u}");
        }
        for u in [
            "",
            "git@github.com:o",
            "git@github.com:o/",
            "git@github.com:/r",
            "git@github.com:o/r/extra",
            "https://github.com/",
            "https://github.com/o",
            "https://gitlab.com/o/r",
            "https://github.com.evil.com/o/r",
            "https://token@github.com/o/r",
            "/srv/git/r.git",
        ] {
            assert_eq!(github_https(u), None, "{u}");
        }
    }

    #[test]
    fn origin_section_parsing() {
        assert_eq!(origin_url(""), None);
        assert_eq!(origin_url("[remote \"origin\"]\n\tfetch = x\n"), None);
        assert_eq!(origin_url("[Remote \"origin\"]\nURL=git@github.com:o/r\n"), Some("git@github.com:o/r"));
        assert_eq!(origin_url("[remote \"origin\"]\r\n\turl = a \r\n"), Some("a"));
        // Only the origin section counts, not lookalikes or later sections.
        assert_eq!(origin_url("[remote \"origin-old\"]\nurl = a\n[remote \"origin\"]\nurl = b\n"), Some("b"));
        assert_eq!(origin_url("[remote \"origin\"]\nfetch = x\n[branch \"main\"]\nurl = c\n"), None);
        assert_eq!(origin_url("# url = z\n[remote \"origin\"]\n; url = y\n\turl = b\n"), Some("b"));
    }

    #[test]
    fn detached_head_and_nested_branch() {
        let d = tmp("detached");
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), "4b825dc642cb6eb9a060e54bf8d69288fbee4904\n").unwrap();
        let i = inspect(&d);
        assert_eq!(i.root.as_deref(), Some(d.as_path()));
        assert_eq!(i.branch, "");
        assert_eq!(i.github, None);
        fs::write(d.join(".git/HEAD"), "ref: refs/heads/feature/x-y\r\n").unwrap();
        assert_eq!(inspect(&d).branch, "feature/x-y");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn linked_worktree() {
        let d = tmp("worktree");
        let main = d.join("main");
        let wt_git = main.join(".git/worktrees/wt");
        fs::create_dir_all(&wt_git).unwrap();
        fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(main.join(".git/config"), "[remote \"origin\"]\n\turl = git@github.com:o/r.git\n").unwrap();
        fs::write(wt_git.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = d.join("wt");
        fs::create_dir_all(wt.join("src")).unwrap();
        // Relative gitdir, resolved against the worktree root.
        fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        let i = inspect(&wt.join("src"));
        assert_eq!(i.root.as_deref(), Some(wt.as_path()));
        assert_eq!(i.branch, "topic");
        assert_eq!(i.github.as_deref(), Some("https://github.com/o/r"));
        // Absolute gitdir works too.
        fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        assert_eq!(inspect(&wt).branch, "topic");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn bogus_dot_git_file_is_skipped() {
        let d = tmp("bogus");
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), "ref: refs/heads/outer\n").unwrap();
        let inner = d.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join(".git"), "not a gitdir pointer\n").unwrap();
        let i = inspect(&inner);
        assert_eq!(i.root.as_deref(), Some(d.as_path()));
        assert_eq!(i.branch, "outer");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn finds_repo() {
        let d = std::env::temp_dir().join(format!("cp-git-{}", std::process::id()));
        let sub = d.join("src/deep");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(d.join(".git/config"), "[remote \"origin\"]\n url = https://github.com/o/r.git\n").unwrap();
        let i = inspect(&sub);
        assert_eq!(i.root.as_deref(), Some(d.as_path()));
        assert_eq!(i.branch, "main");
        assert_eq!(i.github.as_deref(), Some("https://github.com/o/r"));
        let _ = fs::remove_dir_all(&d);
    }
}
