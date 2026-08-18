//! Pure layered-DAG layout for the Beads board Flow view.
//!
//! Coordinates are physical pixels at the requested text scale. The graph band
//! keeps its fixed 139px reservation while node and gap dimensions scale.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gpui::{
    AnyElement, App, FocusHandle, KeyDownEvent, MouseButton, Role, SharedString, Window, div,
    prelude::*, px,
};
use scribe_common::protocol::{BeadsEpicGraph, BeadsGraphEdge, BeadsGraphNode, BeadsIssueQueue};

use crate::beads_board::BeadsBoardColors;
use crate::layout::Rect;

const MAX_FLOW_NODES: usize = 200;

fn scalar(value: usize) -> f32 {
    u16::try_from(value).map_or(f32::from(u16::MAX), f32::from)
}

fn count_to_f32(value: u32) -> f32 {
    u16::try_from(value).map_or(f32::from(u16::MAX), f32::from)
}

/// Geometry copied from the normative A3 Flow mock as formulas, not pitches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowMetrics {
    pub node_width: f32,
    pub node_height: f32,
    pub gutter: f32,
    pub row_gap: f32,
    pub graph_height: f32,
    pub left_padding: f32,
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
    pub fn rank_pitch(self, text_scale: f32) -> f32 {
        (self.node_width + self.gutter) * text_scale
    }

    #[must_use]
    pub fn row_pitch(self, text_scale: f32) -> f32 {
        (self.node_height + self.row_gap) * text_scale
    }

    #[must_use]
    pub fn rows_that_fit(self, text_scale: f32) -> Option<usize> {
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
    InvalidTextScale(f32),
    #[error("Flow rank {rank} has {nodes} nodes but only {capacity} fit")]
    RankTooWide { rank: usize, nodes: usize, capacity: usize },
}

/// One real graph node after ranking and barycenter ordering.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowNodeLayout {
    pub issue_index: usize,
    pub rank: usize,
    pub order: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
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
    pub offset: f32,
    pub start: f32,
    pub end: f32,
}

