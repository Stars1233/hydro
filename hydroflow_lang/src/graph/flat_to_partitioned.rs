use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::Span;
use slotmap::{Key, SecondaryMap, SlotMap, SparseSecondaryMap};
use syn::parse_quote;
use syn::spanned::Spanned;

use crate::diagnostic::{Diagnostic, Level};
use crate::union_find::UnionFind;

use super::di_mul_graph::DiMulGraph;
use super::flat_graph::FlatGraph;
use super::ops::{find_node_op_constraints, find_op_op_constraints, DelayType};
use super::partitioned_graph::PartitionedGraph;
use super::{
    get_operator_generics, graph_algorithms, node_color, Color, GraphEdgeId, GraphNodeId,
    GraphSubgraphId, Node, OperatorInstance, PortIndexValue,
};

struct FlatToPartitionedBuilder {
    /// Partitioned graph. We will build the partitions AKA subgraphs over the lifetime of `self`.
    partitioned_graph: PartitionedGraph,

    /// Edges which cross barriers.
    barrier_crossers: SecondaryMap<GraphEdgeId, DelayType>,
}

impl FlatToPartitionedBuilder {
    pub fn from_flat(flat_graph: FlatGraph) -> Self {
        let mut partitioned_graph = PartitionedGraph::unpartitioned_from_flat_graph(flat_graph);
        let barrier_crossers = Self::helper_find_barrier_crossers(&partitioned_graph);
        partitioned_graph.node_color = Self::helper_find_node_color(&partitioned_graph);
        Self {
            partitioned_graph,
            barrier_crossers,
        }
    }

    /// Return a map containing all barrier crossers.
    fn helper_find_barrier_crossers(
        partitioned_graph: &PartitionedGraph,
    ) -> SecondaryMap<GraphEdgeId, DelayType> {
        partitioned_graph
            .edges()
            .filter_map(|(edge_id, (_src, _src_port, dst, dst_port))| {
                let op_constraints = partitioned_graph.node(dst).1?.op_constraints;
                let input_barrier = (op_constraints.input_delaytype_fn)(dst_port)?;
                Some((edge_id, input_barrier))
            })
            .collect()
    }

    fn helper_find_node_color(
        partitioned_graph: &PartitionedGraph,
    ) -> SparseSecondaryMap<GraphNodeId, Color> {
        partitioned_graph
            .nodes()
            .filter_map(|(node_id, node)| {
                let inn_degree = partitioned_graph.degree_in(node_id);
                let out_degree = partitioned_graph.degree_out(node_id);
                let op_color =
                    node_color(matches!(node, Node::Handoff { .. }), inn_degree, out_degree);
                op_color.map(|op_color| (node_id, op_color))
            })
            .collect()
    }

    /// Find subgraph and insert handoffs.
    /// Modifies barrier_crossers so that the edge OUT of an inserted handoff has
    /// the DelayType data.
    fn make_subgraphs(&mut self) {
        // Algorithm:
        // 1. Each node begins as its own subgraph.
        // 2. Collect edges. (Future optimization: sort so edges which should not be split across a handoff come first).
        // 3. For each edge, try to join `(to, from)` into the same subgraph.

        // TODO(mingwei):
        // self.partitioned_graph.assert_valid();

        let (subgraph_unionfind, handoff_edges) = self.helper_find_subgraph_unionfind();

        // Insert handoffs between subgraphs (or on subgraph self-loop edges)
        for edge_id in handoff_edges {
            let (src_id, _src_port, dst_id, _dst_port) = self.partitioned_graph.edge(edge_id);

            // Already has a handoff, no need to insert one.
            let (src_node, _src_port) = self.partitioned_graph.node(src_id);
            let (dst_node, _dst_port) = self.partitioned_graph.node(dst_id);
            if matches!(src_node, Node::Handoff { .. }) || matches!(dst_node, Node::Handoff { .. })
            {
                continue;
            }

            let hoff = Node::Handoff {
                src_span: src_node.span(),
                dst_span: dst_node.span(),
            };
            let (_node_id, out_edge_id) = self
                .partitioned_graph
                .insert_intermediate_node(edge_id, hoff);

            // Update barrier_crossers for inserted node.
            if let Some(delay_type) = self.barrier_crossers.remove(edge_id) {
                self.barrier_crossers.insert(out_edge_id, delay_type);
            }
        }

        // Determine node's subgraph and subgraph's nodes.
        // This list of nodes in each subgraph are to be in topological sort order.
        // Eventually returned directly in the `PartitionedGraph`.
        let (node_subgraph, subgraph_nodes) = self.make_subgraph_collect(subgraph_unionfind);
        self.partitioned_graph.node_subgraph = node_subgraph; // TODO(mingwei): encapsulate
        self.partitioned_graph.subgraph_nodes = subgraph_nodes; // TODO(mingwei): encapsulate
    }

