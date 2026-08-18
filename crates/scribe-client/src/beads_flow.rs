//! Pure layered-DAG layout for the Beads board Flow view.
//!
//! Coordinates are physical pixels at the requested text scale. The graph band
//! keeps its fixed 139px reservation while node and gap dimensions scale.

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};

use scribe_common::protocol::{BeadsEpicGraph, BeadsGraphEdge};

const MAX_FLOW_NODES: usize = 200;

fn scalar(value: usize) -> f64 {
    u32::try_from(value).map_or(f64::from(u32::MAX), f64::from)
}

/// Geometry copied from the normative A3 Flow mock as formulas, not pitches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowMetrics {
    pub node_width: f64,
    pub node_height: f64,
    pub gutter: f64,
    pub row_gap: f64,
    pub graph_height: f64,
    pub left_padding: f64,
}

impl FlowMetrics {
    pub const fn standard() -> Self {
        Self {
            node_width: 214.0,
            node_height: 24.0,
            gutter: 28.0,
            row_gap: 10.0,
            graph_height: 139.0,
            left_padding: 30.0,
        }
    }

    #[must_use]
    pub fn rank_pitch(self, text_scale: f64) -> f64 {
        (self.node_width + self.gutter) * text_scale
    }

    #[must_use]
    pub fn row_pitch(self, text_scale: f64) -> f64 {
        (self.node_height + self.row_gap) * text_scale
    }

    #[must_use]
    pub fn rows_that_fit(self, text_scale: f64) -> Option<usize> {
        if !text_scale.is_finite() || text_scale <= 0.0 {
            return None;
        }
        Some(
            (1..=MAX_FLOW_NODES)
                .take_while(|&rows| {
                    let total = scalar(rows) * self.node_height * text_scale
                        + scalar(rows.saturating_sub(1)) * self.row_gap * text_scale;
                    total <= self.graph_height
                })
                .count(),
        )
    }
}

