//! `git_browser_file`: one file in full with its change inline — every
//! line of the newer side, the removed lines of the older side in place,
//! with old/new line numbers. Also the renderer behind
//! `git_browser_commit_file` ([`view`]).

use super::{fail, open, project_relative, status, StatusKind};
use gix::diff::blob::{
    diff_with_slider_heuristics, Algorithm, Hunk, InternedInput, Interner, Token,
};
use std::io::Read as _;
use std::path::Path;

/// A side bigger than this (in git form) is not diffed: the view is
/// `TooLarge`, with no lines.
const MAX_SIDE_BYTES: usize = 2 * 1024 * 1024;
/// Lines emitted before the rest is dropped and the view marked `truncated`.
const MAX_LINES: usize = 20_000;
/// Characters kept per line; a longer one is cut and the view marked
/// `truncated`.
const MAX_LINE_CHARS: usize = 500;
/// A NUL byte this far into either side makes the file binary (git's
/// heuristic).
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FileKind {
    Text,
    Binary,
    /// Neither side exists.
    Missing,
    /// A side exceeds [`MAX_SIDE_BYTES`]; no lines.
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Ctx,
    Add,
    Rem,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct Line {
    pub kind: LineKind,
    /// 1-based line number in the older side; `None` for an added line.
    pub old: Option<u32>,
    /// 1-based line number in the newer side; `None` for a removed line.
    pub new: Option<u32>,
    /// Without its line break, lossily decoded.
    pub text: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    /// Normalised project-relative path.
    pub path: String,
    pub kind: FileKind,
    pub status: Option<StatusKind>,
    pub adds: u32,
    pub rems: u32,
    pub lines: Vec<Line>,
    /// The line count or a line's length hit its cap.
    pub truncated: bool,
}

/// `git_browser_file(path, rel_path)`: the work-tree file against its
/// `HEAD` blob — the rename source's blob for a renamed file.
pub fn file(path: &Path, rel_path: &str) -> Result<FileView, String> {
    let (mut repo, work_dir) = open(path)?;
    // `core.safecrlf=true` makes the filter pipeline refuse a file whose line
    // endings would not survive a round trip; `git diff` downgrades that to a
    // warning when it reads a file to show it, and so does this view.
    repo.config_snapshot_mut()
        .set_value(&gix::config::tree::Core::SAFE_CRLF, "false")
        .map_err(fail("failed to configure git filters"))?;
    let rel = project_relative(&work_dir, rel_path)?;
    let changes = status::changes(&repo)?;
    let base = changes.sources.get(&rel).unwrap_or(&rel);
    let head = repo.head().map_err(fail("failed to read HEAD"))?;
    let before = if head.is_unborn() {
        Side::Absent
    } else {
        let tree = repo.head_tree().map_err(fail("failed to read HEAD tree"))?;
        blob_at(&repo, &tree, base)?
    };
    let status = changes.kinds.get(&rel).copied();
    let after = read_worktree(&repo, &work_dir, &rel)?;
    Ok(render(rel, status, &before, &after))
}

/// One side of a file view, read with its size cap.
pub(super) enum Side {
    /// Absent, or not a file or symlink.
    Absent,
    /// More than [`MAX_SIDE_BYTES`] in git form; not read.
    TooLarge,
    /// The content in git form: a blob, a work-tree file converted as
    /// `git add` would store it, or a symlink's target as git stores it.
    Bytes(Vec<u8>),
}

impl Side {
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::Absent | Self::TooLarge => None,
        }
    }
}

/// The blob at `rel` in `tree`; `Absent` when the path is missing or names
/// anything but a file or symlink (a directory, a submodule). The size is
/// read from the object header, so an oversized blob is never inflated.
pub(super) fn blob_at(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    rel: &str,
) -> Result<Side, String> {
    let Some(entry) = tree
        .lookup_entry_by_path(rel)
        .map_err(fail("failed to read tree"))?
    else {
        return Ok(Side::Absent);
    };
    if !entry.mode().is_blob_or_symlink() {
        return Ok(Side::Absent);
    }
    let header = repo
        .find_header(entry.object_id())
        .map_err(fail("failed to read blob"))?;
    if header.size() > MAX_SIDE_BYTES as u64 {
        return Ok(Side::TooLarge);
    }
    let mut blob = repo
        .find_blob(entry.object_id())
        .map_err(fail("failed to read blob"))?;
    Ok(Side::Bytes(blob.take_data()))
}

