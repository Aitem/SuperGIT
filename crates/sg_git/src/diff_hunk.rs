//! Select one hunk while retaining the file headers of a unified patch.

pub fn hunk_patch(patch: &str, original: bool, line: usize) -> Option<String> {
    let first = patch.find("\n@@ ")? + 1;
    let headers = &patch[..first];
    let mut starts = vec![first];
    starts.extend(
        patch[first..]
            .match_indices("\n@@ ")
            .map(|(offset, _)| first + offset + 1),
    );
    starts.push(patch.len());
    for range in starts.windows(2) {
        let hunk = &patch[range[0]..range[1]];
        let header = hunk.lines().next()?;
        let field = header
            .split_whitespace()
            .nth(if original { 1 } else { 2 })?;
        let (start, count) = field
            .get(1..)?
            .split_once(',')
            .unwrap_or((&field[1..], "1"));
        let start: usize = start.parse().ok()?;
        let count: usize = count.parse().ok()?;
        if count > 0 && line >= start && line < start.saturating_add(count) {
            return Some(format!("{headers}{hunk}"));
        }
    }
    None
}