    fn helper_find_subgraph_unionfind(
        &mut self,
    ) -> (UnionFind<GraphNodeId>, BTreeSet<GraphEdgeId>) {
        let mut subgraph_unionfind: UnionFind<GraphNodeId> =
            UnionFind::with_capacity(self.partitioned_graph.nodes().len());
        // Will contain all edges which are handoffs. Starts out with all edges and
        // we remove from this set as we construct subgraphs.
        let mut handoff_edges: BTreeSet<GraphEdgeId> = self
            .partitioned_graph
            .edges()
            .map(|(edge_id, _)| edge_id)
            .collect();
        // Would sort edges here for priority (for now, no sort/priority).

        // Each edge gets looked at in order. However we may not know if a linear
        // chain of operators is PUSH vs PULL until we look at the ends. A fancier
        // algorithm would know to handle linear chains from the outside inward.
        // But instead we just run through the edges in a loop until no more
        // progress is made. Could have some sort of O(N^2) pathological worst
        // case.
        let mut progress = true;
        while progress {
            progress = false;
            for (edge_id, (src, dst)) in self
                .partitioned_graph
                .edges()
                .map(|(edge_id, (src, _srt_port, dst, _dst_port))| (edge_id, (src, dst)))
                .collect::<Vec<_>>()
            {
                // Ignore (1) already added edges as well as (2) new self-cycles.
                if subgraph_unionfind.same_set(src, dst) {
                    // Note this might be triggered even if the edge (src, dst) is not in the subgraph (not case 1).
                    // This prevents self-loops which would violate the in-out tree structure (case 2).
                    // Handoffs will be inserted later for this self-loop.
                    continue;
                }

                // Ignore if would join stratum crossers (next edges).
                if self.barrier_crossers.iter().any(|(edge_id, _)| {
                    let (x_src, _x_src_port, x_dst, _x_dst_port) =
                        self.partitioned_graph.edge(edge_id);
                    (subgraph_unionfind.same_set(x_src, src)
                        && subgraph_unionfind.same_set(x_dst, dst))
                        || (subgraph_unionfind.same_set(x_src, dst)
                            && subgraph_unionfind.same_set(x_dst, src))
                }) {
                    continue;
                }

                if can_connect_colorize(&mut self.partitioned_graph.node_color, src, dst) {
                    // At this point we have selected this edge and its src & dst to be
                    // within a single subgraph.
                    subgraph_unionfind.union(src, dst);
                    assert!(handoff_edges.remove(&edge_id));
                    progress = true;
                }
            }
        }

        (subgraph_unionfind, handoff_edges)
    }