/// Read the work-tree side of `rel` through git's filter pipeline
/// (`core.autocrlf`, `eol`/`text`/`ident` attributes, clean filters), so a
/// file checked out with CRLF line endings compares equal to its LF blob.
fn read_worktree(repo: &gix::Repository, work_dir: &Path, rel: &str) -> Result<Side, String> {
    let abs = work_dir.join(rel);
    let meta = match std::fs::symlink_metadata(&abs) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Side::Absent),
        Err(e) => return Err(format!("failed to read {}: {e}", abs.display())),
    };
    if meta.is_symlink() {
        let target = std::fs::read_link(&abs).map_err(fail("failed to read symlink"))?;
        return Ok(Side::Bytes(target.into_os_string().into_encoded_bytes()));
    }
    if !meta.is_file() {
        return Ok(Side::Absent);
    }
    // End-of-line conversion at most halves a file, so a work-tree file
    // over twice the cap cannot fit after it; such a file is not read at
    // all (a clean filter that shrinks it further, like LFS, included).
    if meta.len() > 2 * MAX_SIDE_BYTES as u64 {
        return Ok(Side::TooLarge);
    }
    let (mut pipeline, index) = repo
        .filter_pipeline(None)
        .map_err(fail("failed to set up git filters"))?;
    let source = std::fs::File::open(&abs).map_err(fail("failed to open file"))?;
    let converted = pipeline
        .convert_to_git(source, Path::new(rel), &index)
        .map_err(fail("failed to apply git filters"))?;
    let mut bytes = Vec::new();
    converted
        .take(MAX_SIDE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(fail("failed to read file"))?;
    Ok(if bytes.len() > MAX_SIDE_BYTES {
        Side::TooLarge
    } else {
        Side::Bytes(bytes)
    })
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

impl FileView {
    /// A `Text` view of `path` without lines.
    const fn empty(path: String, status: Option<StatusKind>) -> Self {
        Self {
            path,
            kind: FileKind::Text,
            status,
            adds: 0,
            rems: 0,
            lines: Vec::new(),
            truncated: false,
        }
    }
}

/// One line's display text: the line break stripped, lossily decoded, cut
/// to [`MAX_LINE_CHARS`]. The second value says whether it was cut.
fn line_text(token: &[u8]) -> (String, bool) {
    let bytes = token.strip_suffix(b"\n").unwrap_or(token);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    // A char is at most 4 bytes, so this prefix still holds MAX_LINE_CHARS
    // chars whenever the line has that many; decoding it instead of the
    // whole line bounds the work for a minified megabyte-long line. A char
    // split at the cut decodes to U+FFFD past the char cap, never within it.
    let head = &bytes[..bytes.len().min(MAX_LINE_CHARS * 4)];
    let text = String::from_utf8_lossy(head);
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => (text[..end].to_string(), true),
        None => (text.into_owned(), head.len() < bytes.len()),
    }
}

/// Collects lines up to [`MAX_LINES`].
struct Emitter<'a> {
    interner: &'a Interner<&'a [u8]>,
    lines: Vec<Line>,
    truncated: bool,
}

impl Emitter<'_> {
    /// Emit `token` with its 0-based line numbers; `false` once the line cap
    /// is reached.
    fn push(&mut self, kind: LineKind, old: Option<u32>, new: Option<u32>, token: Token) -> bool {
        if self.lines.len() == MAX_LINES {
            self.truncated = true;
            return false;
        }
        let (text, cut) = line_text(self.interner[token]);
        self.truncated |= cut;
        self.lines.push(Line {
            kind,
            old: old.map(|o| o + 1),
            new: new.map(|n| n + 1),
            text,
        });
        true
    }
}

