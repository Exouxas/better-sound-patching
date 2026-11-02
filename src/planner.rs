//! Turns the routing graph plus the live PipeWire graph into the exact set of port links that
//! should exist.
//!
//! This module is pure: no PipeWire calls, so it is fully unit-testable.

use crate::live::{Dir, Live, LivePort};
use crate::model::{BusKind, BusSpec, Channels, FilterSpec, Graph, GraphNode, NodeKind, Selections, VolumeSpec, Wire};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// (output port id, input port id)
pub type PortPair = (u32, u32);

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Virtual devices the profile needs to exist.
    pub buses: Vec<BusSpec>,
    /// Processing nodes (EQ, levels, volume) that need to run.
    pub filters: Vec<FilterSpec>,
    /// Links that must exist.
    pub links: BTreeSet<PortPair>,
    /// Existing links (by link id) that conflict with the profile and must be removed.
    pub remove: Vec<u32>,
    /// Live node ids that this profile controls exclusively.
    pub managed: BTreeSet<u32>,
    /// Graph nodes whose device or app is currently absent, for display.
    pub missing: Vec<u32>,
    /// App streams this profile routes itself instead of leaving them to WirePlumber.
    pub app_streams: BTreeSet<u32>,
    /// What the "Other apps" nodes want as system default output / input (live node ids).
    pub default_sink: Option<u32>,
    pub default_source: Option<u32>,
    /// The pin each switch node uses (None: it has no pins).
    pub active_slots: BTreeMap<u32, Option<u32>>,
}

fn app_claims(profile: &Graph) -> HashSet<&str> {
    profile
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::App { app: Some(a) } => Some(a.binary.as_str()),
            _ => None,
        })
        .collect()
}

/// Which live nodes a graph node stands for right now.
pub fn resolve(profile: &Graph, live: &Live) -> BTreeMap<u32, Vec<u32>> {
    let claimed = app_claims(profile);
    let by_name = |name: &str| -> Vec<u32> { live.nodes.values().filter(|n| n.name == name).map(|n| n.id).collect() };
    let streams = |playback: bool| -> Vec<u32> {
        live.nodes
            .values()
            .filter(|n| n.is_app_stream() && n.is_playback_stream() == playback && !claimed.contains(n.app_binary.as_str()))
            .map(|n| n.id)
            .collect()
    };
    profile
        .nodes
        .iter()
        .map(|gn| {
            let ids = match &gn.kind {
                NodeKind::Input { device: Some(d) } | NodeKind::Output { device: Some(d) } => by_name(&d.node_name),
                k if k.owned_name().is_some() => by_name(k.owned_name().unwrap()),
                NodeKind::App { app: Some(a) } => live
                    .nodes
                    .values()
                    .filter(|n| n.is_app_stream() && n.app_binary == a.binary)
                    .map(|n| n.id)
                    .collect(),
                NodeKind::OtherAppsPlayback => streams(true),
                NodeKind::OtherAppsRecording => streams(false),
                _ => vec![],
            };
            (gn.id, ids)
        })
        .collect()
}

/// The pin a switch uses: the selected one, else the first. None if it has no pins.
pub fn selected_pin(kind: &NodeKind, selected: &Selections) -> Option<u32> {
    let (name, pins) = kind.switch()?;
    let k = selected.get(name).copied().unwrap_or(0);
    (!pins.is_empty()).then(|| k.min(pins.len() as u32 - 1))
}

/// Wires seen through switches: each real node paired with the real nodes its audio actually
/// reaches right now.
struct Routing<'a> {
    graph: &'a Graph,
    kinds: BTreeMap<u32, &'a NodeKind>,
    active: BTreeMap<u32, Option<u32>>,
}

const MAX_DEPTH: u32 = 8;