    /// Builds the datastructures for checking which subgraph each node belongs to
    /// after handoffs have already been inserted to partition subgraphs.
    /// This list of nodes in each subgraph are returned in topological sort order.
    fn make_subgraph_collect(
        &self,
        mut subgraph_unionfind: UnionFind<GraphNodeId>,
    ) -> (
        SecondaryMap<GraphNodeId, GraphSubgraphId>,
        SlotMap<GraphSubgraphId, Vec<GraphNodeId>>,
    ) {
        // We want the nodes of each subgraph to be listed in topo-sort order.
        // We could do this on each subgraph, or we could do it all at once on the
        // whole node graph by ignoring handoffs, which is what we do here:
        let topo_sort = graph_algorithms::topo_sort(
            self.partitioned_graph
                .nodes()
                .filter(|&(_, node)| !matches!(node, Node::Handoff { .. }))
                .map(|(node_id, _)| node_id),
            |v| {
                self.partitioned_graph
                    .predecessors(v)
                    .map(|(_pred_edge_id, _pred_port, pred_id)| pred_id)
                    .filter(|&pred_id| {
                        let (pred, _) = self.partitioned_graph.node(pred_id);
                        !matches!(pred, Node::Handoff { .. })
                    })
            },
        );

        let mut grouped_nodes: SecondaryMap<GraphNodeId, Vec<GraphNodeId>> = Default::default();
        for node_id in topo_sort {
            let repr_node = subgraph_unionfind.find(node_id);
            if !grouped_nodes.contains_key(repr_node) {
                grouped_nodes.insert(repr_node, Default::default());
            }
            grouped_nodes[repr_node].push(node_id);
        }

        let mut node_subgraph: SecondaryMap<GraphNodeId, GraphSubgraphId> = Default::default();
        let mut subgraph_nodes: SlotMap<GraphSubgraphId, Vec<GraphNodeId>> = Default::default();
        for (_repr_node, member_nodes) in grouped_nodes {
            subgraph_nodes.insert_with_key(|subgraph_id| {
                for &node_id in member_nodes.iter() {
                    node_subgraph.insert(node_id, subgraph_id);
                }
                member_nodes
            });
        }
        (node_subgraph, subgraph_nodes)
    }