impl Default for FlowMetrics {
    fn default() -> Self {
        Self::standard()
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FlowLayoutError {
    #[error("Flow graph is empty")]
    EmptyGraph,
    #[error("Flow graph has {nodes} nodes; the limit is {limit}")]
    TooLarge { nodes: usize, limit: usize },
    #[error("duplicate Flow graph node id: {id}")]
    DuplicateNode { id: String },
    #[error("Flow graph edge {edge_index} names unknown node {id}")]
    UnknownNode { edge_index: usize, id: String },
    #[error("Flow graph contains a dependency cycle")]
    Cycle,
    #[error("invalid Flow text scale: {0}")]
    InvalidTextScale(f64),
    #[error("Flow rank {rank} has {nodes} nodes but only {capacity} fit")]
    RankTooWide { rank: usize, nodes: usize, capacity: usize },
}

/// One real graph node after ranking and barycenter ordering.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowNodeLayout {
    pub issue_index: usize,
    pub rank: usize,
    pub order: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Virtual node inserted into each intermediate rank crossed by a skip edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowDummyNode {
    pub edge_index: usize,
    pub rank: usize,
    pub order: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WireAxis {
    Horizontal,
    Vertical,
}

/// Paint precedence when multiple edge runs share the same physical rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WireClass {
    Dimmed,
    Base,
    Traced,
}

/// One pre-union interval owned by a graph edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeWireRun {
    pub edge_index: usize,
    pub axis: WireAxis,
    pub offset: f64,
    pub start: f64,
    pub end: f64,
}

/// One non-overlapping paint interval keyed by rail and colour class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WireSegment {
    pub axis: WireAxis,
    pub offset: f64,
    pub start: f64,
    pub end: f64,
    pub class: WireClass,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlowLayout {
    pub nodes: Vec<FlowNodeLayout>,
    pub dummy_nodes: Vec<FlowDummyNode>,
    pub wire_runs: Vec<EdgeWireRun>,
    pub rank_count: usize,
    pub width: f64,
    pub height: f64,
}

impl FlowLayout {
    /// Resolve the current trace state into non-overlapping paint segments.
    pub fn wire_segments(&self, class_for_edge: impl Fn(usize) -> WireClass) -> Vec<WireSegment> {
        union_wire_runs(&self.wire_runs, class_for_edge)
    }
}

#[derive(Debug)]
struct IndexedGraph {
    edges: Vec<(usize, usize)>,
    successors: Vec<Vec<usize>>,
}

impl IndexedGraph {
    fn new(graph: &BeadsEpicGraph) -> Result<Self, FlowLayoutError> {
        if graph.nodes.is_empty() {
            return Err(FlowLayoutError::EmptyGraph);
        }
        if graph.nodes.len() > MAX_FLOW_NODES {
            return Err(FlowLayoutError::TooLarge {
                nodes: graph.nodes.len(),
                limit: MAX_FLOW_NODES,
            });
        }
        let node_by_id = index_nodes(graph)?;
        let mut edges = Vec::with_capacity(graph.edges.len());
        let mut successors = vec![Vec::new(); graph.nodes.len()];
        for (edge_index, edge) in graph.edges.iter().enumerate() {
            let from = endpoint(&node_by_id, edge, edge_index, true)?;
            let to = endpoint(&node_by_id, edge, edge_index, false)?;
            edges.push((from, to));
            if let Some(outgoing) = successors.get_mut(from) {
                outgoing.push(to);
            }
        }
        Ok(Self { edges, successors })
    }
}

fn index_nodes(graph: &BeadsEpicGraph) -> Result<HashMap<String, usize>, FlowLayoutError> {
    let mut node_by_id = HashMap::with_capacity(graph.nodes.len());
    for (index, node) in graph.nodes.iter().enumerate() {
        if node_by_id.insert(node.id.clone(), index).is_some() {
            return Err(FlowLayoutError::DuplicateNode { id: node.id.clone() });
        }
    }
    Ok(node_by_id)
}

fn endpoint(
    node_by_id: &HashMap<String, usize>,
    edge: &BeadsGraphEdge,
    edge_index: usize,
    from: bool,
) -> Result<usize, FlowLayoutError> {
    let id = if from { &edge.from } else { &edge.to };
    node_by_id
        .get(id)
        .copied()
        .ok_or_else(|| FlowLayoutError::UnknownNode { edge_index, id: id.clone() })
}

/// Assign each node the maximum blocker distance from any root.
pub fn longest_path_ranks(graph: &BeadsEpicGraph) -> Result<Vec<usize>, FlowLayoutError> {
    let indexed = IndexedGraph::new(graph)?;
    longest_path_ranks_indexed(&indexed)
}

fn longest_path_ranks_indexed(indexed: &IndexedGraph) -> Result<Vec<usize>, FlowLayoutError> {
    let mut indegree = vec![0usize; indexed.successors.len()];
    for &(_, to) in &indexed.edges {
        if let Some(degree) = indegree.get_mut(to) {
            *degree += 1;
        }
    }
    let mut ready = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, &degree)| (degree == 0).then_some(index))
        .collect::<VecDeque<_>>();
    let mut ranks = vec![0usize; indegree.len()];
    let mut visited = 0usize;
    while let Some(from) = ready.pop_front() {
        visited += 1;
        let from_rank = ranks.get(from).copied().unwrap_or_default();
        let Some(successors) = indexed.successors.get(from) else {
            continue;
        };
        for &to in successors {
            if let Some(to_rank) = ranks.get_mut(to) {
                *to_rank = (*to_rank).max(from_rank + 1);
            }
            release_successor(to, &mut indegree, &mut ready);
        }
    }
    if visited != ranks.len() {
        return Err(FlowLayoutError::Cycle);
    }
    Ok(ranks)
}

fn release_successor(to: usize, indegree: &mut [usize], ready: &mut VecDeque<usize>) {
    let Some(degree) = indegree.get_mut(to) else {
        return;
    };
    *degree = degree.saturating_sub(1);
    if *degree == 0 {
        ready.push_back(to);
    }
}

#[derive(Debug, Clone, Copy)]
enum ExpandedKind {
    Issue(usize),
    Dummy { edge_index: usize },
}

#[derive(Debug)]
struct ExpandedNode {
    kind: ExpandedKind,
    rank: usize,
    stable_order: usize,
    predecessors: Vec<usize>,
}

struct ExpandedGraph {
    nodes: Vec<ExpandedNode>,
    by_rank: Vec<Vec<usize>>,
}