impl Routing<'_> {
    /// Real nodes reached from `id`'s output, following switches.
    fn targets(&self, id: u32, depth: u32, out: &mut Vec<u32>) {
        if depth > MAX_DEPTH {
            return;
        }
        let only_pin = match self.kinds.get(&id) {
            Some(NodeKind::SwitchOutput { .. }) => match self.active.get(&id).copied().flatten() {
                Some(k) => Some(k),
                None => return,
            },
            _ => None,
        };
        for w in self.graph.wires.iter().filter(|w| w.from == id && only_pin.is_none_or(|k| w.from_pin == k)) {
            match self.kinds.get(&w.to) {
                Some(NodeKind::SwitchOutput { .. }) => self.targets(w.to, depth + 1, out),
                Some(NodeKind::SwitchInput { .. }) => {
                    if self.active.get(&w.to).copied().flatten() == Some(w.to_pin) {
                        self.targets(w.to, depth + 1, out);
                    }
                }
                Some(_) => out.push(w.to),
                None => {}
            }
        }
    }

    fn pairs(&self) -> BTreeSet<(u32, u32)> {
        let mut pairs = BTreeSet::new();
        for (&id, kind) in &self.kinds {
            if kind.is_switch() {
                continue;
            }
            let mut t = vec![];
            self.targets(id, 0, &mut t);
            pairs.extend(t.into_iter().filter(|&to| to != id).map(|to| (id, to)));
        }
        pairs
    }
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
        pairs = outs.iter().enumerate().map(|(k, o)| (o.id, ins[k % ins.len()].id)).collect();
    }
    pairs
}

/// Name of the hidden gain stage after a virtual output.
pub fn master_name(bus: &str) -> String {
    format!("master-{bus}")
}

/// The graph with the master volume in it: a hidden Volume stage right after every virtual
/// output that feeds anything, so everything played into the virtual outputs gets one gain.
/// The stages are always there (even at 100%), so changing the master only retunes them.
pub fn with_master(g: &Graph, gain: f32) -> Graph {
    let mut out = g.clone();
    let mut next = g.nodes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
    for n in &g.nodes {
        let NodeKind::Bus(b) = &n.kind else { continue };
        if b.kind != BusKind::Playback || !g.wires.iter().any(|w| w.from == n.id) {
            continue;
        }
        let id = next;
        next += 1;
        let channels = if b.channels == Channels::Mono { Channels::Mono } else { Channels::Stereo };
        out.nodes.push(GraphNode {
            id,
            pos: n.pos,
            kind: NodeKind::Volume(VolumeSpec { name: master_name(&b.name), channels, gain }),
        });
        for w in out.wires.iter_mut().filter(|w| w.from == n.id) {
            w.from = id;
        }
        out.wires.push(Wire { from: n.id, to: id, from_pin: 0, to_pin: 0 });
    }
    out
}

