//! Copy, cut and paste of nodes. Copied nodes go to the system clipboard as TOML, together with
//! the wires between them, so they can also be pasted into another window or kept in a text file.

use crate::model::{Graph, GraphNode, NodeKind, Wire};
use egui::{Pos2, Vec2, pos2};
use egui_snarl::{InPinId, NodeId, OutPinId, Snarl};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// What goes on the clipboard. The marker keeps ordinary text from being taken for nodes.
#[derive(Serialize, Deserialize)]
struct Clip {
    #[serde(rename = "better-sound-patching-nodes")]
    version: u32,
    #[serde(flatten)]
    graph: Graph,
}

/// The selected nodes and the wires between them, as clipboard text.
pub fn copy(snarl: &Snarl<NodeKind>, selected: &[NodeId]) -> Option<String> {
    let chosen: HashSet<NodeId> = selected.iter().copied().filter(|n| snarl.get_node(*n).is_some()).collect();
    if chosen.is_empty() {
        return None;
    }
    let mut nodes: Vec<GraphNode> = snarl
        .nodes_pos_ids()
        .filter(|(id, _, _)| chosen.contains(id))
        .map(|(id, pos, kind)| GraphNode { id: id.0 as u32, pos: [pos.x.round(), pos.y.round()], kind: kind.clone() })
        .collect();
    nodes.sort_by_key(|n| n.id);
    let mut wires: Vec<Wire> = snarl
        .wires()
        .filter(|(o, i)| chosen.contains(&o.node) && chosen.contains(&i.node))
        .map(|(o, i)| Wire { from: o.node.0 as u32, to: i.node.0 as u32, from_pin: o.output as u32, to_pin: i.input as u32 })
        .collect();
    wires.sort();
    toml::to_string_pretty(&Clip { version: 1, graph: Graph { nodes, wires } }).ok()
}

/// Add the nodes in clipboard text to the graph, with their top-left corner at `at` (or next to
/// where they were copied from). Names that must be unique get a free one. Returns the new
/// nodes, or `None` if the text isn't copied nodes.
pub fn paste(snarl: &mut Snarl<NodeKind>, text: &str, at: Option<Pos2>) -> Option<Vec<NodeId>> {
    let clip: Clip = toml::from_str(text).ok()?;
    let nodes = clip.graph.nodes;
    if nodes.is_empty() {
        return None;
    }
    let corner = nodes.iter().fold(pos2(f32::INFINITY, f32::INFINITY), |c, n| pos2(c.x.min(n.pos[0]), c.y.min(n.pos[1])));
    let shift = match at {
        Some(at) => at - corner,
        None => Vec2::splat(40.0),
    };
    let mut taken: HashSet<String> = snarl.nodes().filter_map(|n| n.clone().unique_name_mut().map(|s| s.clone())).collect();
    let mut ids = BTreeMap::new();
    for mut n in nodes {
        if let Some(name) = n.kind.unique_name_mut() {
            *name = free_name(name, &taken);
            taken.insert(name.clone());
        }
        let id = snarl.insert_node(pos2(n.pos[0], n.pos[1]) + shift, n.kind);
        ids.insert(n.id, id);
    }
    for w in clip.graph.wires {
        if let (Some(&a), Some(&b)) = (ids.get(&w.from), ids.get(&w.to)) {
            let (from, to) = (OutPinId { node: a, output: w.from_pin as usize }, InPinId { node: b, input: w.to_pin as usize });
            // Ignore wires to pins the node doesn't have (hand-edited text).
            let fits = snarl.get_node(a).is_some_and(|k| from.output < k.outputs()) && snarl.get_node(b).is_some_and(|k| to.input < k.inputs());
            if fits {
                snarl.connect(from, to);
            }
        }
    }
    Some(ids.into_values().collect())
}

/// `name` if nobody uses it, else `name-2`, `name-3`, ... (a trailing number is counted on from).
fn free_name(name: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(name) {
        return name.to_owned();
    }
    let (base, start) = match name.rsplit_once('-') {
        Some((b, n)) if !b.is_empty() && n.parse::<u32>().is_ok() => (b, n.parse::<u32>().unwrap() + 1),
        _ => (name, 2),
    };
    (start..).map(|k| format!("{base}-{k}")).find(|n| !taken.contains(n)).unwrap()
}

pub fn remove(snarl: &mut Snarl<NodeKind>, nodes: &[NodeId]) {
    for &n in nodes {
        if snarl.get_node(n).is_some() {
            snarl.remove_node(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Channels, EqSpec, SwitchPin};

    #[test]
    fn pasted_nodes_get_free_names_and_keep_only_their_own_wires() {
        let mut snarl = Snarl::new();
        let eq = snarl.insert_node(pos2(0.0, 0.0), NodeKind::Eq(EqSpec::new("eq-mic", Channels::Mono)));
        let sw = snarl.insert_node(pos2(200.0, 50.0), NodeKind::SwitchInput { name: "Mic".into(), pins: vec![SwitchPin::default(); 2] });
        let out = snarl.insert_node(pos2(400.0, 0.0), NodeKind::OtherAppsRecording);
        snarl.connect(OutPinId { node: eq, output: 0 }, InPinId { node: sw, input: 1 });
        snarl.connect(OutPinId { node: sw, output: 0 }, InPinId { node: out, input: 0 });

        let text = copy(&snarl, &[eq, sw]).unwrap();
        let new = paste(&mut snarl, &text, Some(pos2(1000.0, 1000.0))).unwrap();
        assert_eq!(new.len(), 2);
        let names: Vec<String> = new.iter().map(|&n| snarl[n].clone().unique_name_mut().unwrap().clone()).collect();
        assert_eq!(names, ["eq-mic-2", "Mic-2"]);
        assert_eq!(snarl.get_node_info(new[0]).unwrap().pos, pos2(1000.0, 1000.0));
        assert_eq!(snarl.get_node_info(new[1]).unwrap().pos, pos2(1200.0, 1050.0));
        // The wire between the copies comes along, with its pin; the wire to `out` does not.
        let wires: Vec<_> = snarl.wires().filter(|(o, _)| new.contains(&o.node)).collect();
        assert_eq!(wires, [(OutPinId { node: new[0], output: 0 }, InPinId { node: new[1], input: 1 })]);
        // Pasting again counts on.
        let again = paste(&mut snarl, &text, None).unwrap();
        assert_eq!(snarl[again[0]].clone().unique_name_mut().unwrap(), "eq-mic-3");
    }

    #[test]
    fn ordinary_text_is_not_pasted() {
        let mut snarl: Snarl<NodeKind> = Snarl::new();
        assert!(paste(&mut snarl, "hello", None).is_none());
        assert!(paste(&mut snarl, "nodes = []\nwires = []", None).is_none());
        assert_eq!(snarl.nodes().count(), 0);
    }

    #[test]
    fn free_names_count_on_from_a_number() {
        let taken: HashSet<String> = ["levels-1", "levels-2", "Output"].map(String::from).into();
        assert_eq!(free_name("levels-1", &taken), "levels-3");
        assert_eq!(free_name("Output", &taken), "Output-2");
        assert_eq!(free_name("new", &taken), "new");
    }
}