impl ExpandedGraph {
    fn new(indexed: &IndexedGraph, ranks: &[usize]) -> Self {
        let rank_count = ranks.iter().copied().max().map_or(0, |rank| rank + 1);
        let mut expanded = Self {
            nodes: Vec::with_capacity(ranks.len() + indexed.edges.len()),
            by_rank: vec![Vec::new(); rank_count],
        };
        for (issue_index, &rank) in ranks.iter().enumerate() {
            let index = expanded.nodes.len();
            expanded.nodes.push(ExpandedNode {
                kind: ExpandedKind::Issue(issue_index),
                rank,
                stable_order: index,
                predecessors: Vec::new(),
            });
            if let Some(bucket) = expanded.by_rank.get_mut(rank) {
                bucket.push(index);
            }
        }
        for (edge_index, &(from, to)) in indexed.edges.iter().enumerate() {
            expanded.add_edge(edge_index, from, to, ranks);
        }
        expanded
    }

    fn add_edge(&mut self, edge_index: usize, from: usize, to: usize, ranks: &[usize]) {
        let from_rank = ranks.get(from).copied().unwrap_or_default();
        let to_rank = ranks.get(to).copied().unwrap_or(from_rank);
        let mut previous = from;
        for (rank, bucket) in self.by_rank.iter_mut().enumerate().take(to_rank).skip(from_rank + 1)
        {
            let index = self.nodes.len();
            self.nodes.push(ExpandedNode {
                kind: ExpandedKind::Dummy { edge_index },
                rank,
                stable_order: index,
                predecessors: vec![previous],
            });
            bucket.push(index);
            previous = index;
        }
        if let Some(target) = self.nodes.get_mut(to) {
            target.predecessors.push(previous);
        }
    }

    fn barycenter_pass(&mut self) {
        for rank in 1..self.by_rank.len() {
            let (before, current_and_after) = self.by_rank.split_at_mut(rank);
            let (Some(previous), Some(current)) = (before.last(), current_and_after.first_mut())
            else {
                continue;
            };
            let positions = previous
                .iter()
                .enumerate()
                .map(|(position, &index)| (index, scalar(position)))
                .collect::<HashMap<_, _>>();
            current.sort_by(|&left, &right| compare_expanded(&self.nodes, &positions, left, right));
        }
    }
}

fn compare_expanded(
    nodes: &[ExpandedNode],
    positions: &HashMap<usize, f64>,
    left: usize,
    right: usize,
) -> Ordering {
    let (Some(left_node), Some(right_node)) = (nodes.get(left), nodes.get(right)) else {
        return left.cmp(&right);
    };
    barycenter(left_node, positions)
        .total_cmp(&barycenter(right_node, positions))
        .then_with(|| left_node.stable_order.cmp(&right_node.stable_order))
}

fn barycenter(node: &ExpandedNode, positions: &HashMap<usize, f64>) -> f64 {
    let (sum, count) = node
        .predecessors
        .iter()
        .filter_map(|index| positions.get(index))
        .fold((0.0, 0usize), |(sum, count), position| (sum + position, count + 1));
    if count == 0 { scalar(node.stable_order) } else { sum / scalar(count) }
}

/// Rank, order, place and route an admitted epic graph.
pub fn layout_flow(graph: &BeadsEpicGraph, text_scale: f64) -> Result<FlowLayout, FlowLayoutError> {
    let metrics = FlowMetrics::standard();
    let capacity =
        metrics.rows_that_fit(text_scale).ok_or(FlowLayoutError::InvalidTextScale(text_scale))?;
    let indexed = IndexedGraph::new(graph)?;
    let ranks = longest_path_ranks_indexed(&indexed)?;
    let mut expanded = ExpandedGraph::new(&indexed, &ranks);
    expanded.barycenter_pass();
    let placed = place_nodes(&expanded, metrics, text_scale, capacity)?;
    let wire_runs =
        WireRouter { metrics, text_scale }.emit(&indexed.edges, &ranks, &placed.positions);
    let rank_count = expanded.by_rank.len();
    Ok(FlowLayout {
        nodes: placed.nodes,
        dummy_nodes: placed.dummies,
        wire_runs,
        rank_count,
        width: metrics.left_padding
            + scalar(rank_count.saturating_sub(1)) * metrics.rank_pitch(text_scale)
            + metrics.node_width * text_scale,
        height: metrics.graph_height,
    })
}