    fn find_subgraph_strata(&mut self) -> Result<(), Diagnostic> {
        // Determine subgraphs's stratum number.
        // Find SCCs ignoring `next_tick()` edges, then do TopoSort on the resulting DAG.
        // (Cycles on cross-stratum negative edges are an error.)

        // Generate a subgraph graph. I.e. each node is a subgraph.
        // Edges are connections between subgraphs, ignoring tick-crossers.
        // TODO: use DiMulGraph here?
        let mut subgraph_preds: BTreeMap<GraphSubgraphId, Vec<GraphSubgraphId>> =
            Default::default();
        let mut subgraph_succs: BTreeMap<GraphSubgraphId, Vec<GraphSubgraphId>> =
            Default::default();

        // Negative (next stratum) connections between subgraphs. (Ignore `next_tick()` connections).
        let mut subgraph_negative_connections: BTreeSet<(GraphSubgraphId, GraphSubgraphId)> =
            Default::default();

        for (node_id, node) in self.partitioned_graph.nodes() {
            if matches!(node, Node::Handoff { .. }) {
                assert_eq!(1, self.partitioned_graph.successors(node_id).count());
                let (succ_edge, _port, succ) =
                    self.partitioned_graph.successors(node_id).next().unwrap();

                // Ignore tick edges.
                if Some(&DelayType::Tick) == self.barrier_crossers.get(succ_edge) {
                    continue;
                }

                assert_eq!(1, self.partitioned_graph.predecessors(node_id).count());
                let (_edge_id, _port, pred) =
                    self.partitioned_graph.predecessors(node_id).next().unwrap();

                let pred_sg = self.partitioned_graph.subgraph(pred).unwrap();
                let succ_sg = self.partitioned_graph.subgraph(succ).unwrap();

                subgraph_preds.entry(succ_sg).or_default().push(pred_sg);
                subgraph_succs.entry(pred_sg).or_default().push(succ_sg);

                if Some(&DelayType::Stratum) == self.barrier_crossers.get(succ_edge) {
                    subgraph_negative_connections.insert((pred_sg, succ_sg));
                }
            }
        }

        let scc = graph_algorithms::scc_kosaraju(
            self.partitioned_graph.subgraphs(),
            |v| subgraph_preds.get(&v).into_iter().flatten().cloned(),
            |u| subgraph_succs.get(&u).into_iter().flatten().cloned(),
        );

        let topo_sort_order = {
            // Condensed each SCC into a single node for toposort.
            let mut condensed_preds: BTreeMap<GraphSubgraphId, Vec<GraphSubgraphId>> =
                Default::default();
            for (u, preds) in subgraph_preds.iter() {
                condensed_preds
                    .entry(scc[u])
                    .or_default()
                    .extend(preds.iter().map(|v| scc[v]));
            }

            graph_algorithms::topo_sort(self.partitioned_graph.subgraphs(), |v| {
                condensed_preds.get(&v).into_iter().flatten().cloned()
            })
        };

        // Each subgraph stratum is the same as it's predecessors. Unless there is a negative edge, then we increment.
        for sg_id in topo_sort_order {
            let stratum = subgraph_preds
                .get(&sg_id)
                .into_iter()
                .flatten()
                .filter_map(|&pred_sg_id| {
                    self.partitioned_graph
                        .subgraph_stratum(pred_sg_id)
                        .map(|stratum| {
                            stratum
                                + (subgraph_negative_connections.contains(&(pred_sg_id, sg_id))
                                    as usize)
                        })
                })
                .max()
                .unwrap_or(0);
            self.partitioned_graph.set_subgraph_stratum(sg_id, stratum);
        }

        // Re-introduce the `next_tick()` edges, ensuring they actually go to the next tick.
        let extra_stratum = self.partitioned_graph.max_stratum().unwrap_or(0) + 1; // Used for `next_tick()` delayer subgraphs.
        for (edge_id, &delay_type) in self.barrier_crossers.iter() {
            let (hoff, _hoff_port, dst, dst_port) = self.partitioned_graph.edge(edge_id);
            // let (hoff, dst) = graph.edge(edge_id).unwrap();
            assert_eq!(1, self.partitioned_graph.predecessors(hoff).count());
            let (_edge, _src_port, src) = self.partitioned_graph.predecessors(hoff).next().unwrap();

            let src_sg = self.partitioned_graph.subgraph(src).unwrap();
            let dst_sg = self.partitioned_graph.subgraph(dst).unwrap();
            let src_stratum = self.partitioned_graph.subgraph_stratum(src_sg);
            let dst_stratum = self.partitioned_graph.subgraph_stratum(dst_sg);
            match delay_type {
                DelayType::Tick => {
                    // If tick edge goes foreward in stratum, need to buffer.
                    // (TODO(mingwei): could use a different kind of handoff.)
                    if src_stratum <= dst_stratum {
                        // We inject a new subgraph between the src/dst which runs as the last stratum
                        // of the tick and therefore delays the data until the next tick.

                        // Before: A (src) -> H -> B (dst)
                        // Then add intermediate identity:
                        let (new_node_id, new_edge_id) =
                            self.partitioned_graph.insert_intermediate_node(
                                edge_id,
                                // TODO(mingwei): Proper span w/ `parse_quote_spanned!`?
                                Node::Operator(parse_quote! { identity() }),
                            );
                        // Intermediate: A (src) -> H -> ID -> B (dst)
                        let hoff = Node::Handoff {
                            src_span: Span::call_site(), // TODO(mingwei): Proper spanning?
                            dst_span: Span::call_site(),
                        };
                        let (_hoff_node_id, _hoff_edge_id) = self
                            .partitioned_graph
                            .insert_intermediate_node(new_edge_id, hoff);
                        // After: A (src) -> H -> ID -> H' -> B (dst)

                        // Set stratum number for new intermediate:
                        // Create subgraph. // TODO(mingwei): encapsulate
                        let new_subgraph_id = self
                            .partitioned_graph
                            .subgraph_nodes
                            .insert(vec![new_node_id]);
                        self.partitioned_graph
                            .node_subgraph
                            .insert(new_node_id, new_subgraph_id);
                        // Assign stratum.
                        self.partitioned_graph
                            .set_subgraph_stratum(new_subgraph_id, extra_stratum);
                    }
                }
                DelayType::Stratum => {
                    // Any negative edges which go onto the same or previous stratum are bad.
                    // Indicates an unbroken negative cycle.
                    if dst_stratum <= src_stratum {
                        return Err(Diagnostic::spanned(dst_port.span(), Level::Error, "Negative edge creates a negative cycle which must be broken with a `next_tick()` operator."));
                    }
                }
            }
        }
        Ok(())
    }

