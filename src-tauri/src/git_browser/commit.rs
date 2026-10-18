//! `git_browser_commit` / `git_browser_commit_file`: a commit's metadata and
//! the files it touched against its first parent (the empty tree for a
//! root commit), and one of those files as of the commit with its change.

use super::file::{blob_at, render, FileView, Side};
use super::{body, fail, open, parse_id, status, subject, StatusKind};
use super::{TreeNode, MAX_FILES};
use gix::bstr::ByteSlice as _;
use gix::object::tree::diff::ChangeDetached as Change;
use std::path::Path;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitView {
    pub id: String,
    pub short: String,
    /// First line of the message.
    pub subject: String,
    /// The rest of the message, without the separating blank line.
    pub body: String,
    pub author: String,
    pub email: String,
    /// Author time, unix seconds.
    pub at: i64,
    pub parents: Vec<String>,
    /// Only the files the commit touched.
    pub tree: TreeNode,
    /// Touched files in `tree`.
    pub files: u32,
    /// More than [`MAX_FILES`] touched files: the rest were dropped.
    pub truncated: bool,
}

/// A change's kind; `None` for a directory entry (its files are listed
/// on their own). A copy leaves its source in place, so its destination
/// counts as added.
fn kind(change: &Change) -> Option<StatusKind> {
    if change.entry_mode().is_tree() {
        return None;
    }
    Some(match change {
        Change::Addition { .. } | Change::Rewrite { copy: true, .. } => StatusKind::Added,
        Change::Deletion { .. } => StatusKind::Deleted,
        Change::Modification { .. } => StatusKind::Modified,
        Change::Rewrite { .. } => StatusKind::Renamed,
    })
}

/// The commit's first parent's tree (the empty tree for a root commit) and
/// its own, with the changes between them (rename tracking as configured).
fn diff<'repo>(
    repo: &'repo gix::Repository,
    commit: &gix::Commit<'repo>,
) -> Result<(gix::Tree<'repo>, gix::Tree<'repo>, Vec<Change>), String> {
    let tree = commit.tree().map_err(fail("failed to read commit tree"))?;
    let parent = match commit.parent_ids().next() {
        Some(parent) => repo
            .find_commit(parent)
            .map_err(fail("failed to read parent commit"))?
            .tree()
            .map_err(fail("failed to read parent tree"))?,
        None => repo.empty_tree(),
    };
    let changes = repo
        .diff_tree_to_tree(Some(&parent), Some(&tree), None)
        .map_err(fail("failed to diff commit"))?;
    Ok((parent, tree, changes))
}

/// `git_browser_commit(path, sha)`.
pub fn commit(path: &Path, sha: &str) -> Result<CommitView, String> {
    let (_gate, repo, _) = open(path)?;
    let id = parse_id(sha)?;
    let commit = repo
        .find_commit(id)
        .map_err(fail("failed to read commit"))?;
    let (_, _, changes) = diff(&repo, &commit)?;

    let mut touched = changes.iter().filter_map(|change| {
        Some((
            change.location().to_str_lossy().into_owned(),
            Some(kind(change)?),
        ))
    });
    let files: Vec<_> = touched.by_ref().take(MAX_FILES).collect();
    let truncated = touched.next().is_some();
    let tree = status::fold(files);

    let decoded = commit.decode().map_err(fail("failed to decode commit"))?;
    let author = decoded.author().map_err(fail("failed to decode author"))?;
    Ok(CommitView {
        id: id.to_string(),
        short: id.to_hex_with_len(7).to_string(),
        subject: subject(decoded.message),
        body: body(decoded.message),
        author: author.name.to_str_lossy().into_owned(),
        email: author.email.to_str_lossy().into_owned(),
        at: author.seconds(),
        parents: decoded.parents().map(|p| p.to_string()).collect(),
        files: tree.changed,
        tree,
        truncated,
    })
}