struct PlacedNodes {
    nodes: Vec<FlowNodeLayout>,
    dummies: Vec<FlowDummyNode>,
    positions: Vec<(f64, f64)>,
}

#[derive(Clone, Copy)]
struct Placement {
    metrics: FlowMetrics,
    text_scale: f64,
    capacity: usize,
}

fn place_nodes(
    expanded: &ExpandedGraph,
    metrics: FlowMetrics,
    text_scale: f64,
    capacity: usize,
) -> Result<PlacedNodes, FlowLayoutError> {
    let issue_count =
        expanded.nodes.iter().filter(|node| matches!(node.kind, ExpandedKind::Issue(_))).count();
    let mut placed = PlacedNodes {
        nodes: Vec::with_capacity(issue_count),
        dummies: Vec::new(),
        positions: vec![(0.0, 0.0); issue_count],
    };
    let placement = Placement { metrics, text_scale, capacity };
    for (rank, expanded_rank) in expanded.by_rank.iter().enumerate() {
        place_rank(rank, expanded_rank, &expanded.nodes, placement, &mut placed)?;
    }
    Ok(placed)
}

fn place_rank(
    rank: usize,
    expanded_rank: &[usize],
    expanded: &[ExpandedNode],
    placement: Placement,
    placed: &mut PlacedNodes,
) -> Result<(), FlowLayoutError> {
    let real = expanded_rank
        .iter()
        .filter_map(|&index| {
            let node = expanded.get(index)?;
            let ExpandedKind::Issue(issue_index) = node.kind else {
                return None;
            };
            Some((index, issue_index))
        })
        .collect::<Vec<_>>();
    if real.len() > placement.capacity {
        return Err(FlowLayoutError::RankTooWide {
            rank,
            nodes: real.len(),
            capacity: placement.capacity,
        });
    }
    let tops = centered_row_tops(real.len(), placement.metrics, placement.text_scale);
    let x = placement.metrics.left_padding
        + scalar(rank) * placement.metrics.rank_pitch(placement.text_scale);
    for (order, ((_, issue_index), y)) in real.into_iter().zip(tops).enumerate() {
        if let Some(position) = placed.positions.get_mut(issue_index) {
            *position = (x, y);
        }
        placed.nodes.push(FlowNodeLayout {
            issue_index,
            rank,
            order,
            x,
            y,
            width: placement.metrics.node_width * placement.text_scale,
            height: placement.metrics.node_height * placement.text_scale,
        });
    }
    for (order, &index) in expanded_rank.iter().enumerate() {
        let Some(node) = expanded.get(index) else {
            continue;
        };
        if let ExpandedKind::Dummy { edge_index } = node.kind {
            placed.dummies.push(FlowDummyNode { edge_index, rank: node.rank, order });
        }
    }
    Ok(())
}

fn centered_row_tops(count: usize, metrics: FlowMetrics, text_scale: f64) -> Vec<f64> {
    if count == 0 {
        return Vec::new();
    }
    let node_height = metrics.node_height * text_scale;
    let row_gap = metrics.row_gap * text_scale;
    let total = scalar(count) * node_height + scalar(count.saturating_sub(1)) * row_gap;
    let first = (metrics.graph_height - total) / 2.0;
    (0..count).map(|row| first + scalar(row) * (node_height + row_gap)).collect()
}

#[derive(Clone, Copy)]
struct WireRouter {
    metrics: FlowMetrics,
    text_scale: f64,
}