fn token_count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// [`view`] of two capped sides: `TooLarge`, with no lines, when either is.
pub(super) fn render(
    path: String,
    status: Option<StatusKind>,
    before: &Side,
    after: &Side,
) -> FileView {
    if matches!(before, Side::TooLarge) || matches!(after, Side::TooLarge) {
        let mut out = FileView::empty(path, status);
        out.kind = FileKind::TooLarge;
        return out;
    }
    view(path, status, before.bytes(), after.bytes())
}

/// Render `before` → `after` as one full-file view. `None` means the side
/// does not exist (an added or deleted file); both `None` is `Missing`.
pub(super) fn view(
    path: String,
    status: Option<StatusKind>,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> FileView {
    let mut out = FileView::empty(path, status);
    if before.is_none() && after.is_none() {
        out.kind = FileKind::Missing;
        return out;
    }
    let (before, after) = (before.unwrap_or_default(), after.unwrap_or_default());
    if before.len() > MAX_SIDE_BYTES || after.len() > MAX_SIDE_BYTES {
        out.kind = FileKind::TooLarge;
        return out;
    }
    if is_binary(before) || is_binary(after) {
        out.kind = FileKind::Binary;
        return out;
    }

    let input = InternedInput::new(before, after);
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
    out.adds = diff.count_additions();
    out.rems = diff.count_removals();

    let mut emit = Emitter {
        interner: &input.interner,
        lines: Vec::new(),
        truncated: false,
    };
    let (old_len, new_len) = (
        token_count(input.before.len()),
        token_count(input.after.len()),
    );
    // A final empty hunk at the end of both sides flushes the trailing context.
    let tail = Hunk {
        before: old_len..old_len,
        after: new_len..new_len,
    };
    let (mut old, mut new) = (0u32, 0u32);
    'hunks: for hunk in diff.hunks().chain(std::iter::once(tail)) {
        let (old_end, new_end) = (hunk.before.end, hunk.after.end);
        while old < hunk.before.start {
            if !emit.push(
                LineKind::Ctx,
                Some(old),
                Some(new),
                input.after[new as usize],
            ) {
                break 'hunks;
            }
            old += 1;
            new += 1;
        }
        for o in hunk.before {
            if !emit.push(LineKind::Rem, Some(o), None, input.before[o as usize]) {
                break 'hunks;
            }
        }
        for n in hunk.after {
            if !emit.push(LineKind::Add, None, Some(n), input.after[n as usize]) {
                break 'hunks;
            }
        }
        (old, new) = (old_end, new_end);
    }
    out.lines = emit.lines;
    out.truncated = emit.truncated;
    out
}

#[cfg(test)]
mod tests {
    use super::{file, view, FileKind, Line, LineKind, MAX_LINES, MAX_LINE_CHARS, MAX_SIDE_BYTES};
    use crate::git_browser::{fixture, StatusKind};
    use std::fmt::Write as _;

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> Line {
        Line {
            kind,
            old,
            new,
            text: text.to_string(),
        }
    }

    fn render(before: Option<&str>, after: Option<&str>) -> super::FileView {
        view(
            "f.txt".to_string(),
            None,
            before.map(str::as_bytes),
            after.map(str::as_bytes),
        )
    }

    #[test]
    fn unchanged_file_is_all_context_with_both_numbers() {
        let out = render(Some("a\nb\n"), Some("a\nb\n"));
        assert_eq!(out.kind, FileKind::Text);
        assert_eq!((out.adds, out.rems, out.truncated), (0, 0, false));
        assert_eq!(
            out.lines,
            [
                line(LineKind::Ctx, Some(1), Some(1), "a"),
                line(LineKind::Ctx, Some(2), Some(2), "b"),
            ]
        );
    }