/// One non-overlapping paint interval keyed by rail and colour class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WireSegment {
    pub axis: WireAxis,
    pub offset: f32,
    pub start: f32,
    pub end: f32,
    pub class: WireClass,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlowLayout {
    pub nodes: Vec<FlowNodeLayout>,
    pub dummy_nodes: Vec<FlowDummyNode>,
    pub wire_runs: Vec<EdgeWireRun>,
    pub rank_count: usize,
    pub width: f32,
    pub height: f32,
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
// @lat: [[client#Client#Beads Flow Layout Engine]]
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
    positions: &HashMap<usize, f32>,
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

fn barycenter(node: &ExpandedNode, positions: &HashMap<usize, f32>) -> f32 {
    let (sum, count) = node
        .predecessors
        .iter()
        .filter_map(|index| positions.get(index))
        .fold((0.0, 0usize), |(sum, count), position| (sum + position, count + 1));
    if count == 0 { scalar(node.stable_order) } else { sum / scalar(count) }
}

/// Rank, order, place and route an admitted epic graph.
pub fn layout_flow(graph: &BeadsEpicGraph, text_scale: f32) -> Result<FlowLayout, FlowLayoutError> {
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
    positions: Vec<(f32, f32)>,
}

#[derive(Clone, Copy)]
struct Placement {
    metrics: FlowMetrics,
    text_scale: f32,
    capacity: usize,
}

fn place_nodes(
    expanded: &ExpandedGraph,
    metrics: FlowMetrics,
    text_scale: f32,
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

fn centered_row_tops(count: usize, metrics: FlowMetrics, text_scale: f32) -> Vec<f32> {
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
    text_scale: f32,
}

impl WireRouter {
    fn emit(
        self,
        edges: &[(usize, usize)],
        ranks: &[usize],
        positions: &[(f32, f32)],
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
        start: (f32, f32),
        end: (f32, f32),
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
        start: (f32, f32),
        end: (f32, f32),
        runs: &mut Vec<EdgeWireRun>,
    ) {
        let lane = self.nearest_lane(f32::midpoint(start.1, end.1));
        let stub = 8.0 * self.text_scale;
        let left = start.0 + stub;
        let right = end.0 - stub;
        push_horizontal(runs, edge_index, start.1, start.0, left);
        push_vertical(runs, edge_index, left, start.1, lane);
        push_horizontal(runs, edge_index, lane, left, right);
        push_vertical(runs, edge_index, right, lane, end.1);
        push_horizontal(runs, edge_index, end.1, right, end.0);
    }

    fn nearest_lane(self, target: f32) -> f32 {
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
    offset: f32,
    start: f32,
    end: f32,
) {
    push_run(runs, EdgeWireRun { edge_index, axis: WireAxis::Horizontal, offset, start, end });
}

fn push_vertical(
    runs: &mut Vec<EdgeWireRun>,
    edge_index: usize,
    offset: f32,
    start: f32,
    end: f32,
) {
    push_run(runs, EdgeWireRun { edge_index, axis: WireAxis::Vertical, offset, start, end });
}

fn push_run(runs: &mut Vec<EdgeWireRun>, mut run: EdgeWireRun) {
    if run.start > run.end {
        std::mem::swap(&mut run.start, &mut run.end);
    }
    if run.end - run.start > f32::EPSILON {
        runs.push(run);
    }
}

/// Union overlapping edge runs so translucent rails paint exactly once.
///
/// Higher classes win only on their covered sub-interval, which lets a traced
/// edge light half a shared gutter without brightening or replacing the rest.
// @lat: [[test#GPUI Client Headless Suites#Flow layout and paint-path guard]]
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
    boundaries.sort_by(f32::total_cmp);
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

/// The lit subgraph a hovered node raises, and the counts describing it.
///
/// Hover is state, and the renderer is pure, so the workspace computes this
/// once per hover change and hands it back through [`FlowRender`]. Membership
/// is the ancestor closure, the descendant closure, and the hovered node
/// itself: the question a dependency tracker exists to answer is what a node
/// waited on and what it releases, and neither is a direct-neighbour query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowTrace {
    pub hovered_issue_id: String,
    /// Issue ids at full opacity. Everything else dims.
    pub on_path: HashSet<String>,
    /// Per-edge paint class, indexed to match `BeadsEpicGraph::edges`.
    pub edge_classes: Vec<WireClass>,
    /// Nodes the hovered node eventually unblocks.
    pub releases: usize,
    /// Nodes the hovered node eventually waited on.
    pub blocked_by: usize,
}

impl FlowTrace {
    /// Resolve the ancestor/descendant closure of `hovered_issue_id`.
    ///
    /// Returns `None` when the id is not in the graph, which is what an
    /// out-of-date hover against a re-opened graph looks like.
    // @lat: [[client#Client#Beads Flow Layout Engine#Tracing a chain by hovering]]
    pub fn from_hover(graph: &BeadsEpicGraph, hovered_issue_id: &str) -> Option<Self> {
        if !graph.nodes.iter().any(|node| node.id == hovered_issue_id) {
            return None;
        }
        let descendants = reachable(graph, hovered_issue_id, Direction::Forward);
        let ancestors = reachable(graph, hovered_issue_id, Direction::Backward);
        let releases = descendants.len();
        let blocked_by = ancestors.len();
        let mut on_path = descendants;
        on_path.extend(ancestors);
        on_path.insert(hovered_issue_id.to_owned());
        // An edge lights only when BOTH endpoints are lit. A lit node at the
        // fringe of the closure still has edges leaving the traced path, and
        // brightening those would claim a relationship the trace does not have.
        let edge_classes = graph
            .edges
            .iter()
            .map(|edge| {
                if on_path.contains(&edge.from) && on_path.contains(&edge.to) {
                    WireClass::Traced
                } else {
                    WireClass::Dimmed
                }
            })
            .collect();
        Some(Self {
            hovered_issue_id: hovered_issue_id.to_owned(),
            on_path,
            edge_classes,
            releases,
            blocked_by,
        })
    }

    /// The mock's chip wording, verbatim.
    fn chip_text(&self) -> String {
        format!("releases {} · blocked by {}", self.releases, self.blocked_by)
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

/// Breadth-first closure over `blocks` edges in one direction.
fn reachable(graph: &BeadsEpicGraph, start: &str, direction: Direction) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([start.to_owned()]);
    while let Some(current) = queue.pop_front() {
        for edge in &graph.edges {
            let (near, far) = match direction {
                Direction::Forward => (&edge.from, &edge.to),
                Direction::Backward => (&edge.to, &edge.from),
            };
            if *near == current && !seen.contains(far) && *far != start {
                seen.insert(far.clone());
                queue.push_back(far.clone());
            }
        }
    }
    seen
}

/// One Flow-node activation owned by the mode and panel work items.
///
/// Rendering only delivers the issue id. It deliberately knows neither the
/// panel nor the board mode, so retargeting remains the later interaction
/// slice's responsibility.
pub type FlowNodeActionHandler = Arc<dyn Fn(String, &mut Window, &mut App)>;

/// Pointer entering or leaving a node, delivered to the workspace-owned state.
pub type FlowNodeHoverHandler = Arc<dyn Fn(String, bool, &mut Window, &mut App)>;

/// Focus, activation and hover supplied by the workspace-owned Flow state.
#[derive(Clone)]
pub struct FlowNodeControl {
    pub focus: FocusHandle,
    pub on_activate: FlowNodeActionHandler,
    pub on_hover: FlowNodeHoverHandler,
}

/// Inputs the workspace-owned mode state supplies to the pure Flow renderer.
pub struct FlowRender<'a> {
    pub rect: Rect,
    pub graph: &'a BeadsEpicGraph,
    pub layout: &'a FlowLayout,
    pub cursor_issue_id: &'a str,
    /// Current horizontal offset, owned and changed by the later mode slice.
    pub scroll_x: f32,
    pub text_scale: f32,
    pub colors: BeadsBoardColors,
    /// Must carry a focus and generic activation seam for every graph node.
    pub node_controls: &'a HashMap<String, FlowNodeControl>,
    /// `None` is the at-rest Base treatment. A trace supplies node membership
    /// and per-edge classes without rewriting graph geometry.
    pub trace: Option<&'a FlowTrace>,
    /// Issues an agent session is live on in this window, right now.
    ///
    /// Membership alone decides the halo. A node's `assignee` names the agent
    /// but never implies it is running: bd records who claimed an issue, and
    /// a claim outlives the process that made it.
    pub live_issue_ids: &'a HashSet<String>,
}

/// A boundary failure between an admitted graph, its layout, and GPUI chrome.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlowRenderError {
    #[error("Flow layout node {issue_index} is outside the graph")]
    LayoutNodeOutsideGraph { issue_index: usize },
    #[error("Flow layout contains issue {issue_index} more than once")]
    DuplicateLayoutIssue { issue_index: usize },
    #[error("Flow layout has {actual} nodes but graph has {expected}")]
    IncompleteLayout { expected: usize, actual: usize },
    #[error("Flow cursor issue is not in graph: {issue_id}")]
    CursorNotInGraph { issue_id: String },
    #[error("Flow node is missing its focus and activation seam: {issue_id}")]
    MissingNodeControl { issue_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowNodeState {
    Done,
    Ready,
    Blocked,
    InProgress,
    Backlog,
}

impl FlowNodeState {
    const fn label(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Ready => "ready",
            Self::Blocked => "blocked",
            Self::InProgress => "in progress",
            Self::Backlog => "backlog",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct FlowNodePresentation {
    id: String,
    title: String,
    priority: u8,
    state: FlowNodeState,
    description: String,
    rank: usize,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    cursor: bool,
    /// False only while a trace is active and this node sits off its path.
    on_path: bool,
    /// Whether an agent session is running on this issue in this window.
    liveness: FlowLiveness,
}

/// Whether a machine is on this issue right now, and which one.
///
/// One field rather than a flag beside an optional name, because a name
/// without liveness is not a state this view has: `bd`'s assignee outlives the
/// process that set it, so a name alone must never reach the halo.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FlowLiveness {
    Idle,
    /// The agent's short name is absent when `bd` recorded no assignee. The
    /// halo still stands: the missing field is the label, not the evidence.
    Running {
        agent: Option<String>,
    },
}

impl FlowLiveness {
    const fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    fn agent(&self) -> Option<&str> {
        match self {
            Self::Running { agent } => agent.as_deref(),
            Self::Idle => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct FlowRankLabel {
    rank: usize,
    x: f32,
    text: &'static str,
}

/// The floating count chip a hovered node raises.
#[derive(Debug, Clone, PartialEq)]
struct FlowChip {
    text: String,
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct FlowPresentation {
    nodes: Vec<FlowNodePresentation>,
    wires: Vec<WireSegment>,
    rank_labels: Vec<FlowRankLabel>,
    chip: Option<FlowChip>,
    width: f32,
}

const FLOW_BAND_HEIGHT: f32 = 34.0;
const FLOW_RULER_HEIGHT: f32 = 15.0;
const FLOW_GRAPH_HEIGHT: f32 = 139.0;
const FLOW_GRAPH_TOP: f32 = FLOW_BAND_HEIGHT + FLOW_RULER_HEIGHT;
const FLOW_HBAR_TOP: f32 = FLOW_GRAPH_TOP + FLOW_GRAPH_HEIGHT;
const FLOW_HBAR_HEIGHT: f32 = 2.0;
const FLOW_FLOOR_HEIGHT: f32 = 3.0;
const FLOW_PROGRESS_WIDTH: f32 = 150.0;
/// Chip offset from the hovered node's own box, read off the mock's
/// `left:286px;top:104px` chip against its `left:272px;top:74px` node.
const FLOW_CHIP_OFFSET_X: f32 = 14.0;
const FLOW_CHIP_GAP_Y: f32 = 6.0;
/// The mock's `.fl.trace .node { opacity:.24 }`.
const FLOW_TRACE_DIM: f32 = 0.24;

/// Lower an admitted graph and its layout into the Flow strip.
///
/// Mode transitions, wheel routing, node retargeting, trace state, and live
/// sessions stay outside this function. Their state arrives through the
/// explicit input seams instead of being recreated by the renderer.
// @lat: [[client#Client#Beads Flow Layout Engine#Flow mode entry, exit, and scrolling]]
pub fn render(render: &FlowRender<'_>) -> Result<AnyElement, FlowRenderError> {
    let presentation = present_flow(
        render.graph,
        render.layout,
        render.cursor_issue_id,
        render.trace,
        render.live_issue_ids,
    )?;
    require_node_controls(&presentation, render.node_controls)?;
    let scroll_x = clamped_scroll(render.scroll_x, presentation.width, render.rect.width);
    let board_id = SharedString::from(format!("beads-flow-{}", render.graph.epic_id));
    let contents = div()
        .relative()
        .size_full()
        .overflow_hidden()
        .bg(render.colors.ground)
        .child(flow_band(render.graph, render.cursor_issue_id, &render.colors, render.text_scale))
        .child(rank_ruler(&presentation, scroll_x, &render.colors, render.text_scale))
        .child(flow_graph(
            &presentation,
            render.node_controls,
            scroll_x,
            &render.colors,
            render.text_scale,
        ))
        .child(scrollbar(&presentation, scroll_x, render.rect.width, &render.colors))
        .child(floor(&render.colors));
    Ok(div()
        .id(board_id)
        .absolute()
        .left(px(render.rect.x))
        .top(px(render.rect.y))
        .w(px(render.rect.width))
        .h(px(render.rect.height))
        .child(contents)
        .into_any_element())
}

fn present_flow(
    graph: &BeadsEpicGraph,
    layout: &FlowLayout,
    cursor_issue_id: &str,
    trace: Option<&FlowTrace>,
    live_issue_ids: &HashSet<String>,
) -> Result<FlowPresentation, FlowRenderError> {
    let nodes = presentation_nodes(graph, layout, cursor_issue_id, trace, live_issue_ids)?;
    let rank_labels = rank_labels(&nodes);
    let wires = layout.wire_segments(|edge| {
        trace.map_or(WireClass::Base, |trace| {
            trace.edge_classes.get(edge).copied().unwrap_or(WireClass::Dimmed)
        })
    });
    let chip = trace.and_then(|trace| chip_for(&nodes, trace));
    Ok(FlowPresentation { nodes, wires, rank_labels, chip, width: layout.width })
}

/// Place the chip under the hovered node, per the mock's offsets.
fn chip_for(nodes: &[FlowNodePresentation], trace: &FlowTrace) -> Option<FlowChip> {
    let hovered = nodes.iter().find(|node| node.id == trace.hovered_issue_id)?;
    Some(FlowChip {
        text: trace.chip_text(),
        x: hovered.x + FLOW_CHIP_OFFSET_X,
        y: hovered.y + hovered.height + FLOW_CHIP_GAP_Y,
    })
}

fn presentation_nodes(
    graph: &BeadsEpicGraph,
    layout: &FlowLayout,
    cursor_issue_id: &str,
    trace: Option<&FlowTrace>,
    live_issue_ids: &HashSet<String>,
) -> Result<Vec<FlowNodePresentation>, FlowRenderError> {
    let mut seen = HashSet::with_capacity(layout.nodes.len());
    let mut nodes = Vec::with_capacity(layout.nodes.len());
    for positioned in &layout.nodes {
        let Some(node) = graph.nodes.get(positioned.issue_index) else {
            return Err(FlowRenderError::LayoutNodeOutsideGraph {
                issue_index: positioned.issue_index,
            });
        };
        if !seen.insert(positioned.issue_index) {
            return Err(FlowRenderError::DuplicateLayoutIssue {
                issue_index: positioned.issue_index,
            });
        }
        nodes.push(node_presentation(&NodePresentationInput {
            node,
            positioned,
            graph,
            cursor_issue_id,
            trace,
            live_issue_ids,
        }));
    }
    if nodes.len() != graph.nodes.len() {
        return Err(FlowRenderError::IncompleteLayout {
            expected: graph.nodes.len(),
            actual: nodes.len(),
        });
    }
    if !nodes.iter().any(|node| node.id == cursor_issue_id) {
        return Err(FlowRenderError::CursorNotInGraph { issue_id: cursor_issue_id.into() });
    }
    Ok(nodes)
}

/// The per-node inputs `node_presentation` reads, grouped to keep it inside
/// the crate's argument budget as the renderer gained its liveness seam.
struct NodePresentationInput<'a> {
    node: &'a BeadsGraphNode,
    positioned: &'a FlowNodeLayout,
    graph: &'a BeadsEpicGraph,
    cursor_issue_id: &'a str,
    trace: Option<&'a FlowTrace>,
    live_issue_ids: &'a HashSet<String>,
}

fn node_presentation(input: &NodePresentationInput<'_>) -> FlowNodePresentation {
    let &NodePresentationInput { node, positioned, graph, cursor_issue_id, trace, live_issue_ids } =
        input;
    let state = node_state(node.queue);
    // A hovered node takes the cursor treatment on top of its own, so the
    // node the pointer is reading is never the one node without a marker.
    let cursor =
        node.id == cursor_issue_id || trace.is_some_and(|trace| trace.hovered_issue_id == node.id);
    let liveness = if live_issue_ids.contains(&node.id) {
        FlowLiveness::Running { agent: agent_name(node.assignee.as_deref()) }
    } else {
        FlowLiveness::Idle
    };
    FlowNodePresentation {
        id: node.id.clone(),
        title: node.title.clone(),
        priority: node.priority,
        state,
        description: relationship_description(graph, &node.id),
        rank: positioned.rank,
        x: positioned.x,
        y: positioned.y,
        width: positioned.width,
        height: positioned.height,
        cursor,
        on_path: trace.is_none_or(|trace| trace.on_path.contains(&node.id)),
        liveness,
    }
}

/// Shorten `bd`'s assignee into the agent name the mock shows.
///
/// A run assignee is a generated `codex-implement-ready-run-<stamp>.<id>`,
/// which cannot fit a 214px node beside a title. The leading segment is the
/// agent itself, which is the part that answers "who is on this".
fn agent_name(assignee: Option<&str>) -> Option<String> {
    let assignee = assignee?.trim();
    let name = assignee.split('-').next().unwrap_or(assignee).trim();
    if name.is_empty() { None } else { Some(name.to_owned()) }
}

const fn node_state(queue: BeadsIssueQueue) -> FlowNodeState {
    match queue {
        BeadsIssueQueue::Done => FlowNodeState::Done,
        BeadsIssueQueue::Ready => FlowNodeState::Ready,
        BeadsIssueQueue::Blocked => FlowNodeState::Blocked,
        BeadsIssueQueue::InProgress => FlowNodeState::InProgress,
        BeadsIssueQueue::Backlog => FlowNodeState::Backlog,
    }
}

fn relationship_description(graph: &BeadsEpicGraph, issue_id: &str) -> String {
    let blockers = graph
        .edges
        .iter()
        .filter(|edge| edge.to == issue_id)
        .map(|edge| short_issue_id(&edge.from))
        .collect::<Vec<_>>();
    let dependents = graph
        .edges
        .iter()
        .filter(|edge| edge.from == issue_id)
        .map(|edge| short_issue_id(&edge.to))
        .collect::<Vec<_>>();
    format!(
        "Blockers: {}. Dependents: {}.",
        relationship_list(&blockers),
        relationship_list(&dependents),
    )
}

fn relationship_list(ids: &[String]) -> String {
    if ids.is_empty() { "none".into() } else { ids.join(", ") }
}

fn short_issue_id(id: &str) -> String {
    id.strip_prefix("scribe-").unwrap_or(id).into()
}

fn rank_labels(nodes: &[FlowNodePresentation]) -> Vec<FlowRankLabel> {
    let Some(cursor) = nodes.iter().find(|node| node.cursor) else { return Vec::new() };
    let mut labels = Vec::new();
    if rank_is_done(nodes, 0) {
        push_rank_label(&mut labels, nodes, 0, "SHIPPED");
    }
    push_rank_label(&mut labels, nodes, cursor.rank, "NOW");
    push_rank_label(&mut labels, nodes, cursor.rank + 1, "NEXT");
    let last_rank = nodes.iter().map(|node| node.rank).max().unwrap_or_default();
    if last_rank > cursor.rank + 1 {
        push_rank_label(&mut labels, nodes, last_rank, "LATER");
    }
    labels
}

fn rank_is_done(nodes: &[FlowNodePresentation], rank: usize) -> bool {
    let rank_nodes = nodes.iter().filter(|node| node.rank == rank).collect::<Vec<_>>();
    !rank_nodes.is_empty() && rank_nodes.iter().all(|node| node.state == FlowNodeState::Done)
}

fn push_rank_label(
    labels: &mut Vec<FlowRankLabel>,
    nodes: &[FlowNodePresentation],
    rank: usize,
    text: &'static str,
) {
    if labels.iter().any(|label| label.rank == rank) {
        return;
    }
    if let Some(node) = nodes.iter().find(|node| node.rank == rank) {
        labels.push(FlowRankLabel { rank, x: node.x, text });
    }
}

fn require_node_controls(
    presentation: &FlowPresentation,
    controls: &HashMap<String, FlowNodeControl>,
) -> Result<(), FlowRenderError> {
    for node in &presentation.nodes {
        if !controls.contains_key(&node.id) {
            return Err(FlowRenderError::MissingNodeControl { issue_id: node.id.clone() });
        }
    }
    Ok(())
}

fn clamped_scroll(requested: f32, graph_width: f32, viewport_width: f32) -> f32 {
    let max = (graph_width - viewport_width).max(0.0);
    requested.clamp(0.0, max)
}

fn flow_band(
    graph: &BeadsEpicGraph,
    cursor_issue_id: &str,
    colors: &BeadsBoardColors,
    text_scale: f32,
) -> AnyElement {
    let progress = progress_width(graph);
    div()
        .absolute()
        .left_0()
        .right_0()
        .top_0()
        .h(px(FLOW_BAND_HEIGHT))
        .px(px(14.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .bg(colors.band)
        .border_b_1()
        .border_color(colors.hairline)
        .child(back_label(colors, text_scale))
        .child(epic_label(&graph.epic_title, colors, text_scale))
        .child(div().text_size(px(10.0 * text_scale)).text_color(colors.chevron).child("⌄"))
        .child(tally(graph, colors, text_scale))
        .child(progress_bar(progress, colors))
        .child(opened_tag(cursor_issue_id, colors, text_scale))
        .child(mode_pair(colors, text_scale))
        .into_any_element()
}

fn back_label(colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    div()
        .flex_none()
        .pr(px(8.0))
        .font_weight(gpui::FontWeight(600.0))
        .text_size(px(9.0 * text_scale))
        .text_color(colors.muted)
        .child("← LANES")
        .into_any_element()
}

fn epic_label(epic: &str, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    div()
        .min_w(px(0.0))
        .truncate()
        .font_weight(gpui::FontWeight(700.0))
        .text_size(px(9.5 * text_scale))
        .text_color(colors.title)
        .child(epic.to_uppercase())
        .into_any_element()
}

fn tally(graph: &BeadsEpicGraph, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_baseline()
        .font_family("monospace")
        .text_color(colors.title)
        .child(
            div()
                .font_weight(gpui::FontWeight(600.0))
                .text_size(px(17.0 * text_scale))
                .child(graph.closed.to_string()),
        )
        .child(
            div()
                .font_weight(gpui::FontWeight(500.0))
                .text_size(px(9.5 * text_scale))
                .text_color(colors.muted)
                .child(format!("/{}", graph.total)),
        )
        .into_any_element()
}

fn progress_width(graph: &BeadsEpicGraph) -> f32 {
    let ratio =
        if graph.total == 0 { 0.0 } else { count_to_f32(graph.closed) / count_to_f32(graph.total) };
    FLOW_PROGRESS_WIDTH * ratio.clamp(0.0, 1.0)
}

fn progress_bar(done_width: f32, colors: &BeadsBoardColors) -> AnyElement {
    div()
        .relative()
        .flex_none()
        .w(px(FLOW_PROGRESS_WIDTH))
        .h(px(2.0))
        .bg(colors.progress_track)
        .child(div().absolute().left_0().top_0().w(px(done_width)).h(px(2.0)).bg(colors.done_state))
        .into_any_element()
}

fn opened_tag(cursor_issue_id: &str, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    div()
        .flex_none()
        .font_family("monospace")
        .font_weight(gpui::FontWeight(500.0))
        .text_size(px(9.5 * text_scale))
        .text_color(colors.muted)
        .child(format!("opened {}", short_issue_id(cursor_issue_id)))
        .into_any_element()
}

fn mode_pair(colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    div()
        .ml_auto()
        .flex()
        .gap(px(1.0))
        .child(mode_label("LANES", false, colors, text_scale))
        .child(mode_label("FLOW", true, colors, text_scale))
        .into_any_element()
}

fn mode_label(label: &str, active: bool, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    let element = div()
        .px(px(8.0))
        .py(px(5.0))
        .font_weight(gpui::FontWeight(600.0))
        .text_size(px(9.0 * text_scale))
        .text_color(if active { colors.title } else { colors.muted })
        .child(label.to_owned());
    if active {
        element.bg(colors.button_hover).into_any_element()
    } else {
        element.into_any_element()
    }
}

fn rank_ruler(
    presentation: &FlowPresentation,
    scroll_x: f32,
    colors: &BeadsBoardColors,
    text_scale: f32,
) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(FLOW_BAND_HEIGHT))
        .h(px(FLOW_RULER_HEIGHT))
        .overflow_hidden()
        .children(presentation.rank_labels.iter().map(|label| {
            div()
                .absolute()
                .left(px(label.x - scroll_x))
                .top_0()
                .font_family("monospace")
                .font_weight(gpui::FontWeight(500.0))
                .text_size(px(9.5 * text_scale))
                .text_color(colors.rank_label)
                .child(label.text)
        }))
        .into_any_element()
}

fn flow_graph(
    presentation: &FlowPresentation,
    controls: &HashMap<String, FlowNodeControl>,
    scroll_x: f32,
    colors: &BeadsBoardColors,
    text_scale: f32,
) -> AnyElement {
    let canvas = div()
        .absolute()
        .left(px(-scroll_x))
        .top_0()
        .w(px(presentation.width))
        .h(px(FLOW_GRAPH_HEIGHT))
        .child(wires(&presentation.wires, colors))
        .children(presentation.nodes.iter().filter_map(|node| {
            controls.get(&node.id).map(|control| flow_node(node, control, colors, text_scale))
        }))
        .children(presentation.chip.iter().map(|chip| trace_chip(chip, colors, text_scale)));
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(FLOW_GRAPH_TOP))
        .h(px(FLOW_GRAPH_HEIGHT))
        .overflow_hidden()
        .child(canvas)
        .into_any_element()
}

fn wires(segments: &[WireSegment], colors: &BeadsBoardColors) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .children(segments.iter().map(|segment| wire(segment, colors)))
        .into_any_element()
}

fn wire(segment: &WireSegment, colors: &BeadsBoardColors) -> AnyElement {
    let color = match segment.class {
        WireClass::Base => colors.wire,
        WireClass::Traced => colors.wire_traced,
        WireClass::Dimmed => colors.wire_dimmed,
    };
    let (left, top, width, height) = match segment.axis {
        WireAxis::Horizontal => (segment.start, segment.offset, segment.end - segment.start, 1.0),
        WireAxis::Vertical => (segment.offset, segment.start, 1.0, segment.end - segment.start),
    };
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .h(px(height))
        .bg(color)
        .into_any_element()
}

/// The mock's `.fl .unlocks` chip, stating what the lit path contains.
fn trace_chip(chip: &FlowChip, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    div()
        .absolute()
        .left(px(chip.x))
        .top(px(chip.y))
        .px(px(7.0))
        .py(px(3.0))
        .rounded(px(2.0))
        .bg(colors.chip)
        .border_1()
        .border_color(colors.chip_border)
        .font_family("monospace")
        .font_weight(gpui::FontWeight(500.0))
        .text_size(px(9.5 * text_scale))
        .text_color(colors.queue_name)
        .child(chip.text.clone())
        .into_any_element()
}

fn flow_node(
    node: &FlowNodePresentation,
    control: &FlowNodeControl,
    colors: &BeadsBoardColors,
    text_scale: f32,
) -> AnyElement {
    let click_id = node.id.clone();
    let key_id = node.id.clone();
    let hover_id = node.id.clone();
    let click_focus = control.focus.clone();
    let click_activate = Arc::clone(&control.on_activate);
    let key_activate = Arc::clone(&control.on_activate);
    let on_hover = Arc::clone(&control.on_hover);
    let hover = colors.button_hover;
    let mut element = div()
        .id(SharedString::from(format!("beads-flow-node-{}", node.id)))
        .role(Role::Button)
        .aria_label(node_accessible_name(node))
        .aria_description(node.description.clone())
        .track_focus(&control.focus)
        .tab_stop(true)
        .absolute()
        .left(px(node.x))
        .top(px(node.y))
        .w(px(node.width))
        .h(px(node.height))
        .px(px(6.0 * text_scale))
        .flex()
        .items_center()
        .gap(px(6.0 * text_scale))
        .cursor_pointer()
        .hover(move |element| element.bg(hover))
        .on_hover(move |entered, window, app| on_hover(hover_id.clone(), *entered, window, app))
        .on_mouse_down(MouseButton::Left, |_, _, app| app.stop_propagation())
        .on_click(move |_, window, app| {
            window.focus(&click_focus, app);
            click_activate(click_id.clone(), window, app);
        })
        .on_key_down(move |event: &KeyDownEvent, window, app| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                app.stop_propagation();
                key_activate(key_id.clone(), window, app);
            }
        });
    if node.cursor {
        element = element.bg(colors.cursor_fill).child(
            div().absolute().left_0().top_0().bottom_0().w(px(2.0)).bg(colors.cursor_keyline),
        );
    }
    if !node.on_path {
        element = element.opacity(FLOW_TRACE_DIM);
    }
    element.child(flow_node_contents(node, colors, text_scale)).into_any_element()
}

/// The node's spoken name. Liveness is announced because the halo is not
/// available to a reader who cannot see it, and it is the one fact on the
/// node that the state label does not already carry.
fn node_accessible_name(node: &FlowNodePresentation) -> String {
    let base = format!("{} {}, {}", node.id, node.title, node.state.label());
    match &node.liveness {
        FlowLiveness::Running { agent: Some(agent) } => format!("{base}, {agent} running now"),
        FlowLiveness::Running { agent: None } => format!("{base}, running now"),
        FlowLiveness::Idle => base,
    }
}

fn flow_node_contents(
    node: &FlowNodePresentation,
    colors: &BeadsBoardColors,
    text_scale: f32,
) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .gap(px(6.0 * text_scale))
        .child(node_dot(node, colors, text_scale))
        .child(priority_text(node.priority, colors, text_scale))
        .child(node_title(node, colors, text_scale))
        .children(node.liveness.agent().map(|agent| agent_line(agent, colors, text_scale)))
        .child(node_id(&node.id, colors, text_scale))
        .into_any_element()
}

// @lat: [[client#Client#Beads Flow Layout Engine#Reading liveness from a node]]
fn node_dot(node: &FlowNodePresentation, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    let size = px(8.0 * text_scale);
    let dot = div().flex_none().size(size).rounded_full();
    // A live node's dot outranks its queue treatment: the ring says a machine
    // is on this issue now, which is the one fact a reader is scanning for.
    if node.liveness.is_running() {
        let ring = px(3.0 * text_scale);
        return dot
            .bg(colors.progress_state)
            .relative()
            .child(
                div()
                    .absolute()
                    .left(-ring)
                    .top(-ring)
                    .right(-ring)
                    .bottom(-ring)
                    .rounded_full()
                    .bg(colors.agent_halo),
            )
            .into_any_element();
    }
    match node.state {
        FlowNodeState::Done => dot.bg(colors.done_state).into_any_element(),
        FlowNodeState::Ready => dot.border_1().border_color(colors.ready_state).into_any_element(),
        FlowNodeState::Blocked => {
            dot.border_1().border_color(colors.blocked_state).into_any_element()
        }
        FlowNodeState::InProgress => dot.bg(colors.progress_state).into_any_element(),
        FlowNodeState::Backlog => dot.bg(colors.muted).into_any_element(),
    }
}

/// The mock's `.fl .node .agent`: who is running, and a dot saying they are.
fn agent_line(agent: &str, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0 * text_scale))
        .font_family("monospace")
        .font_weight(gpui::FontWeight(500.0))
        .text_size(px(9.5 * text_scale))
        .text_color(colors.agent)
        .child(agent.to_owned())
        .child(
            div().flex_none().size(px(4.0 * text_scale)).rounded_full().bg(colors.progress_state),
        )
        .into_any_element()
}

