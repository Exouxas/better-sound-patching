//! Turns a profile plus the live graph into the exact set of port links that should exist.
//!
//! This module is pure: no PipeWire calls, so it is fully unit-testable.

use crate::live::{Dir, Live, LivePort};
use crate::model::{BusSpec, NodeKind, Profile};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// (output port id, input port id)
pub type PortPair = (u32, u32);

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Virtual devices the profile needs to exist.
    pub buses: Vec<BusSpec>,
    /// Links that must exist.
    pub links: BTreeSet<PortPair>,
    /// Existing links (by link id) that conflict with the profile and must be removed.
    pub remove: Vec<u32>,
    /// Live node ids that this profile controls exclusively.
    pub managed: BTreeSet<u32>,
    /// Graph nodes whose device is currently absent, for display.
    pub missing: Vec<u32>,
}

/// Which live nodes a graph node stands for right now.
pub fn resolve(profile: &Profile, live: &Live) -> BTreeMap<u32, Vec<u32>> {
    let assigned: HashSet<&str> = profile
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Input { device: Some(d) } | NodeKind::Output { device: Some(d) } => {
                Some(d.node_name.as_str())
            }
            NodeKind::Bus(b) => Some(b.name.as_str()),
            _ => None,
        })
        .collect();

    let by_name = |name: &str| -> Vec<u32> {
        live.nodes.values().filter(|n| n.name == name).map(|n| n.id).collect()
    };

    profile
        .nodes
        .iter()
        .map(|gn| {
            let ids = match &gn.kind {
                NodeKind::Input { device: Some(d) } | NodeKind::Output { device: Some(d) } => {
                    by_name(&d.node_name)
                }
                NodeKind::Input { device: None } | NodeKind::Output { device: None } => vec![],
                NodeKind::Bus(b) => by_name(&b.name),
                NodeKind::FallbackInput => live
                    .nodes
                    .values()
                    .filter(|n| n.is_hardware() && n.is_source() && !assigned.contains(n.name.as_str()))
                    .map(|n| n.id)
                    .collect(),
                NodeKind::FallbackOutput => live
                    .nodes
                    .values()
                    .filter(|n| n.is_hardware() && n.is_sink() && !assigned.contains(n.name.as_str()))
                    .map(|n| n.id)
                    .collect(),
            };
            (gn.id, ids)
        })
        .collect()
}

/// Pair output ports with input ports by channel position.
///
/// - mono → many: the mono channel feeds every input
/// - many → mono: every channel is mixed into the mono input
/// - otherwise channels are matched by name (FL→FL, FR→FR); if no names match at all,
///   ports are paired by position.
pub fn map_channels(outs: &[&LivePort], ins: &[&LivePort]) -> Vec<PortPair> {
    if outs.is_empty() || ins.is_empty() {
        return vec![];
    }
    if outs.len() == 1 {
        return ins.iter().map(|i| (outs[0].id, i.id)).collect();
    }
    if ins.len() == 1 {
        return outs.iter().map(|o| (o.id, ins[0].id)).collect();
    }
    let mut pairs: Vec<PortPair> = outs
        .iter()
        .flat_map(|o| {
            ins.iter()
                .filter(move |i| !o.channel.is_empty() && i.channel == o.channel)
                .map(move |i| (o.id, i.id))
        })
        .collect();
    if pairs.is_empty() {
        pairs = outs
            .iter()
            .enumerate()
            .map(|(k, o)| (o.id, ins[k % ins.len()].id))
            .collect();
    }
    pairs
}

