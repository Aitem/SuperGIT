//! Structural diffs through difftastic (`difft`), an optional external tool.
//!
//! `difft --display json` reports, per file, the lines that hold novel syntax
//! and the byte ranges of those tokens — but neither the text of the lines nor
//! how the unchanged lines of both sides line up. So this module reads both
//! sides of each file itself (from Git objects or the working tree), runs
//! difftastic on them in temporary files named like the originals (difftastic
//! picks the language by file name), and aligns the two sides into rows that a
//! side-by-side or unified view can show.
//!
//! The JSON output is marked unstable by difftastic (`DFT_UNSTABLE=yes`); the
//! parser accepts missing fields so a newer version degrades instead of failing.

use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use serde::Deserialize;

use crate::runner::git_bytes_with_input;

/// Files beyond this many are listed but not compared.
pub const MAX_FILES: usize = 300;
/// Larger sides are not compared: they would be slow to align and to show.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Like Git, a NUL byte early in a file marks it as binary.
const BINARY_PROBE: usize = 8000;
const TAB_WIDTH: usize = 4;
/// Unchanged stretches shorter than this stay visible between two hunks.
const MIN_FOLDED_ROWS: usize = 2;

/// The installed difftastic's version (`0.67.0`), or `None` when `difft` is
/// not on `PATH`.
pub fn version() -> Option<String> {
    let out = Command::new("difft")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim();
    Some(
        first
            .strip_prefix("Difftastic ")
            .unwrap_or(first)
            .to_string(),
    )
}

/// Where one side of a comparison comes from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A commit-ish such as `HEAD`, a sha or `sha^`. A path missing there —
    /// or a revision that does not exist, like the parent of a root commit —
    /// reads as an absent file.
    Revision(String),
    /// The files on disk in the worktree.
    WorkTree,
}

/// One file of a patch to compare structurally.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileRequest {
    /// The path after the change (the old path for a deletion).
    pub path: String,
    /// The path before a rename or copy.
    pub old_path: Option<String>,
    /// Created by the change: there is no old side to read.
    pub added: bool,
    /// Removed by the change: there is no new side to read.
    pub deleted: bool,
}

impl FileRequest {
    fn old_path(&self) -> &str {
        self.old_path.as_deref().unwrap_or(&self.path)
    }
}

/// One side of a file as read for comparison.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Content {
    Absent,
    Bytes(Vec<u8>),
    TooLarge(u64),
}

/// Both sides of one file, read and ready to compare.
#[derive(Clone, Debug)]
pub struct FilePair {
    pub request: FileRequest,
    pub old: Content,
    pub new: Content,
}

impl FilePair {
    /// Identifies the comparison's inputs, so an unchanged file need not be
    /// compared again on the next refresh.
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        self.request.hash(&mut hasher);
        self.old.hash(&mut hasher);
        self.new.hash(&mut hasher);
        hasher.finish()
    }
}

/// What difftastic made of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Changed,
    Created,
    Deleted,
    /// Only whitespace, formatting or the file mode differs.
    Unchanged,
    Binary,
    TooLarge,
    /// Beyond [`MAX_FILES`].
    Skipped,
    Failed(String),
}

/// A source line: tabs expanded, line ending removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// Difftastic listed the line as part of a change.
    pub changed: bool,
    /// Byte ranges of `text` holding novel tokens, ordered, not overlapping.
    pub novel: Vec<Range<usize>>,
}

/// One display row: a line of either side, or of both when they correspond.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub old: Option<usize>,
    pub new: Option<usize>,
}

/// One file of a structural diff, aligned for display.
#[derive(Clone, Debug)]
pub struct StructuralFile {
    pub path: String,
    pub old_path: Option<String>,
    /// Difftastic's name for the language it parsed (`Rust`, `Text`).
    pub language: Option<String>,
    pub outcome: Outcome,
    pub old: Vec<Line>,
    pub new: Vec<Line>,
    pub rows: Vec<Row>,
}