impl WireRouter {
    fn emit(
        self,
        edges: &[(usize, usize)],
        ranks: &[usize],
        positions: &[(f64, f64)],
    ) -> Vec<EdgeWireRun> {
        let mut runs = Vec::new();
        let node_width = self.metrics.node_width * self.text_scale;
        let node_height = self.metrics.node_height * self.text_scale;
        for (edge_index, &(from, to)) in edges.iter().enumerate() {
            let (Some(&from_position), Some(&to_position)) =
                (positions.get(from), positions.get(to))
            else {
                continue;
            };
            let start = (from_position.0 + node_width, from_position.1 + node_height / 2.0);
            let end = (to_position.0, to_position.1 + node_height / 2.0);
            let adjacent =
                ranks.get(from).zip(ranks.get(to)).is_some_and(|(from, to)| *to == *from + 1);
            if adjacent {
                self.emit_adjacent(edge_index, start, end, &mut runs);
            } else {
                self.emit_skip(edge_index, start, end, &mut runs);
            }
        }
        runs
    }

    fn emit_adjacent(
        self,
        edge_index: usize,
        start: (f64, f64),
        end: (f64, f64),
        runs: &mut Vec<EdgeWireRun>,
    ) {
        if start.1.to_bits() == end.1.to_bits() {
            push_horizontal(runs, edge_index, start.1, start.0, end.0);
            return;
        }
        let middle = start.0 + self.metrics.gutter * self.text_scale / 2.0;
        push_horizontal(runs, edge_index, start.1, start.0, middle);
        push_vertical(runs, edge_index, middle, start.1, end.1);
        push_horizontal(runs, edge_index, end.1, middle, end.0);
    }

    fn emit_skip(
        self,
        edge_index: usize,
        start: (f64, f64),
        end: (f64, f64),
        runs: &mut Vec<EdgeWireRun>,
    ) {
        let lane = self.nearest_lane(f64::midpoint(start.1, end.1));
        let stub = 8.0 * self.text_scale;
        let left = start.0 + stub;
        let right = end.0 - stub;
        push_horizontal(runs, edge_index, start.1, start.0, left);
        push_vertical(runs, edge_index, left, start.1, lane);
        push_horizontal(runs, edge_index, lane, left, right);
        push_vertical(runs, edge_index, right, lane, end.1);
        push_horizontal(runs, edge_index, end.1, right, end.0);
    }

    fn nearest_lane(self, target: f64) -> f64 {
        let rows = self.metrics.rows_that_fit(self.text_scale).unwrap_or(1).max(1);
        let tops = centered_row_tops(rows, self.metrics, self.text_scale);
        let node_height = self.metrics.node_height * self.text_scale;
        let row_gap = self.metrics.row_gap * self.text_scale;
        tops.iter()
            .enumerate()
            .map(|(index, top)| {
                let gap =
                    if index + 1 == tops.len() { 3.0 * self.text_scale } else { row_gap / 2.0 };
                top + node_height + gap
            })
            .min_by(|left, right| (left - target).abs().total_cmp(&(right - target).abs()))
            .unwrap_or(self.metrics.graph_height / 2.0)
    }
}

fn push_horizontal(
    runs: &mut Vec<EdgeWireRun>,
    edge_index: usize,
    offset: f64,
    start: f64,
    end: f64,
) {
    push_run(runs, EdgeWireRun { edge_index, axis: WireAxis::Horizontal, offset, start, end });
}

fn push_vertical(
    runs: &mut Vec<EdgeWireRun>,
    edge_index: usize,
    offset: f64,
    start: f64,
    end: f64,
) {
    push_run(runs, EdgeWireRun { edge_index, axis: WireAxis::Vertical, offset, start, end });
}

fn push_run(runs: &mut Vec<EdgeWireRun>, mut run: EdgeWireRun) {
    if run.start > run.end {
        std::mem::swap(&mut run.start, &mut run.end);
    }
    if run.end - run.start > f64::EPSILON {
        runs.push(run);
    }
}

/// Union overlapping edge runs so translucent rails paint exactly once.
///
/// Higher classes win only on their covered sub-interval, which lets a traced
/// edge light half a shared gutter without brightening or replacing the rest.
pub fn union_wire_runs(
    runs: &[EdgeWireRun],
    class_for_edge: impl Fn(usize) -> WireClass,
) -> Vec<WireSegment> {
    let mut sorted = runs.to_vec();
    sorted.sort_by(|left, right| {
        left.axis
            .cmp(&right.axis)
            .then_with(|| left.offset.total_cmp(&right.offset))
            .then_with(|| left.start.total_cmp(&right.start))
            .then_with(|| left.end.total_cmp(&right.end))
    });
    let mut groups: Vec<Vec<EdgeWireRun>> = Vec::new();
    for run in sorted {
        if groups
            .last()
            .and_then(|group| group.first())
            .is_some_and(|first| same_track(*first, run))
        {
            if let Some(group) = groups.last_mut() {
                group.push(run);
            }
        } else {
            groups.push(vec![run]);
        }
    }
    let mut segments = Vec::new();
    for group in groups {
        union_track(&group, &class_for_edge, &mut segments);
    }
    segments
}