    /// Put `is_external_input: true` operators in separate stratum 0 subgraphs if they are not in stratum 0.
    fn separate_external_inputs(&mut self) {
        let external_input_nodes: Vec<_> = self
            .partitioned_graph
            .nodes()
            // Ensure node is an operator (not a handoff), get constraints spec.
            .filter_map(|(node_id, node)| {
                find_node_op_constraints(node).map(|op_constraints| (node_id, op_constraints))
            })
            // Ensure current `node_id` is an external input.
            .filter(|(_node_id, op_constraints)| op_constraints.is_external_input)
            // Collect just `node_id`s.
            .map(|(node_id, _op_constraints)| node_id)
            // Ignore if operator node is already stratum 0.
            .filter(|&node_id| {
                0 != self
                    .partitioned_graph
                    .subgraph_stratum(self.partitioned_graph.node_subgraph(node_id).unwrap())
                    .unwrap()
            })
            .collect();

        for node_id in external_input_nodes {
            let old_sg_id = self.partitioned_graph.node_subgraph(node_id).unwrap();
            // Remove node from old subgraph.
            {
                // TODO(mingwei): encapsulate
                let old_subgraph_nodes = &mut self.partitioned_graph.subgraph_nodes[old_sg_id];
                let index = old_subgraph_nodes
                    .iter()
                    .position(|&nid| node_id == nid)
                    .unwrap();
                old_subgraph_nodes.remove(index);
            }
            // Create new subgraph in stratum 0 for this source.
            // TODO(mingwei): encapsulate
            let new_sg_id = self.partitioned_graph.subgraph_nodes.insert(vec![node_id]);
            self.partitioned_graph
                .node_subgraph
                .insert(node_id, new_sg_id);
            self.partitioned_graph.subgraph_stratum.insert(new_sg_id, 0);
            // Insert handoff.
            let successor_edges: Vec<_> = self
                .partitioned_graph
                .successors(node_id)
                .map(|(edge_id, _succ_port, _succ_node)| edge_id)
                .collect();
            for edge_id in successor_edges {
                let span = self.partitioned_graph.node(node_id).0.span();
                let hoff = Node::Handoff {
                    src_span: span,
                    dst_span: span,
                };
                self.partitioned_graph
                    .insert_intermediate_node(edge_id, hoff);
            }
        }
    }

    // Find the input (recv) and output (send) handoffs for each subgraph.
    fn helper_find_subgraph_handoffs(
        &self,
    ) -> (
        SecondaryMap<GraphSubgraphId, Vec<GraphNodeId>>,
        SecondaryMap<GraphSubgraphId, Vec<GraphNodeId>>,
    ) {
        // TODO(mingwei): These should be invariants maintained by `PartitionedGraph`.
        {
            // Get data on handoff src and dst subgraphs.
            let mut subgraph_recv_handoffs: SecondaryMap<GraphSubgraphId, Vec<GraphNodeId>> = self
                .partitioned_graph
                .subgraph_nodes
                .keys()
                .map(|k| (k, Default::default()))
                .collect();
            let mut subgraph_send_handoffs = subgraph_recv_handoffs.clone();

            self.partitioned_graph.subgraph_recv_handoffs = subgraph_recv_handoffs;
            self.partitioned_graph.subgraph_send_handoffs = subgraph_send_handoffs;
        }

        // For each edge in the graph, if `src` or `dst` are a handoff then assign
        // that handoff the to neighboring subgraphs (the other of `src`/`dst`).
        // (Mingwei: alternatively, could iterate nodes instead and just look at pred/succ).
        for (_edge_id, (src, _src_port, dst, _dst_port)) in self.partitioned_graph.edges() {
            let (src_node, _src_inst) = self.partitioned_graph.node(src);
            let (dst_node, _dst_inst) = self.partitioned_graph.node(dst);
            match (src_node, dst_node) {
                (Node::Operator(_), Node::Operator(_)) => {}
                (Node::Operator(_), Node::Handoff { .. }) => {
                    // TODO(mingwei): encapsulate
                    self.partitioned_graph.subgraph_send_handoffs
                        [self.partitioned_graph.node_subgraph(src).unwrap()]
                    .push(dst);
                }
                (Node::Handoff { .. }, Node::Operator(_)) => {
                    // TODO(mingwei): encapsulate
                    self.partitioned_graph.subgraph_recv_handoffs
                        [self.partitioned_graph.node_subgraph(dst).unwrap()]
                    .push(src);
                }
                (Node::Handoff { .. }, Node::Handoff { .. }) => {
                    Diagnostic::spanned(
                        Span::call_site(),
                        Level::Error,
                        format!(
                            "Internal Error: Consecutive handoffs {:?} -> {:?}",
                            src.data(),
                            dst.data()
                        ),
                    )
                    .emit();
                }
            }
        }

        (subgraph_recv_handoffs, subgraph_send_handoffs)
    }
}