pub fn plan(profile: &Graph, live: &Live, selected: &Selections) -> Plan {
    let resolved = resolve(profile, live);
    let kinds: BTreeMap<u32, &NodeKind> = profile.nodes.iter().map(|n| (n.id, &n.kind)).collect();
    let active_slots: BTreeMap<u32, Option<u32>> =
        kinds.iter().filter(|(_, k)| k.is_switch()).map(|(&id, k)| (id, selected_pin(k, selected))).collect();
    let routing = Routing { graph: profile, kinds: kinds.clone(), active: active_slots.clone() };
    let pairs = routing.pairs();

    let buses: Vec<BusSpec> = profile
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Bus(b) => Some(b.clone()),
            _ => None,
        })
        .collect();
    let filters: Vec<FilterSpec> = profile.nodes.iter().filter_map(|n| n.kind.filter()).collect();

    // "Other apps" nodes don't link anything: they pick the system defaults, which every
    // program without its own App node follows (WirePlumber does that part).
    let single = |node: u32, want: fn(&crate::live::LiveNode) -> bool| -> Option<u32> {
        let ids = resolved.get(&node)?;
        ids.iter().copied().find(|id| live.nodes.get(id).is_some_and(want))
    };
    let is = |id: u32, f: fn(&NodeKind) -> bool| kinds.get(&id).is_some_and(|k| f(k));
    let default_sink = pairs
        .iter()
        .filter(|(a, _)| is(*a, |k| matches!(k, NodeKind::OtherAppsPlayback)))
        .find_map(|&(_, b)| single(b, |n| n.is_sink()));
    let default_source = pairs
        .iter()
        .filter(|(_, b)| is(*b, |k| matches!(k, NodeKind::OtherAppsRecording)))
        .find_map(|&(a, _)| single(a, |n| n.is_source()));

    // An app's playback is ours to route only if its App node's out is wired, and likewise for
    // recording; otherwise WirePlumber keeps doing what it does.
    let mut app_streams = BTreeSet::new();
    for n in &profile.nodes {
        if !matches!(n.kind, NodeKind::App { app: Some(_) }) {
            continue;
        }
        let routes_out = pairs.iter().any(|(a, _)| *a == n.id);
        let routes_in = pairs.iter().any(|(_, b)| *b == n.id);
        for &id in resolved.get(&n.id).into_iter().flatten() {
            let playback = live.nodes.get(&id).is_some_and(|l| l.is_playback_stream());
            if (playback && routes_out) || (!playback && routes_in) {
                app_streams.insert(id);
            }
        }
    }

    let other_apps = |id: u32| is(id, |k| matches!(k, NodeKind::OtherAppsPlayback | NodeKind::OtherAppsRecording));
    let mut links = BTreeSet::new();
    for &(from, to) in pairs.iter().filter(|(a, b)| !other_apps(*a) && !other_apps(*b)) {
        for &a in resolved.get(&from).into_iter().flatten() {
            for &b in resolved.get(&to).into_iter().flatten() {
                if a == b {
                    continue;
                }
                links.extend(map_channels(&live.ports_of(a, Dir::Out), &live.ports_of(b, Dir::In)));
            }
        }
    }

    // Everything the profile mentions, all hardware (unused hardware gets nothing, like in
    // Voicemeeter) and the app streams we took over are ours to control.
    let mut managed: BTreeSet<u32> = profile
        .nodes
        .iter()
        .filter(|n| !matches!(n.kind, NodeKind::App { .. } | NodeKind::OtherAppsPlayback | NodeKind::OtherAppsRecording))
        .flat_map(|n| resolved.get(&n.id).into_iter().flatten().copied())
        .collect();
    managed.extend(live.nodes.values().filter(|n| n.is_hardware()).map(|n| n.id));
    managed.extend(&app_streams);

    // A link is ours to judge when it touches something we manage and joins two audio devices
    // (real or virtual), or touches an app stream we took over. Other apps' links and the
    // inside of other programs' effect chains are left alone.
    let endpoint = |id: u32| live.nodes.get(&id).is_some_and(|n| n.is_audio_endpoint());
    let remove = live
        .links
        .values()
        .filter(|l| managed.contains(&l.out_node) || managed.contains(&l.in_node))
        .filter(|l| {
            (endpoint(l.out_node) && endpoint(l.in_node))
                || app_streams.contains(&l.out_node)
                || app_streams.contains(&l.in_node)
        })
        .filter(|l| !links.contains(&(l.out_port, l.in_port)))
        .map(|l| l.id)
        .collect();

    let missing = profile
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                NodeKind::Input { device: Some(_) } | NodeKind::Output { device: Some(_) } | NodeKind::App { app: Some(_) }
            )
        })
        .filter(|n| resolved.get(&n.id).is_none_or(|v| v.is_empty()))
        .map(|n| n.id)
        .collect();

    Plan { buses, filters, links, remove, managed, missing, app_streams, default_sink, default_source, active_slots }
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
                    app_binary: String::new(),
                    app_name: String::new(),
                    client_id: None,
                    meter: false,
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
                            monitor: false,
                        },
                    );
                }
            }
            id
        }
        fn app(&mut self, binary: &str, playback: bool) -> u32 {
            let (class, ins, outs): (&str, &[&str], &[&str]) = if playback {
                ("Stream/Output/Audio", &[], &["FL", "FR"])
            } else {
                ("Stream/Input/Audio", &["MONO"], &[])
            };
            let id = self.node(binary, class, false, ins, outs);
            self.live.nodes.get_mut(&id).unwrap().app_binary = binary.into();
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
        NodeKind::Bus(BusSpec::new(name, kind, channels))
    }

    #[test]
    fn stereo_bus_to_headset_and_mono_mic_to_virtual_mic() {
        let mut b = B::new();
        let sys_in = b.node("system-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let head = b.node("head", "Audio/Sink", true, &["FL", "FR"], &["FL", "FR"]);
        let mic = b.node("mic", "Audio/Source", true, &[], &["MONO"]);
        let sys_out = b.node("system-out", "Audio/Source/Virtual", false, &["MONO"], &["MONO"]);
        let p = Graph {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, NodeKind::Output { device: dev("head") }),
                n(3, NodeKind::Input { device: dev("mic") }),
                n(4, bus("system-out", BusKind::Microphone, Channels::Mono)),
            ],
            wires: vec![Wire::new(1, 2), Wire::new(3, 4)],
        };
        let plan = plan(&p, &b.live, &Selections::new());
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
        let p = Graph {
            nodes: vec![n(1, NodeKind::Input { device: dev("mic") }), n(2, NodeKind::Output { device: dev("head") })],
            wires: vec![Wire::new(1, 2)],
        };
        let plan = plan(&p, &b.live, &Selections::new());
        assert_eq!(plan.links.len(), 2);
        assert!(plan.links.contains(&(b.port(mic, Dir::Out, "MONO"), b.port(head, Dir::In, "FR"))));
    }

    #[test]
    fn unused_hardware_gets_nothing() {
        let mut b = B::new();
        let webcam = b.node("webcam", "Audio/Source", true, &[], &["FL", "FR"]);
        let sys_out = b.node("system-out", "Audio/Source/Virtual", false, &["MONO"], &["MONO"]);
        let stray = b.link(b.port(webcam, Dir::Out, "FL"), b.port(sys_out, Dir::In, "MONO"));
        let p = Graph { nodes: vec![n(1, bus("system-out", BusKind::Microphone, Channels::Mono))], wires: vec![] };
        let plan = plan(&p, &b.live, &Selections::new());
        assert!(plan.links.is_empty());
        assert_eq!(plan.remove, vec![stray]);
    }

    fn pins(n: usize) -> Vec<SwitchPin> {
        vec![SwitchPin::default(); n]
    }

    #[test]
    fn switch_output_uses_only_the_selected_device_and_goes_silent_without_it() {
        let mut b = B::new();
        let sys_in = b.node("system-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let speakers = b.node("speakers", "Audio/Sink", true, &["FL", "FR"], &[]);
        let wire = |from, from_pin, to| Wire { from, to, from_pin, to_pin: 0 };
        let p = Graph {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, NodeKind::SwitchOutput { name: "Output".into(), pins: pins(2) }),
                n(3, NodeKind::Output { device: dev("headset") }),
                n(4, NodeKind::Output { device: dev("speakers") }),
            ],
            wires: vec![Wire::new(1, 2), wire(2, 0, 3), wire(2, 1, 4)],
        };
        // Nothing selected yet: pin 1, the headset, which is unplugged -> silence, not speakers.
        let none = Selections::new();
        let plan1 = plan(&p, &b.live, &none);
        assert_eq!(plan1.active_slots[&2], Some(0));
        assert!(plan1.links.is_empty());
        // Select the speakers.
        let sel: Selections = [("Output".to_string(), 1)].into();
        let plan2 = plan(&p, &b.live, &sel);
        let expect: BTreeSet<_> = [
            (b.port(sys_in, Dir::Out, "FL"), b.port(speakers, Dir::In, "FL")),
            (b.port(sys_in, Dir::Out, "FR"), b.port(speakers, Dir::In, "FR")),
        ]
        .into();
        assert_eq!(plan2.links, expect);
        // A selection past the last pin falls back to the last pin.
        let far: Selections = [("Output".to_string(), 7)].into();
        assert_eq!(plan(&p, &b.live, &far).active_slots[&2], Some(1));
    }

    #[test]
    fn switch_input_passes_only_the_selected_mic_through_processing() {
        let mut b = B::new();
        let a50 = b.node("a50-mic", "Audio/Source", true, &[], &["MONO"]);
        let index_mic = b.node("index-mic", "Audio/Source", true, &[], &["MONO"]);
        let eq = b.node("eq-mic", "Audio/Sink", false, &["MONO"], &["MONO"]);
        let sys_out = b.node("system-out", "Audio/Source/Virtual", false, &["MONO"], &["MONO"]);
        let p = Graph {
            nodes: vec![
                n(1, NodeKind::Input { device: dev("a50-mic") }),
                n(2, NodeKind::Input { device: dev("index-mic") }),
                n(5, NodeKind::Eq(EqSpec::new("eq-mic", Channels::Mono))),
                n(3, NodeKind::SwitchInput { name: "Mic".into(), pins: pins(2) }),
                n(4, bus("system-out", BusKind::Microphone, Channels::Mono)),
            ],
            wires: vec![
                Wire::new(1, 5),
                Wire { from: 5, to: 3, from_pin: 0, to_pin: 0 },
                Wire { from: 2, to: 3, from_pin: 0, to_pin: 1 },
                Wire::new(3, 4),
            ],
        };
        let mic_eq = (b.port(a50, Dir::Out, "MONO"), b.port(eq, Dir::In, "MONO"));
        let a50_path = plan(&p, &b.live, &Selections::new());
        assert_eq!(a50_path.links, [mic_eq, (b.port(eq, Dir::Out, "MONO"), b.port(sys_out, Dir::In, "MONO"))].into());
        let sel: Selections = [("Mic".to_string(), 1)].into();
        let index_path = plan(&p, &b.live, &sel);
        assert_eq!(index_path.links, [mic_eq, (b.port(index_mic, Dir::Out, "MONO"), b.port(sys_out, Dir::In, "MONO"))].into());
        assert_eq!(index_path.filters.len(), 1);
    }

    #[test]
    fn app_node_takes_over_only_the_directions_it_wires() {
        let mut b = B::new();
        let sys_in = b.node("system-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let comm = b.node("communication-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let sys_out = b.node("system-out", "Audio/Source/Virtual", false, &["MONO"], &["MONO"]);
        let discord_play = b.app("DiscordCanary", true);
        let discord_rec = b.app("DiscordCanary", false);
        // Recording streams have a monitor port (a copy of the mic). Routing it would echo.
        b.live.ports.insert(
            9000,
            LivePort {
                id: 9000,
                node_id: discord_rec,
                name: "monitor_MONO".into(),
                dir: Dir::Out,
                channel: "MONO".into(),
                index: 1,
                monitor: true,
            },
        );
        let firefox = b.app("firefox", true);
        // What WirePlumber did on its own: everything to the defaults.
        let wp_play = b.link(b.port(discord_play, Dir::Out, "FL"), b.port(sys_in, Dir::In, "FL"));
        let wp_rec = b.link(b.port(sys_out, Dir::Out, "MONO"), b.port(discord_rec, Dir::In, "MONO"));
        let ff = b.link(b.port(firefox, Dir::Out, "FL"), b.port(sys_in, Dir::In, "FL"));
        let discord = Some(AppRef { binary: "DiscordCanary".into(), name: "Discord".into() });
        let p = Graph {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, bus("communication-in", BusKind::Playback, Channels::Stereo)),
                n(3, NodeKind::App { app: discord }),
            ],
            wires: vec![Wire::new(3, 2)],
        };
        let plan = plan(&p, &b.live, &Selections::new());
        assert_eq!(plan.app_streams, [discord_play].into());
        assert!(plan.links.iter().all(|&(o, _)| o != 9000), "mic monitor routed: {:?}", plan.links);
        assert_eq!(plan.links.len(), 2);
        assert!(plan.links.contains(&(b.port(discord_play, Dir::Out, "FR"), b.port(comm, Dir::In, "FR"))));
        // Discord's playback leaves system-in; its recording and Firefox are untouched.
        assert_eq!(plan.remove, vec![wp_play]);
        let _ = (wp_rec, ff);
    }

    #[test]
    fn master_gain_sits_after_every_virtual_output_that_feeds_something() {
        let wire = |from, to| Wire { from, to, from_pin: 0, to_pin: 0 };
        let g = Graph {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, bus("unused", BusKind::Playback, Channels::Stereo)),
                n(3, bus("system-out", BusKind::Microphone, Channels::Mono)),
                n(4, NodeKind::Output { device: dev("hw") }),
                n(5, NodeKind::Input { device: dev("mic") }),
            ],
            wires: vec![wire(1, 4), wire(5, 3)],
        };
        let m = with_master(&g, 0.5);
        let stage = m.nodes.iter().find(|n| matches!(&n.kind, NodeKind::Volume(v) if v.name == "master-system-in" && v.gain == 0.5)).unwrap();
        assert_eq!(m.nodes.len(), 6, "one stage: not for the unused output or the mic");
        assert!(m.wires.contains(&wire(1, stage.id)) && m.wires.contains(&wire(stage.id, 4)));
        assert!(!m.wires.contains(&wire(1, 4)));
        assert!(m.wires.contains(&wire(5, 3)));
    }

    #[test]
    fn other_apps_nodes_pick_the_defaults() {
        let mut b = B::new();
        let sys_in = b.node("system-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let sys_out = b.node("system-out", "Audio/Source/Virtual", false, &["MONO"], &["MONO"]);
        let p = Graph {
            nodes: vec![
                n(1, NodeKind::OtherAppsPlayback),
                n(2, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(3, bus("system-out", BusKind::Microphone, Channels::Mono)),
                n(4, NodeKind::OtherAppsRecording),
            ],
            wires: vec![Wire::new(1, 2), Wire::new(3, 4)],
        };
        let plan = plan(&p, &b.live, &Selections::new());
        assert_eq!((plan.default_sink, plan.default_source), (Some(sys_in), Some(sys_out)));
        assert!(plan.links.is_empty());
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
        let p = Graph {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, NodeKind::Output { device: dev("head") }),
            ],
            wires: vec![Wire::new(1, 2)],
        };
        let plan = plan(&p, &b.live, &Selections::new());
        let mut removed = plan.remove.clone();
        removed.sort();
        assert_eq!(removed, vec![stale, leftover]);
        let _ = (app_link, filter_link, wanted, unrelated);
    }

    #[test]
    fn missing_device_is_reported_and_produces_no_links() {
        let mut b = B::new();
        b.node("system-in", "Audio/Sink", false, &["FL", "FR"], &["FL", "FR"]);
        let p = Graph {
            nodes: vec![
                n(1, bus("system-in", BusKind::Playback, Channels::Stereo)),
                n(2, NodeKind::Output { device: dev("valve-index") }),
            ],
            wires: vec![Wire::new(1, 2)],
        };
        let plan = plan(&p, &b.live, &Selections::new());
        assert!(plan.links.is_empty());
        assert_eq!(plan.missing, vec![2]);
    }
}