fn same_track(left: EdgeWireRun, right: EdgeWireRun) -> bool {
    left.axis == right.axis && left.offset.to_bits() == right.offset.to_bits()
}

fn union_track(
    runs: &[EdgeWireRun],
    class_for_edge: &impl Fn(usize) -> WireClass,
    output: &mut Vec<WireSegment>,
) {
    let Some(track) = runs.first() else {
        return;
    };
    let mut boundaries = runs.iter().flat_map(|run| [run.start, run.end]).collect::<Vec<_>>();
    boundaries.sort_by(f64::total_cmp);
    boundaries.dedup_by(|left, right| left.to_bits() == right.to_bits());
    for window in boundaries.windows(2) {
        let [start, end] = window else {
            continue;
        };
        let class = runs
            .iter()
            .filter(|run| run.start <= *start && run.end >= *end)
            .map(|run| class_for_edge(run.edge_index))
            .max();
        if let Some(class) = class {
            append_segment(
                output,
                WireSegment {
                    axis: track.axis,
                    offset: track.offset,
                    start: *start,
                    end: *end,
                    class,
                },
            );
        }
    }
}

fn append_segment(output: &mut Vec<WireSegment>, segment: WireSegment) {
    if let Some(last) = output.last_mut()
        && last.axis == segment.axis
        && last.offset.to_bits() == segment.offset.to_bits()
        && last.end.to_bits() == segment.start.to_bits()
        && last.class == segment.class
    {
        last.end = segment.end;
    } else {
        output.push(segment);
    }
}

#[cfg(test)]
mod tests {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    use scribe_common::protocol::{
        BeadsEpicGraph, BeadsGraphEdge, BeadsGraphNode, BeadsIssueQueue,
    };

    use super::*;

    fn node(id: &str) -> BeadsGraphNode {
        BeadsGraphNode {
            id: id.into(),
            title: format!("Node {id}"),
            priority: 1,
            status: "open".into(),
            queue: BeadsIssueQueue::Ready,
            assignee: None,
            updated_at: "2026-08-18T00:00:00Z".into(),
        }
    }

    fn graph(ids: &[&str], edges: &[(&str, &str)]) -> BeadsEpicGraph {
        BeadsEpicGraph {
            epic_id: "flow-epic".into(),
            epic_title: "Flow epic".into(),
            closed: 0,
            total: u32::try_from(ids.len()).unwrap_or(u32::MAX),
            nodes: ids.iter().map(|id| node(id)).collect(),
            edges: edges
                .iter()
                .map(|(from, to)| BeadsGraphEdge { from: (*from).into(), to: (*to).into() })
                .collect(),
        }
    }