impl StructuralFile {
    fn empty(request: &FileRequest, outcome: Outcome) -> Self {
        Self {
            path: request.path.clone(),
            old_path: request.old_path.clone(),
            language: None,
            outcome,
            old: Vec::new(),
            new: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// New lines that are part of a change.
    pub fn additions(&self) -> usize {
        self.new.iter().filter(|line| line.changed).count()
    }

    /// Old lines that are part of a change.
    pub fn deletions(&self) -> usize {
        self.old.iter().filter(|line| line.changed).count()
    }

    pub fn is_changed_row(&self, row: &Row) -> bool {
        let changed = |lines: &[Line], ix: Option<usize>| {
            ix.and_then(|ix| lines.get(ix))
                .is_some_and(|line| line.changed)
        };
        changed(&self.old, row.old) || changed(&self.new, row.new)
    }

    /// Row ranges around the changes with `context` unchanged rows on each
    /// side; stretches shorter than two rows between them are kept.
    pub fn hunks(&self, context: usize) -> Vec<Range<usize>> {
        let mut hunks: Vec<Range<usize>> = Vec::new();
        for (ix, row) in self.rows.iter().enumerate() {
            if !self.is_changed_row(row) {
                continue;
            }
            let start = ix.saturating_sub(context);
            let end = (ix + context + 1).min(self.rows.len());
            match hunks.last_mut() {
                Some(last) if start < last.end + MIN_FOLDED_ROWS => last.end = last.end.max(end),
                _ => hunks.push(start..end),
            }
        }
        hunks
    }
}

/// Reads both sides of `files`: `old` and `new` say where each side lives.
pub fn read_pairs(
    worktree: &Path,
    old: &Source,
    new: &Source,
    files: &[FileRequest],
) -> Result<Vec<FilePair>> {
    let files = &files[..files.len().min(MAX_FILES)];
    let old_specs: Vec<Option<String>> = files
        .iter()
        .map(|file| (!file.added).then(|| file.old_path().to_string()))
        .collect();
    let new_specs: Vec<Option<String>> = files
        .iter()
        .map(|file| (!file.deleted).then(|| file.path.clone()))
        .collect();
    let old_contents = read_side(worktree, old, &old_specs)?;
    let new_contents = read_side(worktree, new, &new_specs)?;
    Ok(files
        .iter()
        .zip(old_contents.into_iter().zip(new_contents))
        .map(|(request, (old, new))| FilePair {
            request: request.clone(),
            old,
            new,
        })
        .collect())
}

fn read_side(worktree: &Path, source: &Source, paths: &[Option<String>]) -> Result<Vec<Content>> {
    match source {
        Source::WorkTree => Ok(paths
            .iter()
            .map(|path| match path {
                Some(path) => read_worktree_file(&worktree.join(path)),
                None => Content::Absent,
            })
            .collect()),
        Source::Revision(revision) => read_blobs(worktree, revision, paths),
    }
}

fn read_worktree_file(path: &Path) -> Content {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Content::Absent;
    };
    // Git stores a link's target path, never the target's contents.
    if meta.file_type().is_symlink() {
        return std::fs::read_link(path)
            .map(|target| Content::Bytes(target.as_os_str().as_encoded_bytes().to_vec()))
            .unwrap_or(Content::Absent);
    }
    if !meta.is_file() {
        return Content::Absent;
    }
    if meta.len() > MAX_BYTES {
        return Content::TooLarge(meta.len());
    }
    std::fs::read(path)
        .map(Content::Bytes)
        .unwrap_or(Content::Absent)
}

/// Blobs at `revision` through one `git cat-file --batch`.
fn read_blobs(worktree: &Path, revision: &str, paths: &[Option<String>]) -> Result<Vec<Content>> {
    let mut contents = vec![Content::Absent; paths.len()];
    // A path with a newline cannot be named on the batch protocol's lines.
    let requested: Vec<(usize, &str)> = paths
        .iter()
        .enumerate()
        .filter_map(|(ix, path)| Some((ix, path.as_deref()?)))
        .filter(|(_, path)| !path.contains('\n'))
        .collect();
    if requested.is_empty() {
        return Ok(contents);
    }
    let mut input = Vec::new();
    for (_, path) in &requested {
        input.extend_from_slice(format!("{revision}:{path}\n").as_bytes());
    }
    let out = git_bytes_with_input(worktree, &["cat-file", "--batch"], input)?;
    for ((ix, _), content) in requested.iter().zip(parse_batch(&out)) {
        contents[*ix] = content;
    }
    Ok(contents)
}

/// `<oid> <type> <size>\n<bytes>\n` per found object, `<name> missing\n`
/// (or `ambiguous`) otherwise.
fn parse_batch(mut out: &[u8]) -> Vec<Content> {
    let mut contents = Vec::new();
    while !out.is_empty() {
        let Some(end) = out.iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&out[..end]).into_owned();
        out = &out[end + 1..];
        let mut fields = header.rsplitn(3, ' ');
        let (size, kind) = (fields.next(), fields.next());
        let size = match (kind, size.and_then(|size| size.parse::<usize>().ok())) {
            (Some(kind), Some(size)) => Some((kind, size)),
            _ => None,
        };
        let Some((kind, size)) = size else {
            // `missing` or `ambiguous`: no body follows.
            contents.push(Content::Absent);
            continue;
        };
        let body = &out[..size.min(out.len())];
        out = &out[(size + 1).min(out.len())..];
        contents.push(if kind != "blob" {
            // A submodule's commit: nothing to compare as text.
            Content::Absent
        } else if size as u64 > MAX_BYTES {
            Content::TooLarge(size as u64)
        } else {
            Content::Bytes(body.to_vec())
        });
    }
    contents
}