/// Set `src` or `dst` color if `None` based on the other (if possible):
/// `None` indicates an op could be pull or push i.e. unary-in & unary-out.
/// So in that case we color `src` or `dst` based on its newfound neighbor (the other one).
///
/// Returns if `src` and `dst` can be in the same subgraph.
fn can_connect_colorize(
    node_color: &mut SparseSecondaryMap<GraphNodeId, Color>,
    src: GraphNodeId,
    dst: GraphNodeId,
) -> bool {
    // Pull -> Pull
    // Push -> Push
    // Pull -> [Computation] -> Push
    // Push -> [Handoff] -> Pull
    let can_connect = match (node_color.get(src), node_color.get(dst)) {
        // Linear chain, can't connect because it may cause future conflicts.
        // But if it doesn't in the _future_ we can connect it (once either/both ends are determined).
        (None, None) => false,

        // Infer left side.
        (None, Some(Color::Pull | Color::Comp)) => {
            node_color.insert(src, Color::Pull);
            true
        }
        (None, Some(Color::Push | Color::Hoff)) => {
            node_color.insert(src, Color::Push);
            true
        }

        // Infer right side.
        (Some(Color::Pull | Color::Hoff), None) => {
            node_color.insert(dst, Color::Pull);
            true
        }
        (Some(Color::Comp | Color::Push), None) => {
            node_color.insert(dst, Color::Push);
            true
        }

        // Both sides already specified.
        (Some(Color::Pull), Some(Color::Pull)) => true,
        (Some(Color::Pull), Some(Color::Comp)) => true,
        (Some(Color::Pull), Some(Color::Push)) => true,

        (Some(Color::Comp), Some(Color::Pull)) => false,
        (Some(Color::Comp), Some(Color::Comp)) => false,
        (Some(Color::Comp), Some(Color::Push)) => true,

        (Some(Color::Push), Some(Color::Pull)) => false,
        (Some(Color::Push), Some(Color::Comp)) => false,
        (Some(Color::Push), Some(Color::Push)) => true,

        // Handoffs are not part of subgraphs.
        (Some(Color::Hoff), Some(_)) => false,
        (Some(_), Some(Color::Hoff)) => false,
    };
    can_connect
}

impl TryFrom<FlatGraph> for PartitionedGraph {
    type Error = Diagnostic;