/// `git_browser_commit_file(path, sha, rel_path)`: the blob at `rel_path`
/// in the commit against the first parent's blob at the same path — at the
/// rename source for a renamed file. A path the commit did not touch has
/// no status and renders as all context. `rel_path` names an entry in the
/// commit's tree, so it is checked textually only
/// ([`crate::workspace::validate_path_text`]): today's file system (a
/// directory since replaced by a symlink) has no say in it.
pub fn commit_file(path: &Path, sha: &str, rel_path: &str) -> Result<FileView, String> {
    let (_gate, repo, _) = open(path)?;
    let rel = crate::workspace::validate_path_text(rel_path)?;
    let commit = repo
        .find_commit(parse_id(sha)?)
        .map_err(fail("failed to read commit"))?;
    let (parent, tree, changes) = diff(&repo, &commit)?;
    let change = changes
        .iter()
        .find(|c| !c.entry_mode().is_tree() && c.location() == rel.as_bytes());
    let base = match change {
        Some(Change::Rewrite {
            source_location,
            copy: false,
            ..
        }) => source_location.to_str_lossy(),
        _ => rel.as_str().into(),
    };
    // A copy's destination did not exist in the parent; any blob at its
    // path there would be an unrelated, deleted file.
    let before = match change {
        Some(Change::Addition { .. } | Change::Rewrite { copy: true, .. }) => Side::Absent,
        _ => blob_at(&repo, &parent, &base)?,
    };
    let after = blob_at(&repo, &tree, &rel)?;
    let status = change.and_then(kind);
    Ok(render(rel, status, &before, &after))
}

#[cfg(test)]
mod tests {
    use super::{commit, commit_file};
    use crate::git_browser::file::{FileKind, LineKind};
    use crate::git_browser::{fixture, StatusKind, TreeNode};

    fn files<'a>(node: &'a TreeNode, out: &mut Vec<(&'a str, Option<StatusKind>)>) {
        for child in &node.children {
            if child.dir {
                files(child, out);
            } else {
                out.push((child.path.as_str(), child.status));
            }
        }
    }

    fn touched(node: &TreeNode) -> Vec<(&str, Option<StatusKind>)> {
        let mut out = Vec::new();
        files(node, &mut out);
        out
    }