/// Compares each pair with difftastic, several at a time. Results keep the
/// order of `pairs`.
pub fn compare_many(pairs: &[&FilePair]) -> Vec<StructuralFile> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
        .min(pairs.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<StructuralFile>>> = Mutex::new(vec![None; pairs.len()]);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let ix = next.fetch_add(1, Ordering::Relaxed);
                    let Some(pair) = pairs.get(ix) else {
                        break;
                    };
                    let file = compare(pair);
                    if let Ok(mut results) = results.lock() {
                        results[ix] = Some(file);
                    }
                }
            });
        }
    });
    let results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results
        .into_iter()
        .zip(pairs)
        .map(|(file, pair)| {
            file.unwrap_or_else(|| {
                StructuralFile::empty(&pair.request, Outcome::Failed("Comparison failed".into()))
            })
        })
        .collect()
}

/// A file beyond [`MAX_FILES`], listed without comparing it.
pub fn skipped(request: &FileRequest) -> StructuralFile {
    StructuralFile::empty(request, Outcome::Skipped)
}

/// Compares one pair with difftastic.
pub fn compare(pair: &FilePair) -> StructuralFile {
    let request = &pair.request;
    let bytes = |content: &Content| match content {
        Content::Bytes(bytes) => Some(bytes.clone()),
        _ => None,
    };
    if matches!(pair.old, Content::TooLarge(_)) || matches!(pair.new, Content::TooLarge(_)) {
        return StructuralFile::empty(request, Outcome::TooLarge);
    }
    let (old, new) = (bytes(&pair.old), bytes(&pair.new));
    let is_binary = |bytes: &Option<Vec<u8>>| {
        bytes
            .as_ref()
            .is_some_and(|bytes| bytes[..bytes.len().min(BINARY_PROBE)].contains(&0))
    };
    if is_binary(&old) || is_binary(&new) {
        return StructuralFile::empty(request, Outcome::Binary);
    }
    if old == new {
        // A mode change or a pure rename: nothing for difftastic to show.
        let lines = old.as_deref().map(split_lines).unwrap_or_default();
        return build(request, &lines, &lines, &Report::unchanged());
    }
    match run_difft(request, old.as_deref(), new.as_deref()) {
        Ok(report) => {
            let old_lines = old.as_deref().map(split_lines).unwrap_or_default();
            let new_lines = new.as_deref().map(split_lines).unwrap_or_default();
            build(request, &old_lines, &new_lines, &report)
        }
        Err(error) => StructuralFile::empty(request, Outcome::Failed(error.to_string())),
    }
}