    #[test]
    fn a_hunk_shows_removed_lines_before_added_ones_with_shifted_numbers() {
        let out = render(Some("a\nb\nc\nd\n"), Some("a\nB1\nB2\nB3\nB4\nc\nd\n"));
        assert_eq!((out.adds, out.rems), (4, 1));
        assert_eq!(
            out.lines,
            [
                line(LineKind::Ctx, Some(1), Some(1), "a"),
                line(LineKind::Rem, Some(2), None, "b"),
                line(LineKind::Add, None, Some(2), "B1"),
                line(LineKind::Add, None, Some(3), "B2"),
                line(LineKind::Add, None, Some(4), "B3"),
                line(LineKind::Add, None, Some(5), "B4"),
                line(LineKind::Ctx, Some(3), Some(6), "c"),
                line(LineKind::Ctx, Some(4), Some(7), "d"),
            ]
        );
    }

    #[test]
    fn separate_hunks_keep_context_between_them() {
        let out = render(Some("x\n1\n2\n3\ny\n"), Some("1\n2\n3\nz\n"));
        assert_eq!(
            out.lines,
            [
                line(LineKind::Rem, Some(1), None, "x"),
                line(LineKind::Ctx, Some(2), Some(1), "1"),
                line(LineKind::Ctx, Some(3), Some(2), "2"),
                line(LineKind::Ctx, Some(4), Some(3), "3"),
                line(LineKind::Rem, Some(5), None, "y"),
                line(LineKind::Add, None, Some(4), "z"),
            ]
        );
    }

    #[test]
    fn a_new_file_is_all_added_and_a_deleted_one_all_removed() {
        let added = render(None, Some("one\ntwo"));
        assert_eq!((added.adds, added.rems), (2, 0));
        assert_eq!(
            added.lines,
            [
                line(LineKind::Add, None, Some(1), "one"),
                line(LineKind::Add, None, Some(2), "two"),
            ]
        );
        let deleted = render(Some("one\r\ntwo\r\n"), None);
        assert_eq!((deleted.adds, deleted.rems), (0, 2));
        assert_eq!(
            deleted.lines,
            [
                line(LineKind::Rem, Some(1), None, "one"),
                line(LineKind::Rem, Some(2), None, "two"),
            ]
        );
    }

    #[test]
    fn neither_side_is_missing_and_a_nul_byte_is_binary() {
        assert_eq!(render(None, None).kind, FileKind::Missing);
        let binary = view(
            "b.bin".to_string(),
            Some(StatusKind::Modified),
            Some(b"text\n"),
            Some(b"PNG\0\x01\x02"),
        );
        assert_eq!(binary.kind, FileKind::Binary);
        assert_eq!(binary.lines.len(), 0);
        assert_eq!(binary.status, Some(StatusKind::Modified));
        // A NUL past the sniff window does not count.
        let mut late = vec![b'a'; super::BINARY_SNIFF_BYTES];
        late.push(0);
        assert_eq!(
            view(String::new(), None, None, Some(&late)).kind,
            FileKind::Text
        );
    }

    #[test]
    fn long_lines_are_cut_at_the_char_cap() {
        let long = "é".repeat(MAX_LINE_CHARS + 10);
        let out = render(None, Some(&format!("{long}\nshort\n")));
        assert!(out.truncated);
        assert_eq!(out.lines[0].text.chars().count(), MAX_LINE_CHARS);
        assert!(out.lines[0].text.chars().all(|c| c == 'é'));
        assert_eq!(out.lines[1].text, "short");

        let exact = "x".repeat(MAX_LINE_CHARS);
        let out = render(None, Some(&exact));
        assert!(!out.truncated);
        assert_eq!(out.lines[0].text, exact);
    }

    #[test]
    fn output_stops_at_the_line_cap() {
        let mut text = String::new();
        for i in 0..=MAX_LINES {
            writeln!(text, "{i}").expect("write to String");
        }
        let out = render(None, Some(&text));
        assert!(out.truncated);
        assert_eq!(out.lines.len(), MAX_LINES);
        assert_eq!(
            out.adds as usize,
            MAX_LINES + 1,
            "counts cover the whole diff"
        );
        assert_eq!(
            out.lines[MAX_LINES - 1].new,
            Some(u32::try_from(MAX_LINES).unwrap_or(0))
        );
    }