    fn try_from(flat_graph: FlatGraph) -> Result<Self, Self::Error> {
        let mut builder = FlatToPartitionedBuilder::from_flat(flat_graph);

        // Partition into subgraphs.
        builder.make_subgraphs();

        // Find strata for subgraphs.
        if let Err(diagnostic) = builder.find_subgraph_strata() {
            diagnostic.emit();
        }

        // Ensure all external inputs are in stratum 0.
        builder.separate_external_inputs();

        let (subgraph_recv_handoffs, subgraph_send_handoffs) =
            builder.helper_find_subgraph_handoffs();

        // TODO(mingwei): WIP: keep using `FlatToPartitionedBuilder` instead of exploding.
        let PartitionedGraph {
            nodes,
            operator_instances,
            graph,
            ports,
            node_subgraph,
            subgraph_nodes,
            subgraph_stratum,
            subgraph_recv_handoffs: _,
            subgraph_send_handoffs: _,
            subgraph_internal_handoffs: _,
            node_color,
            node_varnames,
        } = builder.partitioned_graph;

        let mut subgraph_internal_handoffs: SecondaryMap<GraphSubgraphId, Vec<GraphNodeId>> =
            SecondaryMap::new();

        // build a SecondaryMap from edges.dst to edges to find inbound edges of a node
        let mut dest_map = SecondaryMap::new();
        graph.edges().for_each(|e| {
            let (_, (_src, dest)) = e;
            dest_map.insert(dest, e);
        });
        // iterate through edges, find internal handoffs and their inbound/outbound edges
        for e in graph.edges() {
            let (src, dst) = e.1;
            if let Node::Handoff { .. } = nodes[src] {
                if let Some((_, (inbound_src, _inbound_dest))) = dest_map.get(src) {
                    // Found an inbound edge to this handoff. Check if it's in the same subgraph as dst
                    if let Some(inbound_src_subgraph) = node_subgraph.get(*inbound_src) {
                        if let Some(dst_subgraph) = node_subgraph.get(dst) {
                            if inbound_src_subgraph == dst_subgraph {
                                // Found an internal handoff
                                if let Node::Handoff { .. } = nodes[src] {
                                    subgraph_internal_handoffs
                                        .entry(*inbound_src_subgraph)
                                        .unwrap()
                                        .or_insert(Vec::new())
                                        .push(src);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(PartitionedGraph {
            nodes,
            operator_instances,
            graph,
            ports,
            node_subgraph,

            subgraph_nodes,
            subgraph_stratum,
            subgraph_recv_handoffs,
            subgraph_send_handoffs,
            subgraph_internal_handoffs,
            node_color,

            node_varnames,
        })
    }
}

/// `edge`: X
///
/// Before: A (src) ------------> B (dst)
/// After:  A (src) -> X (new) -> B (dst)
///
/// Returns the ID of X & ID of edge OUT of X.
fn insert_intermediate_node(
    nodes: &mut SlotMap<GraphNodeId, Node>,
    operator_instances: &mut SecondaryMap<GraphNodeId, OperatorInstance>,
    ports: &mut SecondaryMap<GraphEdgeId, (PortIndexValue, PortIndexValue)>,
    graph: &mut DiMulGraph<GraphNodeId, GraphEdgeId>,
    node: Node,
    edge_id: GraphEdgeId,
) -> (GraphNodeId, GraphEdgeId) {
    let span = Some(node.span());

    // Make corresponding operator instance (if `node` is an operator).
    let op_inst_opt = 'oc: {
        let Node::Operator(operator) = &node else { break 'oc None; };
        let Some(op_constraints) = find_op_op_constraints(operator) else { break 'oc None; };
        let (input_port, output_port) = ports.get(edge_id).cloned().unwrap();
        let generics = get_operator_generics(
            &mut Vec::new(), /* TODO(mingwei) diagnostics */
            operator,
        );
        Some(OperatorInstance {
            op_constraints,
            input_ports: vec![input_port],
            output_ports: vec![output_port],
            generics,
            arguments: operator.args.clone(),
        })
    };

    // Insert new `node`.
    let node_id = nodes.insert(node);
    // Insert corresponding `OperatorInstance` if applicable.
    if let Some(op_inst) = op_inst_opt {
        operator_instances.insert(node_id, op_inst);
    }
    // Update edges to insert node within `edge_id`.
    let (e0, e1) = graph.insert_intermediate_vertex(node_id, edge_id).unwrap();

    // Update corresponding ports.
    let (src_idx, dst_idx) = ports.remove(edge_id).unwrap();
    ports.insert(e0, (src_idx, PortIndexValue::Elided(span)));
    ports.insert(e1, (PortIndexValue::Elided(span), dst_idx));

    (node_id, e1)
}