fn priority_text(priority: u8, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    let color =
        colors.priorities.get(usize::from(priority.min(4))).copied().unwrap_or(colors.muted);
    div()
        .flex_none()
        .font_family("monospace")
        .font_weight(gpui::FontWeight(700.0))
        .text_size(px(9.5 * text_scale))
        .text_color(color)
        .child(format!("P{priority}"))
        .into_any_element()
}

fn node_title(
    node: &FlowNodePresentation,
    colors: &BeadsBoardColors,
    text_scale: f32,
) -> AnyElement {
    // Live lifts the title above every queue treatment, done included: a
    // machine running on a closed issue is still the line worth reading.
    let color = if node.liveness.is_running() {
        colors.title
    } else {
        match node.state {
            FlowNodeState::Done => colors.muted,
            FlowNodeState::Blocked => colors.queue_name,
            FlowNodeState::Ready | FlowNodeState::InProgress | FlowNodeState::Backlog => {
                colors.title
            }
        }
    };
    let weight = if node.liveness.is_running() {
        650.0
    } else if node.state == FlowNodeState::Done {
        550.0
    } else {
        600.0
    };
    div()
        .flex_1()
        .min_w(px(0.0))
        .truncate()
        .font_weight(gpui::FontWeight(weight))
        .text_size(px(12.0 * text_scale))
        .text_color(color)
        .child(node.title.clone())
        .into_any_element()
}

