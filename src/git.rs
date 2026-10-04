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
    if let Ok(head) = fs::read_to_string(git_dir.join("HEAD")) {
        if let Some(r) = head.trim().strip_prefix("ref: refs/heads/") {
            info.branch = r.to_owned();
        }
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
            if let Ok(s) = fs::read_to_string(&dot) {
                if let Some(g) = s.lines().find_map(|l| l.strip_prefix("gitdir:")) {
                    return Some((d.to_path_buf(), absolutize(d, g.trim())));
                }
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
        } else if in_origin {
            if let Some((k, v)) = l.split_once('=') {
                if k.trim().eq_ignore_ascii_case("url") {
                    return Some(v.trim());
                }
            }
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
