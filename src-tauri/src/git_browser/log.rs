//! `git_browser_log`: the commit graph of everything reachable from `HEAD`
//! and every ref (`git log --all --date-order`), decorated with refs and
//! laid out onto lanes; plus `HEAD`'s ahead/behind against its upstream.

use super::graph::{self, Edge};
use super::{fail, open, subject};
use gix::bstr::ByteSlice as _;
use gix::ObjectId;
use std::collections::HashMap;
use std::path::Path;

/// Commits listed before the history is cut off and marked `truncated`.
const MAX_COMMITS: usize = 500;

/// Id of the synthetic working-tree row on top.
const WORKTREE_ROW: &str = "@";

/// Declaration order is display order on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    /// The branch `HEAD` points at (named after the branch), or `HEAD`
    /// itself when detached.
    Head,
    Branch,
    /// Remote-tracking branch, named with its remote (`origin/main`).
    Remote,
    Tag,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct Ref {
    pub name: String,
    pub kind: RefKind,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRow {
    /// Full id; `"@"` for the working-tree row.
    pub id: String,
    /// 7-char abbreviation; `""` for `"@"`.
    pub short: String,
    pub parents: Vec<String>,
    /// First line of the message; `""` for `"@"`.
    pub subject: String,
    pub author: String,
    /// Author time, unix seconds; `0` for `"@"`.
    pub at: i64,
    pub refs: Vec<Ref>,
    pub lane: u32,
    pub edges: Vec<Edge>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogView {
    pub rows: Vec<LogRow>,
    /// Lanes used, at most [`graph::MAX_LANES`].
    pub width: u32,
    /// More than [`MAX_COMMITS`] commits: the oldest were dropped.
    pub truncated: bool,
}

/// `git_browser_log(path)`.
pub fn log(path: &Path) -> Result<LogView, String> {
    let (mut repo, _) = open(path)?;
    // Each row's commit is read again after the walk decoded it; the cache
    // (off by default) makes that second read a hit.
    repo.object_cache_size_if_unset(4 * 1024 * 1024);
    let head = repo.head_id().ok().map(gix::Id::detach);
    let (tips, mut refs) = decorations(&repo, head)?;

    let walk = gix::traverse::commit::topo::Builder::from_iters(
        &repo.objects,
        head.into_iter().chain(tips),
        None::<Vec<ObjectId>>,
    )
    .sorting(gix::traverse::commit::topo::Sorting::DateOrder)
    // Generation numbers let the in-degree pass stop early instead of
    // walking all history before the first row; absent or disabled, the
    // walk reads every commit as before.
    .with_commit_graph(repo.commit_graph_if_enabled().ok().flatten())
    .build()
    .map_err(fail("failed to walk history"))?;

    let mut rows = Vec::new();
    if let Some(head) = head {
        rows.push(LogRow {
            id: WORKTREE_ROW.to_string(),
            short: String::new(),
            parents: vec![head.to_string()],
            subject: String::new(),
            author: String::new(),
            at: 0,
            refs: Vec::new(),
            lane: 0,
            edges: Vec::new(),
        });
    }
    let mut truncated = false;
    for info in walk {
        let info = info.map_err(fail("failed to walk history"))?;
        if rows.len() == MAX_COMMITS + usize::from(head.is_some()) {
            truncated = true;
            break;
        }
        let commit = repo
            .find_commit(info.id)
            .map_err(fail("failed to read commit"))?;
        let decoded = commit.decode().map_err(fail("failed to decode commit"))?;
        let author = decoded.author().map_err(fail("failed to decode author"))?;
        rows.push(LogRow {
            id: info.id.to_string(),
            short: info.id.to_hex_with_len(7).to_string(),
            parents: info.parent_ids.iter().map(ToString::to_string).collect(),
            subject: subject(decoded.message),
            author: author.name.to_str_lossy().into_owned(),
            at: author.seconds(),
            refs: refs.remove(&info.id).unwrap_or_default(),
            lane: 0,
            edges: Vec::new(),
        });
    }

    let layout = graph::layout(
        rows.iter()
            .map(|row| (row.id.as_str(), row.parents.iter().map(String::as_str))),
    );
    for (row, placed) in rows.iter_mut().zip(layout.rows) {
        row.lane = placed.lane;
        row.edges = placed.edges;
    }
    Ok(LogView {
        rows,
        width: layout.width,
        truncated,
    })
}

/// Per commit, the refs shown on its row.
type Decorations = HashMap<ObjectId, Vec<Ref>>;

/// Every ref pointing at a commit: the commit ids (walk tips) and, per
/// commit, its branch/remote/tag decorations — `head` first, then
/// branches, remotes and tags, each by name. Other refs (stash, notes)
/// still count as tips, as `git log --all` walks them.
fn decorations(
    repo: &gix::Repository,
    head: Option<ObjectId>,
) -> Result<(Vec<ObjectId>, Decorations), String> {
    use gix::refs::Category;

    let head_name = repo.head_name().map_err(fail("failed to read HEAD"))?;
    let mut tips = Vec::new();
    let mut refs = Decorations::new();
    if let (Some(head), None) = (head, &head_name) {
        refs.entry(head).or_default().push(Ref {
            name: "HEAD".to_string(),
            kind: RefKind::Head,
        });
    }
    let platform = repo.references().map_err(fail("failed to read refs"))?;
    let all = platform.all().map_err(fail("failed to read refs"))?;
    // A broken or unpeelable ref is skipped, as `git log --all` ignores it.
    // A symbolic ref (`refs/remotes/origin/HEAD`) is skipped too: peeling
    // would rename it to its target and badge that branch twice.
    for mut reference in all.flatten() {
        if reference.target().try_name().is_some() {
            continue;
        }
        let Ok(id) = reference.peel_to_id().map(gix::Id::detach) else {
            continue;
        };
        let is_commit = repo
            .find_header(id)
            .is_ok_and(|h| h.kind() == gix::object::Kind::Commit);
        if !is_commit {
            continue;
        }
        tips.push(id);
        let name = reference.name();
        let kind = match name.category() {
            Some(Category::LocalBranch)
                if head_name.as_ref().is_some_and(|h| h.as_ref() == name) =>
            {
                RefKind::Head
            }
            Some(Category::LocalBranch) => RefKind::Branch,
            Some(Category::RemoteBranch) => RefKind::Remote,
            Some(Category::Tag) => RefKind::Tag,
            _ => continue,
        };
        refs.entry(id).or_default().push(Ref {
            name: name.shorten().to_string(),
            kind,
        });
    }
    for list in refs.values_mut() {
        list.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    }
    Ok((tips, refs))
}

/// Branch `head_name` (at `head`)'s commits ahead of and behind its upstream
/// (the branch's configured remote-tracking ref); `None` when it has no
/// upstream configured, or the upstream ref does not exist (never fetched).
pub(super) fn ahead_behind(
    repo: &gix::Repository,
    head: ObjectId,
    head_name: &gix::refs::FullNameRef,
) -> Result<Option<(u32, u32)>, String> {
    let Some(upstream) =
        repo.branch_remote_tracking_ref_name(head_name, gix::remote::Direction::Fetch)
    else {
        return Ok(None);
    };
    let upstream = upstream.map_err(fail("failed to resolve upstream"))?;
    // A missing upstream ref (configured but never fetched) is `None`; a
    // broken ref store is an error, not a silently hidden count.
    let Some(reference) = repo
        .try_find_reference(upstream.as_ref())
        .map_err(fail("failed to read upstream"))?
    else {
        return Ok(None);
    };
    let upstream = reference
        .into_fully_peeled_id()
        .map_err(fail("failed to resolve upstream"))?
        .detach();
    Ok(Some((
        count_only(repo, head, upstream)?,
        count_only(repo, upstream, head)?,
    )))
}

/// Commits reachable from `tip` but not from `hidden` — `git rev-list
/// tip ^hidden`, which is what git counts for ahead/behind. Hiding the other
/// side directly, not a merge base, keeps a criss-cross history (several
/// bases) exact.
fn count_only(repo: &gix::Repository, tip: ObjectId, hidden: ObjectId) -> Result<u32, String> {
    let walk = repo
        .rev_walk([tip])
        .with_hidden([hidden])
        .all()
        .map_err(fail("failed to walk history"))?;
    let mut count = 0u32;
    for info in walk {
        info.map_err(fail("failed to walk history"))?;
        count = count.saturating_add(1);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::{ahead_behind, log, Ref, RefKind, MAX_COMMITS};
    use crate::git_browser::fixture;
    use std::fmt::Write as _;

    fn r(name: &str, kind: RefKind) -> Ref {
        Ref {
            name: name.to_string(),
            kind,
        }
    }

    /// main: c1 ← c2 (HEAD); side: c1 ← s1, also `origin/main`; tag v1 on c1.
    fn diverged(tag: &str) -> Option<(fixture::Repo, [String; 3])> {
        let repo = fixture::init(tag)?;
        let c1 = repo.commit("first\n\nbody", 1_700_000_000);
        let c2 = repo.commit("second", 1_700_000_100);
        repo.git(&["tag", "-a", "v1", "-m", "release", &c1]);
        repo.git(&["checkout", "-q", "-b", "side", &c1]);
        let s1 = repo.commit("side work", 1_700_000_200);
        repo.git(&["update-ref", "refs/remotes/origin/main", &s1]);
        // The clone-time `origin/HEAD -> origin/main` symbolic ref gets no
        // badge of its own and must not badge `origin/main` twice.
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        repo.git(&["checkout", "-q", "main"]);
        Some((repo, [c1, c2, s1]))
    }

    #[test]
    fn log_lists_all_refs_newest_first_with_decorations() {
        let Some((repo, [c1, c2, s1])) = diverged("log-refs") else {
            return;
        };
        let view = log(repo.path()).expect("log");
        assert!(!view.truncated);
        let ids: Vec<&str> = view.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["@", s1.as_str(), c2.as_str(), c1.as_str()]);

        let wt = &view.rows[0];
        assert_eq!(wt.parents, [c2.as_str()]);
        assert_eq!((wt.short.as_str(), wt.subject.as_str(), wt.at), ("", "", 0));

        assert_eq!(
            view.rows[1].refs,
            [
                r("side", RefKind::Branch),
                r("origin/main", RefKind::Remote)
            ]
        );
        assert_eq!(view.rows[2].refs, [r("main", RefKind::Head)]);
        assert_eq!(view.rows[3].refs, [r("v1", RefKind::Tag)]);

        let first = &view.rows[3];
        assert_eq!(first.subject, "first");
        assert_eq!(first.short, &c1[..7]);
        assert_eq!(first.author, "Test");
        assert_eq!(first.at, 1_700_000_000);
        assert_eq!(first.parents.len(), 0);
        assert_eq!(view.rows[1].parents, [c1.as_str()]);

        // "@" and main share lane 0; side opens lane 1 waiting for c1, so
        // main's line bends into it and c1 sits there.
        let lanes: Vec<u32> = view.rows.iter().map(|r| r.lane).collect();
        assert_eq!(lanes, [0, 1, 0, 1]);
        assert_eq!(
            view.rows[2].edges,
            [
                super::Edge { from: 0, to: 1 },
                super::Edge { from: 1, to: 1 }
            ]
        );
        assert_eq!(view.width, 2);
    }

    #[test]
    fn a_detached_head_is_decorated_as_head() {
        let Some((repo, [c1, ..])) = diverged("log-detached") else {
            return;
        };
        repo.git(&["checkout", "-q", "--detach", &c1]);
        let view = log(repo.path()).expect("log");
        let row = view.rows.iter().find(|r| r.id == c1).expect("c1 listed");
        assert_eq!(row.refs, [r("HEAD", RefKind::Head), r("v1", RefKind::Tag)]);
        assert_eq!(view.rows[0].parents, [c1.as_str()]);
        // The main branch is a plain branch now.
        assert!(view.rows.iter().any(|r| r.refs.contains(&super::Ref {
            name: "main".to_string(),
            kind: RefKind::Branch
        })));
    }

    #[test]
    fn an_unborn_head_has_no_rows() {
        let Some(repo) = fixture::init("log-unborn") else {
            return;
        };
        let view = log(repo.path()).expect("log");
        assert_eq!(view.rows.len(), 0);
        assert_eq!(view.width, 0);
    }

    #[test]
    fn history_is_cut_at_the_commit_cap() {
        let Some(repo) = fixture::init("log-cap") else {
            return;
        };
        // fast-import builds the long history in one process.
        let mut stream = String::new();
        for i in 0..=MAX_COMMITS {
            writeln!(
                stream,
                "commit refs/heads/main\ncommitter T <t@e> {} +0000\ndata 2\nc\n",
                1_700_000_000 + i
            )
            .expect("write to String");
        }
        let mut child = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["fast-import", "--quiet"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn fast-import");
        std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), stream.as_bytes())
            .expect("feed fast-import");
        drop(child.stdin.take());
        assert!(child.wait().expect("fast-import").success());

        let view = log(repo.path()).expect("log");
        assert!(view.truncated);
        assert_eq!(view.rows.len(), MAX_COMMITS + 1, "commits plus the @ row");
    }

    #[test]
    fn ahead_behind_counts_against_the_upstream() {
        let Some((repo, [_, c2, _])) = diverged("log-upstream") else {
            return;
        };
        let open = || gix::open(repo.path()).expect("open");
        let head = gix::ObjectId::from_hex(c2.as_bytes()).expect("hex");
        let main: gix::refs::FullName = "refs/heads/main".try_into().expect("ref name");
        let main = main.as_ref();
        // No upstream configured.
        assert_eq!(ahead_behind(&open(), head, main), Ok(None));

        repo.git(&["remote", "add", "origin", "../nowhere"]);
        repo.git(&["config", "branch.main.remote", "origin"]);
        repo.git(&["config", "branch.main.merge", "refs/heads/main"]);
        assert_eq!(ahead_behind(&open(), head, main), Ok(Some((1, 1))));

        // Configured but never fetched: no upstream ref.
        repo.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
        assert_eq!(ahead_behind(&open(), head, main), Ok(None));
    }

    #[test]
    fn ahead_behind_counts_a_criss_cross_history_without_a_merge_base() {
        let Some(repo) = fixture::init("log-criss-cross") else {
            return;
        };
        // base ← left, base ← right; ours = merge(left, right) is HEAD,
        // theirs = merge(right, left) the upstream: two merge bases.
        let base = repo.commit("base", 1_700_000_000);
        let left = repo.commit("left", 1_700_000_100);
        repo.git(&["checkout", "-q", "-b", "other", &base]);
        let right = repo.commit("right", 1_700_000_200);
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["merge", "-q", "--no-ff", "--no-edit", &right]);
        let ours = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["checkout", "-q", "other"]);
        repo.git(&["merge", "-q", "--no-ff", "--no-edit", &left]);
        let theirs = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", &theirs]);
        repo.git(&["remote", "add", "origin", "../nowhere"]);
        repo.git(&["config", "branch.main.remote", "origin"]);
        repo.git(&["config", "branch.main.merge", "refs/heads/main"]);

        let open = gix::open(repo.path()).expect("open");
        let head = gix::ObjectId::from_hex(ours.as_bytes()).expect("hex");
        let main: gix::refs::FullName = "refs/heads/main".try_into().expect("ref name");
        assert_eq!(ahead_behind(&open, head, main.as_ref()), Ok(Some((1, 1))));
    }
}