fn node_id(id: &str, colors: &BeadsBoardColors, text_scale: f32) -> AnyElement {
    div()
        .flex_none()
        .font_family("monospace")
        .font_weight(gpui::FontWeight(500.0))
        .text_size(px(9.5 * text_scale))
        .text_color(colors.muted)
        .child(short_issue_id(id))
        .into_any_element()
}

fn scrollbar(
    presentation: &FlowPresentation,
    scroll_x: f32,
    viewport_width: f32,
    colors: &BeadsBoardColors,
) -> AnyElement {
    let graph_width = presentation.width;
    if graph_width <= viewport_width {
        return div().into_any_element();
    }
    let thumb_width = (viewport_width * viewport_width / graph_width).max(34.0);
    let max_scroll = (graph_width - viewport_width).max(1.0);
    let thumb_x = scroll_x / max_scroll * (viewport_width - thumb_width);
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(FLOW_HBAR_TOP))
        .h(px(FLOW_HBAR_HEIGHT))
        .bg(colors.progress_track)
        .child(
            div()
                .absolute()
                .left(px(thumb_x))
                .top_0()
                .w(px(thumb_width))
                .h(px(2.0))
                .bg(colors.hairline),
        )
        .into_any_element()
}

fn floor(colors: &BeadsBoardColors) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .h(px(FLOW_FLOOR_HEIGHT))
        .flex()
        .justify_center()
        .bg(colors.progress_track)
        .child(div().mt(px(1.0)).w(px(34.0)).h(px(1.0)).bg(colors.hairline))
        .into_any_element()
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

    /// No agent session is running, which is every test that is not about
    /// liveness.
    fn no_live() -> HashSet<String> {
        HashSet::new()
    }

    fn live_on(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
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
        assert_eq!(metrics.rank_pitch(1.0).to_bits(), 242.0f32.to_bits());
        assert_eq!(metrics.row_pitch(1.0).to_bits(), 34.0f32.to_bits());
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
    fn presentation_marks_exactly_one_cursor_and_describes_relationships() {
        let mut graph = graph(
            &["scribe-root", "scribe-ready", "scribe-blocked", "scribe-done", "scribe-next"],
            &[
                ("scribe-root", "scribe-ready"),
                ("scribe-ready", "scribe-blocked"),
                ("scribe-done", "scribe-blocked"),
                ("scribe-blocked", "scribe-next"),
            ],
        );
        graph.closed = 1;
        graph.nodes[0].queue = BeadsIssueQueue::InProgress;
        graph.nodes[2].queue = BeadsIssueQueue::Blocked;
        graph.nodes[3].queue = BeadsIssueQueue::Done;
        let layout = layout_flow(&graph, 1.0).unwrap();
        let presentation =
            present_flow(&graph, &layout, "scribe-blocked", None, &no_live()).unwrap();

        assert_eq!(presentation.nodes.iter().filter(|node| node.cursor).count(), 1);
        let blocked = presentation.nodes.iter().find(|node| node.cursor).unwrap();
        assert_eq!(blocked.state, FlowNodeState::Blocked);
        assert_eq!(blocked.description, "Blockers: ready, done. Dependents: next.");
        assert_eq!(
            presentation.nodes.iter().find(|node| node.id == "scribe-ready").unwrap().state,
            FlowNodeState::Ready
        );
        assert_eq!(
            presentation.nodes.iter().find(|node| node.id == "scribe-done").unwrap().state,
            FlowNodeState::Done
        );
        assert!(presentation.wires.iter().all(|wire| wire.class == WireClass::Base));
        assert!(presentation.rank_labels.iter().any(|label| label.text == "NOW"));
        assert!(presentation.rank_labels.iter().any(|label| label.text == "NEXT"));
    }

    #[test]
    fn retargeting_the_cursor_repaints_nothing_but_the_cursor() {
        let graph = graph(
            &["scribe-root", "scribe-mid", "scribe-leaf"],
            &[("scribe-root", "scribe-mid"), ("scribe-mid", "scribe-leaf")],
        );
        let layout = layout_flow(&graph, 1.0).unwrap();
        let opened = present_flow(&graph, &layout, "scribe-root", None, &no_live()).unwrap();
        let retargeted = present_flow(&graph, &layout, "scribe-leaf", None, &no_live()).unwrap();

        assert_eq!(retargeted.nodes.iter().filter(|node| node.cursor).count(), 1);
        assert_eq!(
            retargeted.nodes.iter().find(|node| node.cursor).unwrap().id,
            "scribe-leaf",
            "the cursor lands on the activated node"
        );
        assert_eq!(retargeted.wires, opened.wires, "every wire survives a retarget");
        assert_eq!(retargeted.width.to_bits(), opened.width.to_bits());
        let geometry = |presentation: &FlowPresentation| {
            presentation
                .nodes
                .iter()
                .map(|node| (node.id.clone(), node.rank, node.x.to_bits(), node.y.to_bits()))
                .collect::<Vec<_>>()
        };
        assert_eq!(geometry(&retargeted), geometry(&opened), "no node moves under a retarget");
        let moved: Vec<_> = opened
            .nodes
            .iter()
            .zip(&retargeted.nodes)
            .filter(|(before, after)| before.cursor != after.cursor)
            .map(|(before, _)| before.id.as_str())
            .collect();
        assert_eq!(moved, ["scribe-root", "scribe-leaf"], "only the two ends of the move change");

        // The ruler is deliberately cursor-relative: NOW names the rank the
        // reader is on, so it travels with the cursor while the graph under
        // it stays put.
        let now_rank = |presentation: &FlowPresentation| {
            presentation
                .rank_labels
                .iter()
                .find(|label| label.text == "NOW")
                .map(|label| label.rank)
        };
        assert_eq!(now_rank(&opened), Some(0));
        assert_eq!(now_rank(&retargeted), Some(2));
    }

    #[test]
    fn presentation_rejects_a_cursor_missing_from_the_graph() {
        let graph = graph(&["a", "b"], &[("a", "b")]);
        let layout = layout_flow(&graph, 1.0).unwrap();
        assert_eq!(
            present_flow(&graph, &layout, "not-here", None, &no_live()),
            Err(FlowRenderError::CursorNotInGraph { issue_id: "not-here".into() })
        );
    }

    #[test]
    fn renderer_clamps_only_its_supplied_horizontal_offset() {
        assert_eq!(clamped_scroll(-4.0, 500.0, 200.0).to_bits(), 0.0f32.to_bits());
        assert_eq!(clamped_scroll(999.0, 500.0, 200.0).to_bits(), 300.0f32.to_bits());
        assert_eq!(clamped_scroll(42.0, 200.0, 500.0).to_bits(), 0.0f32.to_bits());
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

    /// The mock's own epic: `.1 -> {.2,.3}`, `.2 -> .5`, `.3 -> .4`,
    /// `{.5,.4} -> .6`, `.6 -> .7`, hovering `.3`.
    fn mock_epic() -> BeadsEpicGraph {
        graph(
            &["2a8z.1", "2a8z.2", "2a8z.3", "2a8z.5", "2a8z.4", "2a8z.6", "2a8z.7"],
            &[
                ("2a8z.1", "2a8z.2"),
                ("2a8z.1", "2a8z.3"),
                ("2a8z.2", "2a8z.5"),
                ("2a8z.3", "2a8z.4"),
                ("2a8z.5", "2a8z.6"),
                ("2a8z.4", "2a8z.6"),
                ("2a8z.6", "2a8z.7"),
            ],
        )
    }

    #[test]
    fn trace_lights_ancestors_and_descendants_and_leaves_a_parallel_branch_dark() {
        let graph = mock_epic();
        let trace = FlowTrace::from_hover(&graph, "2a8z.3").unwrap();
        let mut lit = trace.on_path.iter().cloned().collect::<Vec<_>>();
        lit.sort();
        // Exactly the mock's `on` nodes: the hovered node, its one ancestor,
        // and its three descendants. `.2` and `.5` are the parallel branch.
        assert_eq!(lit, vec!["2a8z.1", "2a8z.3", "2a8z.4", "2a8z.6", "2a8z.7"]);
        assert!(!trace.on_path.contains("2a8z.2"));
        assert!(!trace.on_path.contains("2a8z.5"));
    }

    #[test]
    fn trace_chip_states_the_mock_counts_in_the_mock_wording() {
        let trace = FlowTrace::from_hover(&mock_epic(), "2a8z.3").unwrap();
        // Verbatim from the normative mock's `.fl .unlocks` span. These are
        // closure counts: `.3` has one direct dependent but releases three.
        assert_eq!(trace.chip_text(), "releases 3 · blocked by 1");
    }

    #[test]
    fn trace_dims_every_wire_that_leaves_the_lit_path() {
        let graph = mock_epic();
        let trace = FlowTrace::from_hover(&graph, "2a8z.3").unwrap();
        let traced = graph
            .edges
            .iter()
            .zip(&trace.edge_classes)
            .filter(|(_, class)| **class == WireClass::Traced)
            .map(|(edge, _)| (edge.from.as_str(), edge.to.as_str()))
            .collect::<Vec<_>>();
        // `.1 -> .2` stays dark even though `.1` is lit: one lit endpoint is
        // not a traced relationship. `.5 -> .6` likewise.
        assert_eq!(
            traced,
            vec![
                ("2a8z.1", "2a8z.3"),
                ("2a8z.3", "2a8z.4"),
                ("2a8z.4", "2a8z.6"),
                ("2a8z.6", "2a8z.7"),
            ]
        );
        assert!(trace.edge_classes.contains(&WireClass::Dimmed));
    }

    #[test]
    fn a_traced_edge_lights_only_its_half_of_a_shared_gutter() {
        // Two edges share one rail; only the traced one's interval brightens.
        let runs = [
            EdgeWireRun {
                edge_index: 0,
                axis: WireAxis::Horizontal,
                offset: 20.0,
                start: 0.0,
                end: 40.0,
            },
            EdgeWireRun {
                edge_index: 1,
                axis: WireAxis::Horizontal,
                offset: 20.0,
                start: 20.0,
                end: 60.0,
            },
        ];
        let segments =
            union_wire_runs(
                &runs,
                |edge| {
                    if edge == 1 { WireClass::Traced } else { WireClass::Dimmed }
                },
            );
        assert_eq!(
            segments,
            vec![
                WireSegment {
                    axis: WireAxis::Horizontal,
                    offset: 20.0,
                    start: 0.0,
                    end: 20.0,
                    class: WireClass::Dimmed,
                },
                WireSegment {
                    axis: WireAxis::Horizontal,
                    offset: 20.0,
                    start: 20.0,
                    end: 60.0,
                    class: WireClass::Traced,
                },
            ]
        );
    }

    #[test]
    fn presentation_dims_off_path_nodes_and_gives_the_hovered_node_the_cursor() {
        let graph = mock_epic();
        let layout = layout_flow(&graph, 1.0).unwrap();
        let trace = FlowTrace::from_hover(&graph, "2a8z.3").unwrap();
        // Cursor sits elsewhere, so this also proves hover takes the cursor
        // treatment without stealing the real cursor's.
        let shown = present_flow(&graph, &layout, "2a8z.1", Some(&trace), &no_live()).unwrap();
        let find = |id: &str| shown.nodes.iter().find(|node| node.id == id).cloned().unwrap();
        assert!(find("2a8z.3").on_path && find("2a8z.3").cursor);
        assert!(find("2a8z.1").on_path && find("2a8z.1").cursor);
        assert!(!find("2a8z.2").on_path && !find("2a8z.2").cursor);
        assert!(!find("2a8z.5").on_path);
        assert!(find("2a8z.7").on_path);
    }

    #[test]
    fn chip_sits_under_the_hovered_node_at_the_mock_offsets() {
        let graph = mock_epic();
        let layout = layout_flow(&graph, 1.0).unwrap();
        let trace = FlowTrace::from_hover(&graph, "2a8z.3").unwrap();
        let shown = present_flow(&graph, &layout, "2a8z.3", Some(&trace), &no_live()).unwrap();
        let hovered = shown.nodes.iter().find(|node| node.id == "2a8z.3").unwrap();
        let chip = shown.chip.clone().unwrap();
        assert_eq!(chip.text, "releases 3 · blocked by 1");
        assert!((chip.x - (hovered.x + 14.0)).abs() < f32::EPSILON);
        assert!((chip.y - (hovered.y + hovered.height + 6.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn leaving_restores_every_node_and_wire_in_one_frame() {
        let graph = mock_epic();
        let layout = layout_flow(&graph, 1.0).unwrap();
        // The at-rest frame and the frame after a hover is cleared are the
        // same value, so a pointer-leave repaint restores everything at once
        // rather than easing nodes back independently.
        let at_rest = present_flow(&graph, &layout, "2a8z.3", None, &no_live()).unwrap();
        let trace = FlowTrace::from_hover(&graph, "2a8z.3").unwrap();
        let traced = present_flow(&graph, &layout, "2a8z.3", Some(&trace), &no_live()).unwrap();
        assert_ne!(at_rest, traced);
        let released = present_flow(&graph, &layout, "2a8z.3", None, &no_live()).unwrap();
        assert_eq!(at_rest, released);
        assert!(released.chip.is_none());
        assert!(released.nodes.iter().all(|node| node.on_path));
        assert!(released.wires.iter().all(|wire| wire.class == WireClass::Base));
    }

    #[test]
    fn a_hover_on_a_node_outside_the_graph_traces_nothing() {
        assert!(FlowTrace::from_hover(&mock_epic(), "2a8z.99").is_none());
    }

    #[test]
    fn an_isolated_node_traces_only_itself() {
        let graph = graph(&["alone", "other"], &[]);
        let trace = FlowTrace::from_hover(&graph, "alone").unwrap();
        assert_eq!(trace.chip_text(), "releases 0 · blocked by 0");
        assert_eq!(trace.on_path.len(), 1);
    }

    /// A graph whose nodes all carry a claim, so assignment is held constant
    /// and liveness is the only variable the halo can be reading.
    fn claimed_graph() -> BeadsEpicGraph {
        let mut graph = graph(&["flow-1", "flow-2", "flow-3"], &[("flow-1", "flow-2")]);
        for node in &mut graph.nodes {
            node.assignee = Some("codex-implement-ready-run-20260818T085731.REeEi4".into());
        }
        graph
    }

    fn presented(graph: &BeadsEpicGraph, live: &HashSet<String>) -> Vec<FlowNodePresentation> {
        let layout = layout_flow(graph, 1.0).unwrap();
        present_flow(graph, &layout, "flow-1", None, live).unwrap().nodes
    }

    #[test]
    fn a_live_binding_paints_the_halo_and_agent_line_on_exactly_one_node() {
        let graph = claimed_graph();
        let nodes = presented(&graph, &live_on(&["flow-2"]));

        let live = nodes.iter().filter(|node| node.liveness.is_running()).collect::<Vec<_>>();
        assert_eq!(live.len(), 1, "exactly one node is live");
        assert_eq!(live[0].id, "flow-2");
        assert_eq!(live[0].liveness.agent(), Some("codex"));
        assert!(
            nodes
                .iter()
                .filter(|node| node.id != "flow-2")
                .all(|node| node.liveness.agent().is_none()),
            "no idle node carries an agent line"
        );
    }

    #[test]
    fn assignment_without_liveness_paints_no_halo() {
        let graph = claimed_graph();
        // Every node is claimed; nothing is running.
        let nodes = presented(&graph, &no_live());
        assert!(
            nodes.iter().all(|node| node.liveness == FlowLiveness::Idle),
            "a claim is not a running process, so it earns no halo"
        );
    }

    #[test]
    fn a_binding_for_an_issue_outside_this_graph_paints_nothing() {
        // The frame reached this window, but names an issue no node here has:
        // another region's epic, or an agent working outside the open graph.
        let graph = claimed_graph();
        let nodes = presented(&graph, &live_on(&["flow-99"]));
        assert!(nodes.iter().all(|node| node.liveness == FlowLiveness::Idle));
    }

    #[test]
    fn clearing_the_binding_restores_the_ordinary_treatment() {
        let graph = claimed_graph();
        let lit = presented(&graph, &live_on(&["flow-2"]));
        let cleared = presented(&graph, &no_live());
        assert_ne!(lit, cleared, "the halo is visible while the binding stands");
        assert_eq!(
            cleared,
            presented(&graph, &no_live()),
            "clearing returns the exact pre-halo presentation, in one pass"
        );
        assert!(cleared.iter().all(|node| node.liveness == FlowLiveness::Idle));
    }

    #[test]
    fn a_live_node_without_assignee_data_keeps_the_halo_and_drops_the_line() {
        // bd recorded no assignee, but a session is demonstrably running on
        // the issue. The halo reports the observed process; only the name is
        // missing, and a missing name is not a reason to hide the fact.
        let graph = graph(&["flow-1", "flow-2"], &[("flow-1", "flow-2")]);
        let nodes = presented(&graph, &live_on(&["flow-2"]));
        let live = nodes.iter().find(|node| node.id == "flow-2").unwrap();
        assert!(live.liveness.is_running());
        assert!(live.liveness.agent().is_none(), "no name to show, no placeholder invented");
        assert_eq!(node_accessible_name(live), "flow-2 Node flow-2, ready, running now");
    }

    #[test]
    fn a_live_node_announces_its_agent_to_a_screen_reader() {
        let graph = claimed_graph();
        let nodes = presented(&graph, &live_on(&["flow-2"]));
        let live = nodes.iter().find(|node| node.id == "flow-2").unwrap();
        let idle = nodes.iter().find(|node| node.id == "flow-3").unwrap();
        assert_eq!(node_accessible_name(live), "flow-2 Node flow-2, ready, codex running now");
        assert_eq!(node_accessible_name(idle), "flow-3 Node flow-3, ready");
    }

    #[test]
    fn an_agent_name_is_the_leading_segment_of_a_generated_assignee() {
        assert_eq!(
            agent_name(Some("codex-implement-ready-run-20260818T085731.REeEi4")).as_deref(),
            Some("codex")
        );
        assert_eq!(agent_name(Some("Sharaf Nassar")).as_deref(), Some("Sharaf Nassar"));
        assert_eq!(agent_name(None), None);
        assert_eq!(agent_name(Some("")), None, "an empty claim names nobody");
        assert_eq!(agent_name(Some("   ")), None);
        assert_eq!(agent_name(Some("-leading")), None, "a leading separator names nobody");
    }

    #[test]
    fn liveness_outranks_a_done_nodes_recessive_treatment() {
        // A machine running on a closed issue is still the line worth
        // reading, so the halo is not filtered by queue.
        let mut graph = claimed_graph();
        graph.nodes[1].queue = BeadsIssueQueue::Done;
        let nodes = presented(&graph, &live_on(&["flow-2"]));
        let live = nodes.iter().find(|node| node.id == "flow-2").unwrap();
        assert!(live.liveness.is_running());
        assert_eq!(live.state, FlowNodeState::Done, "the queue state is unchanged");
    }
}
