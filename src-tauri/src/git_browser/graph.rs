//! Commit-graph lane layout for the history pane.
//!
//! Input: commits newest first, children before parents (the topo walk's
//! order), each with its parent ids. Output: per commit the lane (column)
//! its node sits in and the edges from that row to the next (`from` = lane
//! in this row, `to` = lane in the next row). The first parent keeps its
//! child's lane, every other parent takes the leftmost free lane, and a lane
//! whose commit turns up in another lane bends into it (a branch merging
//! back).

/// Lanes a graph may show before the rest is squashed onto the last one, so
/// a pathological history keeps the pane readable. Only the emitted lane
/// numbers are clamped; the bookkeeping keeps every lane exact.
pub const MAX_LANES: usize = 12;

/// A line from a lane in one row to a lane in the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Edge {
    pub from: u32,
    pub to: u32,
}

/// One commit's placement.
#[derive(Debug, PartialEq, Eq)]
pub struct Placement {
    pub lane: u32,
    pub edges: Vec<Edge>,
}

/// The whole layout: one [`Placement`] per input commit, in input order,
/// and the number of lanes shown (at most [`MAX_LANES`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Layout {
    pub rows: Vec<Placement>,
    pub width: u32,
}

/// Lane index as the `u32` the payload carries, squashed onto the last
/// lane at the cap.
fn shown(lane: usize) -> u32 {
    u32::try_from(lane.min(MAX_LANES - 1)).unwrap_or(u32::MAX)
}

/// Append the edge `from` → `to` as shown, unless an identical one is
/// already there (lanes past the cap all draw onto the last one).
fn push_edge(edges: &mut Vec<Edge>, from: usize, to: usize) {
    let edge = Edge {
        from: shown(from),
        to: shown(to),
    };
    if !edges.contains(&edge) {
        edges.push(edge);
    }
}

/// The leftmost free lane, else a new one.
fn free_lane(active: &[Option<&str>]) -> usize {
    active
        .iter()
        .position(Option::is_none)
        .unwrap_or(active.len())
}

/// Set `active[lane]`, growing the vector by one when `lane` is its length
/// (`free_lane` never returns more).
fn put<'a>(active: &mut Vec<Option<&'a str>>, lane: usize, id: &'a str) {
    if lane == active.len() {
        active.push(Some(id));
    } else {
        active[lane] = Some(id);
    }
}