fn run_difft(request: &FileRequest, old: Option<&[u8]>, new: Option<&[u8]>) -> Result<Report> {
    let dir = tempfile::tempdir()?;
    let write = |side: &str, path: &str, bytes: Option<&[u8]>| -> Result<std::path::PathBuf> {
        let Some(bytes) = bytes else {
            return Ok("/dev/null".into());
        };
        let name = path.rsplit('/').next().filter(|n| !n.is_empty());
        let file = dir.path().join(side).join(name.unwrap_or("file"));
        std::fs::create_dir_all(dir.path().join(side))?;
        std::fs::write(&file, bytes)?;
        Ok(file)
    };
    let old_file = write("a", request.old_path(), old)?;
    let new_file = write("b", &request.path, new)?;
    let out = Command::new("difft")
        .args(["--display", "json", "--color", "never"])
        .arg(&old_file)
        .arg(&new_file)
        .env("DFT_UNSTABLE", "yes")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| anyhow::anyhow!("Cannot run difft: {error}"))?;
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "difft produced no output{}",
            stderr
                .lines()
                .next()
                .map(|line| format!(": {line}"))
                .unwrap_or_default()
        );
    }
    parse_report(&out.stdout)
}

/// One file of `difft --display json`.
#[derive(Debug, Default, Deserialize)]
struct Report {
    #[serde(default)]
    chunks: Vec<Vec<ReportLine>>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    status: String,
}