    fn ids_in_rank<'a>(
        layout: &FlowLayout,
        graph: &'a BeadsEpicGraph,
        rank: usize,
    ) -> Vec<&'a str> {
        let mut nodes = layout.nodes.iter().filter(|node| node.rank == rank).collect::<Vec<_>>();
        nodes.sort_by_key(|node| node.order);
        nodes
            .into_iter()
            .filter_map(|node| graph.nodes.get(node.issue_index))
            .map(|node| node.id.as_str())
            .collect()
    }

    #[test]
    fn longest_path_ranking_uses_the_deepest_parent_at_convergence() {
        let graph = graph(
            &["root-a", "root-b", "middle", "joined"],
            &[("root-a", "joined"), ("root-b", "middle"), ("middle", "joined")],
        );
        assert_eq!(longest_path_ranks(&graph).unwrap(), vec![0, 0, 1, 2]);
    }

    #[test]
    fn barycenter_pass_reorders_an_inverted_rank_from_its_predecessors() {
        let graph = graph(
            &["left", "right", "right-child", "left-child"],
            &[("left", "left-child"), ("right", "right-child")],
        );
        let layout = layout_flow(&graph, 1.0).unwrap();
        assert_eq!(ids_in_rank(&layout, &graph, 1), vec!["left-child", "right-child"]);
    }

    #[test]
    fn skip_edge_inserts_one_dummy_in_every_intermediate_rank() {
        let graph = graph(
            &["start", "middle", "end"],
            &[("start", "middle"), ("middle", "end"), ("start", "end")],
        );
        let layout = layout_flow(&graph, 1.0).unwrap();
        assert_eq!(layout.dummy_nodes, vec![FlowDummyNode { edge_index: 2, rank: 1, order: 1 }]);
    }

    #[test]
    fn shared_gutter_splits_into_independently_lightable_intervals() {
        let runs = [
            EdgeWireRun {
                edge_index: 0,
                axis: WireAxis::Horizontal,
                offset: 12.0,
                start: 0.0,
                end: 10.0,
            },
            EdgeWireRun {
                edge_index: 1,
                axis: WireAxis::Horizontal,
                offset: 12.0,
                start: 0.0,
                end: 5.0,
            },
        ];
        let segments =
            union_wire_runs(
                &runs,
                |edge| {
                    if edge == 1 { WireClass::Traced } else { WireClass::Base }
                },
            );
        assert_eq!(
            segments,
            vec![
                WireSegment {
                    axis: WireAxis::Horizontal,
                    offset: 12.0,
                    start: 0.0,
                    end: 5.0,
                    class: WireClass::Traced,
                },
                WireSegment {
                    axis: WireAxis::Horizontal,
                    offset: 12.0,
                    start: 5.0,
                    end: 10.0,
                    class: WireClass::Base,
                },
            ]
        );
    }

    #[test]
    fn in_memory_cycle_is_rejected_instead_of_looping() {
        let graph = graph(&["a", "b"], &[("a", "b"), ("b", "a")]);
        assert_eq!(longest_path_ranks(&graph), Err(FlowLayoutError::Cycle));
        assert_eq!(layout_flow(&graph, 1.0), Err(FlowLayoutError::Cycle));
    }

    #[test]
    fn row_budget_matches_the_mock_at_scale_extremes() {
        let metrics = FlowMetrics::standard();
        assert_eq!(metrics.rank_pitch(1.0).to_bits(), 242.0f64.to_bits());
        assert_eq!(metrics.row_pitch(1.0).to_bits(), 34.0f64.to_bits());
        assert_eq!(metrics.rows_that_fit(0.8), Some(5));
        assert_eq!(metrics.rows_that_fit(1.0), Some(4));
        assert_eq!(metrics.rows_that_fit(1.6), Some(2));
        assert!(layout_flow(&graph(&["a", "b"], &[]), 1.6).is_ok());
        assert!(matches!(
            layout_flow(&graph(&["a", "b", "c"], &[]), 1.6),
            Err(FlowLayoutError::RankTooWide { rank: 0, nodes: 3, capacity: 2 })
        ));
    }

    #[test]
    fn two_hundred_node_layout_stays_under_two_milliseconds() {
        let ids = (0..200).map(|index| format!("n{index}")).collect::<Vec<_>>();
        let edge_ids = (0..199)
            .map(|index| (format!("n{index}"), format!("n{}", index + 1)))
            .collect::<Vec<_>>();
        let graph = BeadsEpicGraph {
            epic_id: "benchmark".into(),
            epic_title: "Benchmark".into(),
            closed: 0,
            total: 200,
            nodes: ids.iter().map(|id| node(id)).collect(),
            edges: edge_ids
                .iter()
                .map(|(from, to)| BeadsGraphEdge { from: from.clone(), to: to.clone() })
                .collect(),
        };
        black_box(layout_flow(&graph, 1.0).unwrap());
        let fastest = (0..20)
            .map(|_| {
                let start = Instant::now();
                black_box(layout_flow(&graph, 1.0).unwrap());
                start.elapsed()
            })
            .min()
            .unwrap_or(Duration::MAX);
        assert!(fastest < Duration::from_millis(2), "fastest 200-node layout took {fastest:?}");
    }
}