    /// Root: a.txt, src/keep.rs, src/move.rs, gone.txt. Second commit:
    /// a.txt edited, gone.txt deleted, src/move.rs → lib/moved.rs, new.txt.
    fn history(tag: &str) -> Option<(fixture::Repo, String, String)> {
        let repo = fixture::init(tag)?;
        repo.write("a.txt", "one\ntwo\n");
        repo.write("src/keep.rs", "fn keep() {}\n");
        repo.write("src/move.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        repo.write("gone.txt", "bye\n");
        repo.git(&["add", "."]);
        let root = repo.commit("root", 1_700_000_000);
        repo.write("a.txt", "one\n2\n");
        repo.write("new.txt", "hi\n");
        repo.git(&["rm", "-q", "gone.txt"]);
        std::fs::create_dir_all(repo.path().join("lib")).expect("mkdir");
        repo.git(&["mv", "src/move.rs", "lib/moved.rs"]);
        repo.git(&["add", "."]);
        let second = repo.commit("second\n\nthe body\nline two\n", 1_700_000_100);
        Some((repo, root, second))
    }

    #[test]
    fn commit_lists_files_touched_against_the_first_parent() {
        let Some((repo, root, second)) = history("commit-files") else {
            return;
        };
        let view = commit(repo.path(), &second).expect("commit");
        assert_eq!(view.id, second);
        assert_eq!(view.short, &second[..7]);
        assert_eq!(view.subject, "second");
        assert_eq!(view.body, "the body\nline two");
        assert_eq!(
            (view.author.as_str(), view.email.as_str()),
            ("Test", "test@example.com")
        );
        assert_eq!(view.at, 1_700_000_100);
        assert_eq!(view.parents, [root.as_str()]);
        assert_eq!(
            touched(&view.tree),
            [
                ("lib/moved.rs", Some(StatusKind::Renamed)),
                ("a.txt", Some(StatusKind::Modified)),
                ("gone.txt", Some(StatusKind::Deleted)),
                ("new.txt", Some(StatusKind::Added)),
            ]
        );
        assert_eq!(view.files, 4);
        assert!(!view.truncated);
    }

    #[test]
    fn a_root_commit_lists_every_file_as_added() {
        let Some((repo, root, _)) = history("commit-root") else {
            return;
        };
        let view = commit(repo.path(), &root).expect("commit");
        assert_eq!(view.parents.len(), 0);
        assert_eq!(view.body, "");
        assert_eq!(
            touched(&view.tree),
            [
                ("src/keep.rs", Some(StatusKind::Added)),
                ("src/move.rs", Some(StatusKind::Added)),
                ("a.txt", Some(StatusKind::Added)),
                ("gone.txt", Some(StatusKind::Added)),
            ]
        );
        assert_eq!(view.tree.changed, 4);
    }

    #[test]
    fn commit_file_shows_the_commits_change_or_plain_context() {
        let Some((repo, root, second)) = history("commit-file") else {
            return;
        };
        let a = commit_file(repo.path(), &second, "a.txt").expect("file");
        assert_eq!(a.status, Some(StatusKind::Modified));
        let kinds: Vec<(LineKind, Option<u32>, Option<u32>)> =
            a.lines.iter().map(|l| (l.kind, l.old, l.new)).collect();
        assert_eq!(
            kinds,
            [
                (LineKind::Ctx, Some(1), Some(1)),
                (LineKind::Rem, Some(2), None),
                (LineKind::Add, None, Some(2)),
            ]
        );

        // Untouched by the commit: no status, all context.
        let keep = commit_file(repo.path(), &second, "src/keep.rs").expect("file");
        assert_eq!(keep.status, None);
        assert_eq!(keep.lines.len(), 1);
        assert_eq!(keep.lines[0].kind, LineKind::Ctx);

        // A rename diffs against its source: unchanged content, all context.
        let moved = commit_file(repo.path(), &second, "lib/moved.rs").expect("file");
        assert_eq!(moved.status, Some(StatusKind::Renamed));
        assert_eq!((moved.adds, moved.rems), (0, 0));
        assert_eq!(moved.lines.len(), 3);

        let gone = commit_file(repo.path(), &second, "gone.txt").expect("file");
        assert_eq!(gone.status, Some(StatusKind::Deleted));
        assert_eq!(gone.lines[0].kind, LineKind::Rem);

        // Root commit: against the empty tree.
        let root_a = commit_file(repo.path(), &root, "a.txt").expect("file");
        assert_eq!(root_a.status, Some(StatusKind::Added));
        assert!(root_a.lines.iter().all(|l| l.kind == LineKind::Add));

        // Absent from both sides.
        let none = commit_file(repo.path(), &root, "nowhere.txt").expect("file");
        assert_eq!(none.kind, FileKind::Missing);
    }

    #[test]
    fn bad_ids_and_paths_are_errors() {
        let Some((repo, root, _)) = history("commit-errors") else {
            return;
        };
        assert!(commit(repo.path(), "not-hex").is_err());
        assert!(commit(repo.path(), &"0".repeat(40)).is_err());
        assert!(commit_file(repo.path(), &root, "../a.txt").is_err());
        assert!(commit_file(repo.path(), &root, "").is_err());
        assert!(commit_file(repo.path(), &root, ".git/config").is_err());
        assert!(commit_file(repo.path(), &root, "/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn commit_file_ignores_what_the_path_is_on_disk_today() {
        let Some(repo) = fixture::init("commit-symlinked-dir") else {
            return;
        };
        repo.write("include/a.h", "int a;\n");
        repo.write("elsewhere/a.h", "int other;\n");
        repo.git(&["add", "."]);
        let first = repo.commit("first", 1_700_000_000);
        // `include/` is now a symlink to a sibling directory.
        std::fs::remove_dir_all(repo.path().join("include")).expect("rm include");
        std::os::unix::fs::symlink("elsewhere", repo.path().join("include")).expect("symlink");

        let out = commit_file(repo.path(), &first, "include/a.h").expect("file");
        assert_eq!(out.path, "include/a.h");
        assert_eq!(out.status, Some(StatusKind::Added));
        let texts: Vec<&str> = out.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["int a;"]);
    }
}