/// Lay out `commits` (`(id, parent ids)`, newest first) onto lanes.
///
/// A commit id waits in at most one lane at a time: an id only enters
/// `active` when [`free_lane`]/[`put`] place it in a free lane after a
/// lookup found it absent, and `active` grows past [`MAX_LANES`] rather
/// than reusing an occupied lane. So a commit never has to gather lines
/// from several lanes into its node — every merge-in bend is drawn by the
/// child's own edge (`from` its lane `to` the lane its parent already waits
/// in). Lanes past the cap are clamped only when emitted ([`shown`]).
pub fn layout<'a, I, P>(commits: I) -> Layout
where
    I: IntoIterator<Item = (&'a str, P)>,
    P: IntoIterator<Item = &'a str>,
{
    // active[lane] = the commit id the line in that lane is waiting for.
    let mut active: Vec<Option<&'a str>> = Vec::new();
    let mut rows: Vec<Placement> = Vec::new();
    let mut width = 0usize;
    for (id, parents) in commits {
        let lane = active
            .iter()
            .position(|a| *a == Some(id))
            .unwrap_or_else(|| {
                let lane = free_lane(&active);
                put(&mut active, lane, id);
                lane
            });
        let mut edges = Vec::new();
        let mut parents = parents.into_iter();
        match parents.next() {
            None => active[lane] = None,
            Some(first) => {
                // The first parent already waiting in another lane: bend into
                // it. Otherwise it continues this lane.
                if let Some(k) = active.iter().position(|a| *a == Some(first)) {
                    active[lane] = None;
                    push_edge(&mut edges, lane, k);
                } else {
                    active[lane] = Some(first);
                    push_edge(&mut edges, lane, lane);
                }
            }
        }
        // Lanes a further parent claims in this row: they start here, so
        // they do not pass through.
        let mut claimed = Vec::new();
        for parent in parents {
            let k = active
                .iter()
                .position(|a| *a == Some(parent))
                .unwrap_or_else(|| {
                    let k = free_lane(&active);
                    put(&mut active, k, parent);
                    claimed.push(k);
                    k
                });
            push_edge(&mut edges, lane, k);
        }
        for (j, waiting) in active.iter().enumerate() {
            if j != lane && waiting.is_some() && !claimed.contains(&j) {
                push_edge(&mut edges, j, j);
            }
        }
        while active.last() == Some(&None) {
            active.pop();
        }
        width = width.max(lane + 1).max(active.len());
        rows.push(Placement {
            lane: shown(lane),
            edges,
        });
    }
    Layout {
        rows,
        width: u32::try_from(width.min(MAX_LANES)).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::{layout, Edge, Layout, MAX_LANES};

    fn run(commits: &[(&str, &[&str])]) -> Layout {
        layout(
            commits
                .iter()
                .map(|(id, parents)| (*id, parents.iter().copied())),
        )
    }

    fn edges(pairs: &[(u32, u32)]) -> Vec<Edge> {
        pairs.iter().map(|&(from, to)| Edge { from, to }).collect()
    }

    /// The design mockup's history: `HEAD` three commits ahead of a merge
    /// commit, a side branch merged back, a remote branch forked off the
    /// release, a second merge, then a linear tail to the root.
    const MOCK: &[(&str, &[&str])] = &[
        ("a1f9c2e", &["b72e01d"]),
        ("b72e01d", &["c3d4a55"]),
        ("c3d4a55", &["d91be70"]),
        ("d91be70", &["e7a2c11", "f0c3b8d"]),
        ("f0c3b8d", &["e7a2c11"]),
        ("e7a2c11", &["0a1b2c3"]),
        ("9f8e7d6", &["8e7d6c5"]),
        ("8e7d6c5", &["0a1b2c3"]),
        ("0a1b2c3", &["1b2c3d4"]),
        ("1b2c3d4", &["2c3d4e5", "3d4e5f6"]),
        ("3d4e5f6", &["4e5f6a7"]),
        ("2c3d4e5", &["5f6a7b8"]),
        ("4e5f6a7", &["5f6a7b8"]),
        ("5f6a7b8", &["6a7b8c9"]),
        ("6a7b8c9", &["7b8c9d0"]),
        ("7b8c9d0", &["8c9d0e1"]),
        ("8c9d0e1", &["9d0e1f2"]),
        ("9d0e1f2", &["0e1f2a3"]),
        ("0e1f2a3", &[]),
    ];

    #[test]
    fn mock_history_fits_two_lanes() {
        let out = run(MOCK);
        assert_eq!(out.width, 2);
        assert_eq!(out.rows.len(), MOCK.len());
        let lanes: Vec<u32> = out.rows.iter().map(|r| r.lane).collect();
        assert_eq!(
            lanes,
            [0, 0, 0, 0, 1, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn merges_branch_out_to_the_next_free_lane() {
        let out = run(MOCK);
        // d91be70 (first merge): first parent straight down, second parent
        // out to lane 1.
        assert_eq!(out.rows[3].edges, edges(&[(0, 0), (0, 1)]));
        // 1b2c3d4 (second merge): same shape.
        assert_eq!(out.rows[9].edges, edges(&[(0, 0), (0, 1)]));
    }

    #[test]
    fn side_branches_bend_back_into_their_first_parents_lane() {
        let out = run(MOCK);
        // f0c3b8d's parent e7a2c11 already waits in lane 0: bend 1 → 0,
        // while lane 0 passes through.
        assert_eq!(out.rows[4].edges, edges(&[(1, 0), (0, 0)]));
        // The remote branch 9f8e7d6 → 8e7d6c5 runs down lane 1, then bends
        // into 0a1b2c3 in lane 0.
        assert_eq!(out.rows[6].edges, edges(&[(1, 1), (0, 0)]));
        assert_eq!(out.rows[7].edges, edges(&[(1, 0), (0, 0)]));
        // 3d4e5f6 / 2c3d4e5 / 4e5f6a7: two parallel lanes rejoining at
        // 5f6a7b8.
        assert_eq!(out.rows[10].edges, edges(&[(1, 1), (0, 0)]));
        assert_eq!(out.rows[11].edges, edges(&[(0, 0), (1, 1)]));
        assert_eq!(out.rows[12].edges, edges(&[(1, 0), (0, 0)]));
        // The root ends its lane.
        assert_eq!(out.rows[18].edges, edges(&[]));
    }

    #[test]
    fn a_second_parent_already_waiting_gets_an_edge_into_its_lane() {
        // x merges y; y's second parent c already waits in lane 0 (x's first
        // parent), so y's merge-in edge goes 1 → 0 instead of a new lane.
        let out = run(&[
            ("x", &["c", "y"]),
            ("y", &["d", "c"]),
            ("d", &["c"]),
            ("c", &[]),
        ]);
        assert_eq!(out.rows[0].edges, edges(&[(0, 0), (0, 1)]));
        assert_eq!(out.rows[1].lane, 1);
        assert_eq!(out.rows[1].edges, edges(&[(1, 1), (1, 0), (0, 0)]));
        // d's first parent c waits in lane 0: bend.
        assert_eq!(out.rows[2].edges, edges(&[(1, 0), (0, 0)]));
        assert_eq!(out.rows[3].lane, 0);
        assert_eq!(out.width, 2);
    }

    #[test]
    fn unrelated_tips_take_new_lanes_and_bend_into_their_shared_base() {
        let out = run(&[("b", &["m"]), ("c", &["m"]), ("m", &[])]);
        assert_eq!(out.rows[0].lane, 0);
        assert_eq!(out.rows[1].lane, 1);
        assert_eq!(out.rows[1].edges, edges(&[(1, 0), (0, 0)]));
        assert_eq!(out.rows[2].lane, 0);
        assert_eq!(out.width, 2);
    }

    #[test]
    fn a_lane_claimed_by_a_second_parent_does_not_pass_through_its_first_row() {
        // a's second parent lands in a fresh lane 1: only the merge edge
        // reaches it, no 1 → 1 pass-through from a row it did not exist in.
        let out = run(&[("a", &["p", "q"]), ("q", &["p"]), ("p", &[])]);
        assert_eq!(out.rows[0].edges, edges(&[(0, 0), (0, 1)]));
        assert_eq!(out.rows[1].edges, edges(&[(1, 0), (0, 0)]));
    }

    #[test]
    fn lanes_past_the_cap_squash_onto_the_last_one() {
        // MAX_LANES + 3 unrelated root commits, each a tip waiting for its
        // own parent: lanes fill up to the cap, the rest reuse the last lane.
        let ids: Vec<(String, String)> = (0..MAX_LANES + 3)
            .map(|i| (format!("tip{i}"), format!("base{i}")))
            .collect();
        let out = layout(
            ids.iter()
                .map(|(tip, base)| (tip.as_str(), std::iter::once(base.as_str()))),
        );
        assert_eq!(out.width as usize, MAX_LANES);
        let lanes: Vec<usize> = out.rows.iter().map(|r| r.lane as usize).collect();
        assert_eq!(lanes[..MAX_LANES], (0..MAX_LANES).collect::<Vec<_>>()[..]);
        assert!(lanes[MAX_LANES..].iter().all(|&l| l == MAX_LANES - 1));
    }

    #[test]
    fn lanes_past_the_cap_keep_every_parent_edge() {
        // MAX_LANES + 1 open branches tip_i → base_i; base_last, sitting in
        // a lane past the cap, merges x and y; then every other base ends,
        // then x and y.
        let n = MAX_LANES + 1;
        let tips: Vec<(String, String)> = (0..n)
            .map(|i| (format!("tip{i}"), format!("base{i}")))
            .collect();
        let mut commits: Vec<(&str, Vec<&str>)> = tips
            .iter()
            .map(|(tip, base)| (tip.as_str(), vec![base.as_str()]))
            .collect();
        commits.push((tips[n - 1].1.as_str(), vec!["x", "y"]));
        commits.extend(
            tips[..n - 1]
                .iter()
                .map(|(_, base)| (base.as_str(), vec![])),
        );
        commits.push(("x", vec![]));
        commits.push(("y", vec![]));
        let out = layout(
            commits
                .iter()
                .map(|(id, parents)| (*id, parents.iter().copied())),
        );
        assert_eq!(out.width as usize, MAX_LANES);
        let lane_of = |id: &str| {
            let row = commits.iter().position(|(c, _)| *c == id).expect("listed");
            out.rows[row].lane
        };
        for ((id, parents), row) in commits.iter().zip(&out.rows) {
            for parent in parents {
                let edge = Edge {
                    from: row.lane,
                    to: lane_of(parent),
                };
                assert!(row.edges.contains(&edge), "{id} → {parent}: {row:?}");
            }
            for (i, edge) in row.edges.iter().enumerate() {
                assert!(!row.edges[i + 1..].contains(edge), "{id}: {row:?}");
            }
        }
    }

    #[test]
    fn empty_history_has_no_lanes() {
        let out = run(&[]);
        assert_eq!(out.rows, []);
        assert_eq!(out.width, 0);
    }
}