impl Report {
    fn unchanged() -> Self {
        Self {
            status: "unchanged".into(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReportLine {
    #[serde(default)]
    lhs: Option<ReportSide>,
    #[serde(default)]
    rhs: Option<ReportSide>,
}

#[derive(Debug, Deserialize)]
struct ReportSide {
    line_number: usize,
    #[serde(default)]
    changes: Vec<ReportChange>,
}

#[derive(Debug, Deserialize)]
struct ReportChange {
    start: usize,
    end: usize,
}

fn parse_report(json: &[u8]) -> Result<Report> {
    let value: serde_json::Value = serde_json::from_slice(json)?;
    // Several files (a directory comparison) arrive as an array.
    let value = match value {
        serde_json::Value::Array(mut files) if !files.is_empty() => files.swap_remove(0),
        other => other,
    };
    Ok(serde_json::from_value(value)?)
}

/// A raw line of a file: its text and, when tabs were expanded, where each
/// source byte landed.
struct RawLine {
    text: String,
    map: Option<Vec<usize>>,
}

impl RawLine {
    /// A byte offset of the source line as an offset into `text`.
    fn offset(&self, source: usize) -> usize {
        let offset = match &self.map {
            Some(map) => map.get(source).copied().unwrap_or(self.text.len()),
            None => source.min(self.text.len()),
        };
        offset.min(self.text.len())
    }
}

fn split_lines(bytes: &[u8]) -> Vec<RawLine> {
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    if bytes.ends_with(b"\n") {
        lines.pop();
    }
    if bytes.is_empty() {
        lines.clear();
    }
    lines.into_iter().map(raw_line).collect()
}

fn raw_line(bytes: &[u8]) -> RawLine {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes);
    if !text.contains('\t') {
        return RawLine {
            text: text.into_owned(),
            map: None,
        };
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut map = Vec::with_capacity(text.len() + 1);
    let mut column = 0;
    for ch in text.chars() {
        map.extend(std::iter::repeat_n(out.len(), ch.len_utf8()));
        if ch == '\t' {
            let width = TAB_WIDTH - column % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', width));
            column += width;
        } else {
            out.push(ch);
            column += 1;
        }
    }
    map.push(out.len());
    RawLine {
        text: out,
        map: Some(map),
    }
}

fn floor_boundary(text: &str, mut ix: usize) -> usize {
    while ix > 0 && !text.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

fn ceil_boundary(text: &str, mut ix: usize) -> usize {
    while ix < text.len() && !text.is_char_boundary(ix) {
        ix += 1;
    }
    ix
}

fn build(
    request: &FileRequest,
    old_raw: &[RawLine],
    new_raw: &[RawLine],
    report: &Report,
) -> StructuralFile {
    let lines = |raw: &[RawLine]| -> Vec<Line> {
        raw.iter()
            .map(|line| Line {
                text: line.text.clone(),
                changed: false,
                novel: Vec::new(),
            })
            .collect()
    };
    let mut old = lines(old_raw);
    let mut new = lines(new_raw);
    let outcome = match report.status.as_str() {
        "created" => Outcome::Created,
        "deleted" => Outcome::Deleted,
        "unchanged" => Outcome::Unchanged,
        _ => Outcome::Changed,
    };
    // Whole files arrive without chunks: every line is the change.
    match outcome {
        Outcome::Created => new.iter_mut().for_each(|line| line.changed = true),
        Outcome::Deleted => old.iter_mut().for_each(|line| line.changed = true),
        _ => {}
    }
    let mut pairs = Vec::new();
    let mark = |lines: &mut [Line], raw: &[RawLine], side: &ReportSide| {
        let (Some(line), Some(raw)) = (lines.get_mut(side.line_number), raw.get(side.line_number))
        else {
            return;
        };
        line.changed = true;
        for change in &side.changes {
            let start = floor_boundary(&line.text, raw.offset(change.start));
            let end = ceil_boundary(&line.text, raw.offset(change.end));
            if start < end {
                line.novel.push(start..end);
            }
        }
    };
    for entry in report.chunks.iter().flatten() {
        if let Some(lhs) = &entry.lhs {
            mark(&mut old, old_raw, lhs);
        }
        if let Some(rhs) = &entry.rhs {
            mark(&mut new, new_raw, rhs);
        }
        if let (Some(lhs), Some(rhs)) = (&entry.lhs, &entry.rhs)
            && lhs.line_number < old.len()
            && rhs.line_number < new.len()
        {
            pairs.push((lhs.line_number, rhs.line_number));
        }
    }
    for line in old.iter_mut().chain(new.iter_mut()) {
        line.novel.sort_by_key(|range| range.start);
        line.novel = emphasis(&line.text, std::mem::take(&mut line.novel));
    }
    let (rows, blocks) = align(&old, &new, &pairs);
    // Blank lines hold no tokens, so difftastic never marks them; inside a
    // block of changed lines they belong to the change, as in a line diff.
    for (old_block, new_block) in blocks {
        for (lines, block) in [(&mut old, old_block), (&mut new, new_block)] {
            let block = &mut lines[block];
            if block.iter().any(|line| line.changed) {
                block
                    .iter_mut()
                    .filter(|line| line.text.trim().is_empty())
                    .for_each(|line| line.changed = true);
            }
        }
    }
    StructuralFile {
        path: request.path.clone(),
        old_path: request.old_path.clone(),
        language: report.language.clone(),
        outcome,
        old,
        new,
        rows,
    }
}

/// Novel token ranges as runs to emphasize: tokens separated only by
/// whitespace form one run, and a line that is novel throughout gets none —
/// its line background already says so, as GitHub does for added lines.
fn emphasis(text: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last)
                if text[last.end.min(range.start)..range.start]
                    .trim()
                    .is_empty() =>
            {
                last.end = last.end.max(range.end)
            }
            _ => merged.push(range),
        }
    }
    let content = text.trim();
    let start = text.len() - text.trim_start().len();
    if let [only] = merged.as_slice()
        && only.start <= start
        && only.end >= start + content.len()
    {
        return Vec::new();
    }
    merged
}

/// What lines are compared by when lining up the sides: unchanged lines by
/// their text without whitespace (difftastic ignores layout), changed lines
/// never match anything.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key {
    Text(String),
    Old(usize),
    New(usize),
}

fn keys(lines: &[Line], changed: fn(usize) -> Key) -> Vec<Key> {
    lines
        .iter()
        .enumerate()
        .map(|(ix, line)| {
            if line.changed {
                changed(ix)
            } else {
                Key::Text(line.text.split_whitespace().collect())
            }
        })
        .collect()
}

/// Lines up both sides into rows.
///
/// Lines difftastic paired (`pairs`, old ↔ new) anchor the alignment; the
/// longest crossing-free subset of them is kept. Between anchors, unchanged
/// lines are matched by a line diff of their text, so a blank line inside an
/// inserted block (difftastic marks no tokens there) is not mistaken for an
/// old one. What lies between matches is a change block: old and new lines
/// share its rows, old on the left, new on the right.
/// Also returns the change blocks, as (old lines, new lines).
fn align(old: &[Line], new: &[Line], pairs: &[(usize, usize)]) -> (Vec<Row>, Vec<Block>) {
    let old_keys = keys(old, Key::Old);
    let new_keys = keys(new, Key::New);
    let mut rows = Vec::with_capacity(old.len().max(new.len()));
    let mut blocks = Vec::new();
    let (mut i, mut j) = (0, 0);
    for (anchor_old, anchor_new) in monotonic(pairs) {
        if anchor_old < i || anchor_new < j {
            continue;
        }
        fill(
            &mut rows,
            &mut blocks,
            i..anchor_old,
            j..anchor_new,
            &old_keys,
            &new_keys,
        );
        rows.push(Row {
            old: Some(anchor_old),
            new: Some(anchor_new),
        });
        (i, j) = (anchor_old + 1, anchor_new + 1);
    }
    fill(
        &mut rows,
        &mut blocks,
        i..old.len(),
        j..new.len(),
        &old_keys,
        &new_keys,
    );
    (rows, blocks)
}

type Block = (Range<usize>, Range<usize>);

fn fill(
    rows: &mut Vec<Row>,
    blocks: &mut Vec<Block>,
    old: Range<usize>,
    new: Range<usize>,
    old_keys: &[Key],
    new_keys: &[Key],
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let ops = similar::capture_diff_slices_deadline(
        similar::Algorithm::Myers,
        &old_keys[old.clone()],
        &new_keys[new.clone()],
        Some(deadline),
    );
    let mut block: Block = (old.start..old.start, new.start..new.start);
    let mut flush = |rows: &mut Vec<Row>, block: &Block| {
        let (a, b) = (block.0.len(), block.1.len());
        if a + b > 0 {
            blocks.push(block.clone());
        }
        for k in 0..a.max(b) {
            rows.push(Row {
                old: (k < a).then_some(block.0.start + k),
                new: (k < b).then_some(block.1.start + k),
            });
        }
    };
    for op in ops {
        let (tag, o, n) = op.as_tag_tuple();
        let (o, n) = (
            old.start + o.start..old.start + o.end,
            new.start + n.start..new.start + n.end,
        );
        if tag == similar::DiffTag::Equal {
            flush(rows, &block);
            rows.extend(o.clone().zip(n.clone()).map(|(old, new)| Row {
                old: Some(old),
                new: Some(new),
            }));
            block = (o.end..o.end, n.end..n.end);
        } else {
            if block.0.is_empty() && block.1.is_empty() {
                block = (o.start..o.start, n.start..n.start);
            }
            block.0.end = block.0.end.max(o.end);
            block.1.end = block.1.end.max(n.end);
        }
    }
    flush(rows, &block);
}

/// The longest subset of `pairs` increasing on both sides.
fn monotonic(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted = pairs.to_vec();
    // Equal old lines sort by descending new line, so at most one of them
    // fits a strictly increasing run.
    sorted.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    sorted.dedup();
    // Patience sorting: `tails[k]` ends the best run of length k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut previous: Vec<Option<usize>> = vec![None; sorted.len()];
    for (ix, &(_, new)) in sorted.iter().enumerate() {
        let position = tails.partition_point(|&tail| sorted[tail].1 < new);
        previous[ix] = position.checked_sub(1).map(|p| tails[p]);
        if position == tails.len() {
            tails.push(ix);
        } else {
            tails[position] = ix;
        }
    }
    let mut chain = Vec::with_capacity(tails.len());
    let mut cursor = tails.last().copied();
    while let Some(ix) = cursor {
        chain.push(sorted[ix]);
        cursor = previous[ix];
    }
    chain.reverse();
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(path: &str) -> FileRequest {
        FileRequest {
            path: path.into(),
            old_path: None,
            added: false,
            deleted: false,
        }
    }

    fn rows(file: &StructuralFile) -> Vec<(Option<usize>, Option<usize>)> {
        file.rows.iter().map(|row| (row.old, row.new)).collect()
    }

    const OLD: &str = "fn main() {\n    let x = foo(1, 2);\n    println!(\"{}\", x);\n    // comment\n    let y = 3;\n}\n";
    const NEW: &str = "fn main() {\n    let x = foo(1,\n                2, 3);\n    println!(\"{} {}\", x, \"new\");\n    let y = 3;\n    let z = y + 1;\n}\n";
    /// `difft --display json` for OLD → NEW (0.67), reduced to the fields used.
    const REPORT: &str = r#"{"chunks":[[
        {"rhs":{"line_number":2,"changes":[{"start":17,"end":18},{"start":19,"end":20}]}},
        {"rhs":{"line_number":5,"changes":[{"start":4,"end":7},{"start":8,"end":9},
            {"start":10,"end":11},{"start":12,"end":13},{"start":14,"end":15},
            {"start":16,"end":17},{"start":17,"end":18}]}},
        {"lhs":{"line_number":2,"changes":[{"start":13,"end":17}]},
         "rhs":{"line_number":3,"changes":[{"start":13,"end":20},{"start":25,"end":30}]}},
        {"lhs":{"line_number":3,"changes":[{"start":4,"end":14}]}}
    ]],"language":"Rust","path":"b.rs","status":"changed"}"#;

    #[test]
    fn aligns_paired_changed_and_unchanged_lines() {
        let report = parse_report(REPORT.as_bytes()).unwrap();
        let file = build(
            &request("src/main.rs"),
            &split_lines(OLD.as_bytes()),
            &split_lines(NEW.as_bytes()),
            &report,
        );
        assert_eq!(file.language.as_deref(), Some("Rust"));
        assert_eq!(file.outcome, Outcome::Changed);
        assert_eq!(
            rows(&file),
            vec![
                (Some(0), Some(0)),
                (Some(1), Some(1)),
                (None, Some(2)),
                (Some(2), Some(3)),
                (Some(3), None),
                (Some(4), Some(4)),
                (None, Some(5)),
                (Some(5), Some(6)),
            ]
        );
        assert_eq!((file.additions(), file.deletions()), (3, 2));
        assert!(!file.old[1].changed && !file.new[1].changed);
        // `,` and `3` are one run: only a space separates them.
        assert_eq!(file.new[2].novel, vec![17..20]);
        // A wholly new line is marked by its background alone.
        assert!(file.new[5].changed && file.new[5].novel.is_empty());
        assert_eq!(&file.new[3].text[25..30], "\"new\"");
        // The single unchanged row between two changes stays visible.
        assert_eq!(file.hunks(0), vec![2..7]);
        assert_eq!(file.hunks(3), vec![0..8]);
    }

    #[test]
    fn whole_file_changes_and_unchanged_files() {
        let created = build(
            &request("new.rs"),
            &[],
            &split_lines(b"a\nb\n"),
            &parse_report(br#"{"language":"Rust","path":"new.rs","status":"created"}"#).unwrap(),
        );
        assert_eq!(created.outcome, Outcome::Created);
        assert_eq!(rows(&created), vec![(None, Some(0)), (None, Some(1))]);
        assert_eq!(created.additions(), 2);

        let unchanged = build(
            &request("a.rs"),
            &split_lines(b"x\n  y\n"),
            &split_lines(b"x\ny\n"),
            &Report::unchanged(),
        );
        assert_eq!(unchanged.outcome, Outcome::Unchanged);
        assert!(unchanged.hunks(3).is_empty());
        assert_eq!(unchanged.rows.len(), 2);
    }

    #[test]
    fn tabs_expand_and_ranges_follow_them() {
        let report = parse_report(
            br#"[{"chunks":[[{"lhs":{"line_number":0,"changes":[]},
                "rhs":{"line_number":0,"changes":[{"start":1,"end":4}]}}]],
                "status":"changed"}]"#,
        )
        .unwrap();
        let file = build(
            &request("Makefile"),
            &split_lines(b"\tfoo x\n"),
            &split_lines(b"\tbar x\r\n"),
            &report,
        );
        assert_eq!(file.new[0].text, "    bar x");
        assert_eq!(file.new[0].novel, vec![4..7]);
        assert!(file.old[0].changed && file.old[0].novel.is_empty());
        assert_eq!(rows(&file), vec![(Some(0), Some(0))]);
    }

    #[test]
    fn ranges_never_split_characters() {
        let report = parse_report(
            "{\"chunks\":[[{\"rhs\":{\"line_number\":0,\"changes\":[{\"start\":1,\"end\":3}]}}]],\"status\":\"changed\"}"
                .as_bytes(),
        )
        .unwrap();
        let file = build(
            &request("a.txt"),
            &[],
            &split_lines("мир\n".as_bytes()),
            &report,
        );
        assert_eq!(file.new[0].novel, vec![0..4]);
    }

    #[test]
    fn blank_lines_inside_an_insertion_stay_with_it() {
        // Two blocks inserted, each followed by a blank line difftastic
        // cannot mark (it holds no tokens).
        let old = "[a]\nx = 1\n\n[b]\ny = 2\n";
        let new = "[a]\nx = 1\n\n[n]\nz = 3\n\n[b]\ny = 2\n";
        let report = parse_report(
            br#"{"chunks":[[
                {"rhs":{"line_number":3,"changes":[{"start":0,"end":3}]}},
                {"rhs":{"line_number":4,"changes":[{"start":0,"end":5}]}}
            ]],"status":"changed"}"#,
        )
        .unwrap();
        let file = build(
            &request("a.toml"),
            &split_lines(old.as_bytes()),
            &split_lines(new.as_bytes()),
            &report,
        );
        assert_eq!(
            rows(&file),
            vec![
                (Some(0), Some(0)),
                (Some(1), Some(1)),
                (Some(2), Some(2)),
                (None, Some(3)),
                (None, Some(4)),
                (None, Some(5)),
                (Some(3), Some(6)),
                (Some(4), Some(7)),
            ]
        );
        // The blank line closing the insertion is part of it.
        assert!(file.new[5].changed && !file.new[2].changed);
        assert_eq!(file.additions(), 3);
    }

    #[test]
    fn crossing_pairs_keep_the_longest_monotonic_chain() {
        assert_eq!(
            monotonic(&[(0, 5), (1, 1), (2, 2), (3, 0), (4, 4)]),
            vec![(1, 1), (2, 2), (4, 4)]
        );
        assert_eq!(monotonic(&[(1, 1), (1, 2)]), vec![(1, 1)]);
    }

    #[test]
    fn parses_cat_file_batch_output() {
        let out = b"abc blob 3\nfoo\nHEAD:gone missing\ndef blob 0\n\n";
        assert_eq!(
            parse_batch(out),
            vec![
                Content::Bytes(b"foo".to_vec()),
                Content::Absent,
                Content::Bytes(Vec::new())
            ]
        );
    }
}