pub fn plan(profile: &Profile, live: &Live) -> Plan {
    let resolved = resolve(profile, live);

    let buses: Vec<BusSpec> = profile
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Bus(b) => Some(b.clone()),
            _ => None,
        })
        .collect();

    let mut links = BTreeSet::new();
    for w in &profile.wires {
        let (Some(from), Some(to)) = (resolved.get(&w.from), resolved.get(&w.to)) else {
            continue;
        };
        for &a in from {
            for &b in to {
                if a == b {
                    continue;
                }
                let outs = live.ports_of(a, Dir::Out);
                let ins = live.ports_of(b, Dir::In);
                links.extend(map_channels(&outs, &ins));
            }
        }
    }

    // Everything the profile mentions plus all hardware endpoints is ours to control.
    // App streams are never managed: WirePlumber keeps routing apps to the default device.
    let mut managed: BTreeSet<u32> = resolved.values().flatten().copied().collect();
    managed.extend(live.nodes.values().filter(|n| n.is_hardware()).map(|n| n.id));

    // A link is ours to judge when it joins two audio devices (real or virtual) and at least
    // one of them is managed. Links to app streams and to the inside of effect chains (filter
    // nodes have no device media class) are left alone.
    let endpoint = |id: u32| live.nodes.get(&id).is_some_and(|n| n.is_audio_endpoint());
    let remove = live
        .links
        .values()
        .filter(|l| managed.contains(&l.out_node) || managed.contains(&l.in_node))
        .filter(|l| endpoint(l.out_node) && endpoint(l.in_node))
        .filter(|l| !links.contains(&(l.out_port, l.in_port)))
        .map(|l| l.id)
        .collect();

    let missing = profile
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Input { device: Some(_) } | NodeKind::Output { device: Some(_) }))
        .filter(|n| resolved.get(&n.id).is_none_or(|v| v.is_empty()))
        .map(|n| n.id)
        .collect();

    Plan { buses, links, remove, managed, missing }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveLink, LiveNode};
    use crate::model::*;

    struct B {
        live: Live,
        next: u32,
    }

    impl B {
        fn new() -> Self {
            B { live: Live::default(), next: 100 }
        }
        fn node(&mut self, name: &str, class: &str, hw: bool, ins: &[&str], outs: &[&str]) -> u32 {
            let id = self.next;
            self.next += 1;
            self.live.nodes.insert(
                id,
                LiveNode {
                    id,
                    name: name.into(),
                    description: name.into(),
                    media_class: class.into(),
                    device_id: hw.then_some(1),
                },
            );
            for (dir, chans) in [(Dir::In, ins), (Dir::Out, outs)] {
                for (k, ch) in chans.iter().enumerate() {
                    let pid = self.next;
                    self.next += 1;
                    self.live.ports.insert(
                        pid,
                        LivePort {
                            id: pid,
                            node_id: id,
                            name: format!("{ch}"),
                            dir,
                            channel: ch.to_string(),
                            index: k as u32,
                        },
                    );
                }
            }
            id
        }
        fn port(&self, node: u32, dir: Dir, ch: &str) -> u32 {
            self.live
                .ports
                .values()
                .find(|p| p.node_id == node && p.dir == dir && p.channel == ch)
                .unwrap()
                .id
        }
        fn link(&mut self, out_port: u32, in_port: u32) -> u32 {
            let id = self.next;
            self.next += 1;
            let out_node = self.live.ports[&out_port].node_id;
            let in_node = self.live.ports[&in_port].node_id;
            self.live.links.insert(id, LiveLink { id, out_port, in_port, out_node, in_node });
            id
        }
    }

    fn dev(name: &str) -> Option<DeviceRef> {
        Some(DeviceRef { node_name: name.into(), description: name.into() })
    }
    fn n(id: u32, kind: NodeKind) -> GraphNode {
        GraphNode { id, pos: [0.0, 0.0], kind }
    }
    fn bus(name: &str, kind: BusKind, channels: Channels) -> NodeKind {
        NodeKind::Bus(BusSpec { name: name.into(), kind, channels })
    }

    #[test]
    fn stereo_bus_to_headset_and_mono_mic_to_virtual_mic() {
        let mut b = B::new();
        let sys_in = b.node("system-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let head = b.node("head", "Audio/Sink", true, &["FL", "FR"], &["FL", "FR"]);
        let mic = b.node("mic", "Audio/Source", true, &[], &["MONO"]);
        let sys_out = b.node("system-out", "Audio/Source/Virtual", false, &["MONO"], &["MONO"]);
        let p = Profile {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, NodeKind::Output { device: dev("head") }),
                n(3, NodeKind::Input { device: dev("mic") }),
                n(4, bus("system-out", BusKind::Microphone, Channels::Mono)),
            ],
            wires: vec![Wire { from: 1, to: 2 }, Wire { from: 3, to: 4 }],
        };
        let plan = plan(&p, &b.live);
        let expect: BTreeSet<_> = [
            (b.port(sys_in, Dir::Out, "FL"), b.port(head, Dir::In, "FL")),
            (b.port(sys_in, Dir::Out, "FR"), b.port(head, Dir::In, "FR")),
            (b.port(mic, Dir::Out, "MONO"), b.port(sys_out, Dir::In, "MONO")),
        ]
        .into();
        assert_eq!(plan.links, expect);
        assert!(plan.missing.is_empty());
    }

    #[test]
    fn mono_source_feeds_both_stereo_channels() {
        let mut b = B::new();
        let mic = b.node("mic", "Audio/Source", true, &[], &["MONO"]);
        let head = b.node("head", "Audio/Sink", true, &["FL", "FR"], &[]);
        let p = Profile {
            nodes: vec![n(1, NodeKind::Input { device: dev("mic") }), n(2, NodeKind::Output { device: dev("head") })],
            wires: vec![Wire { from: 1, to: 2 }],
        };
        let plan = plan(&p, &b.live);
        assert_eq!(plan.links.len(), 2);
        assert!(plan.links.contains(&(b.port(mic, Dir::Out, "MONO"), b.port(head, Dir::In, "FR"))));
    }

    #[test]
    fn fallback_covers_only_unassigned_hardware() {
        let mut b = B::new();
        let _known = b.node("known-mic", "Audio/Source", true, &[], &["MONO"]);
        let yeti = b.node("yeti", "Audio/Source", true, &[], &["FL", "FR"]);
        let _virt = b.node("easyeffects_source", "Audio/Source/Virtual", false, &[], &["FL", "FR"]);
        let sys_out = b.node("system-out", "Audio/Source/Virtual", false, &["MONO"], &["MONO"]);
        let p = Profile {
            nodes: vec![
                // An Input node with no wires: the device is "set" so fallback skips it.
                n(1, NodeKind::Input { device: dev("known-mic") }),
                n(2, NodeKind::FallbackInput),
                n(3, bus("system-out", BusKind::Microphone, Channels::Mono)),
            ],
            wires: vec![Wire { from: 2, to: 3 }],
        };
        let plan = plan(&p, &b.live);
        let expect: BTreeSet<_> = [
            (b.port(yeti, Dir::Out, "FL"), b.port(sys_out, Dir::In, "MONO")),
            (b.port(yeti, Dir::Out, "FR"), b.port(sys_out, Dir::In, "MONO")),
        ]
        .into();
        assert_eq!(plan.links, expect);
    }

    #[test]
    fn stale_links_between_managed_nodes_are_removed_but_app_links_kept() {
        let mut b = B::new();
        let sys_in = b.node("system-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let head = b.node("head", "Audio/Sink", true, &["FL", "FR"], &[]);
        let speakers = b.node("speakers", "Audio/Sink", true, &["FL", "FR"], &[]);
        let app = b.node("firefox", "Stream/Output/Audio", false, &[], &["FL", "FR"]);
        let filter = b.node("ee_filter", "", false, &["FL", "FR"], &["FL", "FR"]);
        let stale = b.link(b.port(sys_in, Dir::Out, "FL"), b.port(speakers, Dir::In, "FL"));
        let app_link = b.link(b.port(app, Dir::Out, "FL"), b.port(sys_in, Dir::In, "FL"));
        let filter_link = b.link(b.port(sys_in, Dir::Out, "FR"), b.port(filter, Dir::In, "FR"));
        let wanted = b.link(b.port(sys_in, Dir::Out, "FL"), b.port(head, Dir::In, "FL"));
        // Leftover qpwgraph link into EasyEffects' virtual sink: would double the audio.
        let ee_sink = b.node("easyeffects_sink", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let leftover = b.link(b.port(sys_in, Dir::Out, "FR"), b.port(ee_sink, Dir::In, "FR"));
        // Unrelated virtual devices talking to each other are none of our business.
        let other_a = b.node("other-a", "Audio/Sink", false, &[], &["FL"]);
        let other_b = b.node("other-b", "Audio/Sink", false, &["FL"], &[]);
        let unrelated = b.link(b.port(other_a, Dir::Out, "FL"), b.port(other_b, Dir::In, "FL"));
        let p = Profile {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, NodeKind::Output { device: dev("head") }),
            ],
            wires: vec![Wire { from: 1, to: 2 }],
        };
        let plan = plan(&p, &b.live);
        let mut removed = plan.remove.clone();
        removed.sort();
        assert_eq!(removed, vec![stale, leftover]);
        let _ = (app_link, filter_link, wanted, unrelated);
    }

    #[test]
    fn missing_device_is_reported_and_produces_no_links() {
        let mut b = B::new();
        b.node("system-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let p = Profile {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, NodeKind::Output { device: dev("valve-index") }),
            ],
            wires: vec![Wire { from: 1, to: 2 }],
        };
        let plan = plan(&p, &b.live);
        assert!(plan.links.is_empty());
        assert_eq!(plan.missing, vec![2]);
    }
}