    #[test]
    fn a_side_past_the_byte_cap_is_too_large_with_no_lines() {
        let line = format!("{}\n", "y".repeat(399));
        let big = line.repeat(MAX_SIDE_BYTES / 400 + 5);
        for (before, after) in [
            (Some("y\n"), Some(big.as_str())),
            (Some(big.as_str()), None),
        ] {
            let out = render(before, after);
            assert_eq!(out.kind, FileKind::TooLarge);
            assert_eq!((out.adds, out.rems, out.truncated), (0, 0, false));
            assert_eq!(out.lines.len(), 0);
        }
        let exact = "y".repeat(MAX_SIDE_BYTES);
        assert_eq!(render(None, Some(&exact)).kind, FileKind::Text);
        assert_eq!(
            serde_json::to_value(FileKind::TooLarge).expect("serialise"),
            "TooLarge"
        );
    }

    #[test]
    fn a_worktree_file_past_the_byte_cap_is_too_large() {
        let Some(repo) = fixture::init("file-too-large") else {
            return;
        };
        repo.write("a.txt", "a\n");
        repo.git(&["add", "."]);
        repo.commit("init", 1_700_000_000);
        repo.write("a.txt", &"y\n".repeat(MAX_SIDE_BYTES / 2 + 1));
        let out = file(repo.path(), "a.txt").expect("file");
        assert_eq!(out.kind, FileKind::TooLarge);
        assert_eq!(out.status, Some(StatusKind::Modified));
        assert_eq!((out.adds, out.rems, out.truncated), (0, 0, false));
        assert_eq!(out.lines.len(), 0);
    }

    #[test]
    fn a_head_blob_past_the_byte_cap_is_too_large() {
        let Some(repo) = fixture::init("file-blob-too-large") else {
            return;
        };
        repo.write("a.txt", &"y\n".repeat(MAX_SIDE_BYTES / 2 + 1));
        repo.git(&["add", "."]);
        repo.commit("init", 1_700_000_000);
        repo.write("a.txt", "a\n");
        let out = file(repo.path(), "a.txt").expect("file");
        assert_eq!(out.kind, FileKind::TooLarge);
        assert_eq!(out.status, Some(StatusKind::Modified));
        assert_eq!(out.lines.len(), 0);
    }

    #[test]
    fn a_crlf_checkout_of_an_lf_blob_is_unchanged() {
        let Some(repo) = fixture::init("file-crlf") else {
            return;
        };
        repo.write(".gitattributes", "*.txt text eol=crlf\n");
        repo.write("a.txt", "one\ntwo\n");
        repo.git(&["add", "."]);
        repo.commit("init", 1_700_000_000);
        // Check the file out again so the attribute writes CRLF endings.
        std::fs::remove_file(repo.path().join("a.txt")).expect("delete");
        repo.git(&["checkout", "--", "a.txt"]);
        let on_disk = std::fs::read(repo.path().join("a.txt")).expect("read");
        assert_eq!(on_disk, b"one\r\ntwo\r\n");

        let out = file(repo.path(), "a.txt").expect("file");
        assert_eq!(out.status, None);
        assert_eq!((out.adds, out.rems), (0, 0));
        assert_eq!(
            out.lines,
            [
                line(LineKind::Ctx, Some(1), Some(1), "one"),
                line(LineKind::Ctx, Some(2), Some(2), "two"),
            ]
        );
    }

    #[test]
    fn safecrlf_does_not_refuse_a_file_whose_endings_would_not_round_trip() {
        let Some(repo) = fixture::init("file-safecrlf") else {
            return;
        };
        repo.write("a.txt", "one\n");
        repo.git(&["add", "."]);
        repo.commit("init", 1_700_000_000);
        repo.git(&["config", "core.autocrlf", "input"]);
        repo.git(&["config", "core.safecrlf", "true"]);
        // `autocrlf=input` turns CRLF into LF on the way in, which `safecrlf`
        // would refuse as lossy; `git diff` only warns, and so must the view.
        repo.write("b.txt", "mixed\r\nendings\r\n");

        let out = file(repo.path(), "b.txt").expect("safecrlf must not fail the view");
        assert_eq!(out.kind, FileKind::Text);
        assert_eq!(out.status, Some(StatusKind::Untracked));
        assert_eq!(
            out.lines,
            [
                line(LineKind::Add, None, Some(1), "mixed"),
                line(LineKind::Add, None, Some(2), "endings"),
            ]
        );
    }

