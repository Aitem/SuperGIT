//! Lanes for the History tab's branch graph, like `git log --graph`: each
//! commit sits in a column (lane); lines run down from a commit to its
//! parents, merge commits fan out to one lane per parent, and lanes waiting
//! for the same commit converge into it.
//!
//! Commits must come children first (`git log --date-order`). Parents that
//! are not in the list keep their lane running to the bottom.

/// Where a segment runs within one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// A lane passing by the row's commit: top to bottom.
    Pass,
    /// Into the commit: lane `from` at the top to the commit at the middle.
    In,
    /// Out of the commit: the commit at the middle to lane `to` at the bottom.
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeKind,
    pub from: usize,
    pub to: usize,
    /// Index into the graph palette, stable along a line.
    pub color: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    /// The commit's lane.
    pub lane: usize,
    pub color: usize,
    pub edges: Vec<Edge>,
}

struct Lane<'a> {
    /// The commit this lane runs down to.
    waits_for: &'a str,
    color: usize,
}

/// One row per commit (sha, parents), in order, and the number of lanes the
/// widest row uses.
pub fn layout<'a>(
    commits: impl IntoIterator<Item = (&'a str, &'a [String])>,
) -> (Vec<GraphRow>, usize) {
    let mut lanes: Vec<Option<Lane<'a>>> = Vec::new();
    let mut next_color = 0;
    let mut new_color = || {
        next_color += 1;
        next_color - 1
    };
    let mut rows = Vec::new();
    let mut width = 0;
    for (sha, parents) in commits {
        let waiting: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, lane)| lane.as_ref().is_some_and(|l| l.waits_for == sha))
            .map(|(ix, _)| ix)
            .collect();
        let (lane, color) = match waiting.first() {
            Some(&ix) => (ix, lanes[ix].as_ref().map_or(0, |l| l.color)),
            None => (free_lane(&mut lanes), new_color()),
        };
        let mut edges: Vec<Edge> = lanes
            .iter()
            .enumerate()
            .filter_map(|(ix, l)| {
                let l = l.as_ref()?;
                let kind = if l.waits_for == sha {
                    EdgeKind::In
                } else {
                    EdgeKind::Pass
                };
                let to = if kind == EdgeKind::In { lane } else { ix };
                Some(Edge {
                    kind,
                    from: ix,
                    to,
                    color: l.color,
                })
            })
            .collect();
        for ix in waiting {
            lanes[ix] = None;
        }
        if let Some(first) = parents.first() {
            lanes[lane] = Some(Lane {
                waits_for: first,
                color,
            });
            edges.push(Edge {
                kind: EdgeKind::Out,
                from: lane,
                to: lane,
                color,
            });
        }
        for parent in parents.iter().skip(1) {
            let existing = lanes
                .iter()
                .position(|l| l.as_ref().is_some_and(|l| l.waits_for == parent));
            let (to, color) = match existing {
                Some(ix) => (ix, lanes[ix].as_ref().map_or(0, |l| l.color)),
                None => {
                    let ix = free_lane(&mut lanes);
                    let color = new_color();
                    lanes[ix] = Some(Lane {
                        waits_for: parent,
                        color,
                    });
                    (ix, color)
                }
            };
            edges.push(Edge {
                kind: EdgeKind::Out,
                from: lane,
                to,
                color,
            });
        }
        let used = edges
            .iter()
            .map(|e| e.from.max(e.to) + 1)
            .max()
            .unwrap_or(0)
            .max(lane + 1);
        width = width.max(used);
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(GraphRow { lane, color, edges });
    }
    (rows, width)
}

/// The leftmost free lane, appending one when all are taken.
fn free_lane(lanes: &mut Vec<Option<Lane<'_>>>) -> usize {
    match lanes.iter().position(Option::is_none) {
        Some(ix) => ix,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use EdgeKind::*;

    fn rows(commits: &[(&'static str, &[&str])]) -> (Vec<GraphRow>, usize) {
        let owned: Vec<(&str, Vec<String>)> = commits
            .iter()
            .map(|(sha, parents)| (*sha, parents.iter().map(|p| p.to_string()).collect()))
            .collect();
        layout(owned.iter().map(|(sha, p)| (*sha, p.as_slice())))
    }

    fn edge(kind: EdgeKind, from: usize, to: usize, color: usize) -> Edge {
        Edge {
            kind,
            from,
            to,
            color,
        }
    }

    #[test]
    fn linear_history_is_one_lane() {
        let (rows, width) = rows(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        assert_eq!(width, 1);
        assert!(rows.iter().all(|r| r.lane == 0 && r.color == 0));
        assert_eq!(rows[0].edges, vec![edge(Out, 0, 0, 0)]);
        assert_eq!(rows[1].edges, vec![edge(In, 0, 0, 0), edge(Out, 0, 0, 0)]);
        // The root commit has nowhere to go.
        assert_eq!(rows[2].edges, vec![edge(In, 0, 0, 0)]);
    }

    #[test]
    fn a_merged_branch_gets_its_own_lane() {
        // m merges f (feature) into b; both fork from a.
        let (rows, width) = rows(&[("m", &["b", "f"]), ("f", &["a"]), ("b", &["a"]), ("a", &[])]);
        assert_eq!(width, 2);
        // The merge fans out: first parent straight down, second to lane 1.
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[0].edges, vec![edge(Out, 0, 0, 0), edge(Out, 0, 1, 1)]);
        // The feature commit sits in lane 1 while lane 0 passes by.
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[1].color, 1);
        assert_eq!(
            rows[1].edges,
            vec![edge(Pass, 0, 0, 0), edge(In, 1, 1, 1), edge(Out, 1, 1, 1)]
        );
        // Both lanes wait for a and converge into it.
        assert_eq!(rows[3].lane, 0);
        assert_eq!(rows[3].edges, vec![edge(In, 0, 0, 0), edge(In, 1, 0, 1)]);
    }

    #[test]
    fn freed_lanes_are_reused() {
        // Two branches merged one after another into main.
        let (rows, width) = rows(&[
            ("m2", &["m1", "y"]),
            ("y", &["m1"]),
            ("m1", &["a", "x"]),
            ("x", &["a"]),
            ("a", &[]),
        ]);
        assert_eq!(width, 2);
        assert_eq!(
            rows.iter().map(|r| r.lane).collect::<Vec<_>>(),
            [0, 1, 0, 1, 0]
        );
    }
}