    #[test]
    fn worktree_file_diffs_against_head() {
        let Some(repo) = fixture::init("file-worktree") else {
            return;
        };
        repo.write("a.txt", "one\ntwo\nthree\n");
        repo.write("b.txt", "same\n");
        repo.write("gone.txt", "bye\n");
        repo.git(&["add", "."]);
        repo.commit("init", 1_700_000_000);
        repo.write("a.txt", "one\n2\nthree\n");
        repo.write("new.txt", "hello\n");
        std::fs::remove_file(repo.path().join("gone.txt")).expect("delete");

        let a = file(repo.path(), "a.txt").expect("file");
        assert_eq!(
            (a.path.as_str(), a.status),
            ("a.txt", Some(StatusKind::Modified))
        );
        assert_eq!(
            a.lines,
            [
                line(LineKind::Ctx, Some(1), Some(1), "one"),
                line(LineKind::Rem, Some(2), None, "two"),
                line(LineKind::Add, None, Some(2), "2"),
                line(LineKind::Ctx, Some(3), Some(3), "three"),
            ]
        );
        let b = file(repo.path(), "b.txt").expect("file");
        assert_eq!(b.status, None);
        assert_eq!(b.lines, [line(LineKind::Ctx, Some(1), Some(1), "same")]);
        let new = file(repo.path(), "new.txt").expect("file");
        assert_eq!(new.status, Some(StatusKind::Untracked));
        assert_eq!(new.lines, [line(LineKind::Add, None, Some(1), "hello")]);
        let gone = file(repo.path(), "gone.txt").expect("file");
        assert_eq!(gone.status, Some(StatusKind::Deleted));
        assert_eq!(gone.lines, [line(LineKind::Rem, Some(1), None, "bye")]);
        assert_eq!(
            file(repo.path(), "nowhere.txt").expect("file").kind,
            FileKind::Missing
        );

        // An absolute path inside the work dir resolves to the same file.
        let abs = repo.path().join("a.txt");
        let by_abs = file(repo.path(), abs.to_str().expect("utf-8")).expect("file");
        assert_eq!(by_abs.path, "a.txt");
        assert_eq!(by_abs.lines, a.lines);
        assert!(file(repo.path(), "../a.txt").is_err());
    }

    #[test]
    fn a_staged_rename_diffs_against_its_source() {
        let Some(repo) = fixture::init("file-rename") else {
            return;
        };
        repo.write("old.txt", "l1\nl2\nl3\nl4\nl5\n");
        repo.git(&["add", "."]);
        repo.commit("init", 1_700_000_000);
        repo.git(&["mv", "old.txt", "new.txt"]);
        repo.write("new.txt", "l1\nl2\nl3\nl4\nl5\nl6\n");

        let out = file(repo.path(), "new.txt").expect("file");
        assert_eq!(out.status, Some(StatusKind::Renamed));
        assert_eq!((out.adds, out.rems), (1, 0));
        assert_eq!(
            out.lines.last(),
            Some(&line(LineKind::Add, None, Some(6), "l6"))
        );
    }

    #[test]
    fn an_unborn_head_shows_staged_files_as_added_lines() {
        let Some(repo) = fixture::init("file-unborn") else {
            return;
        };
        repo.write("a.txt", "x\n");
        repo.git(&["add", "."]);
        let out = file(repo.path(), "a.txt").expect("file");
        assert_eq!(out.status, Some(StatusKind::Added));
        assert_eq!(out.lines, [line(LineKind::Add, None, Some(1), "x")]);
    }
}
