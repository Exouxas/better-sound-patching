//! The PipeWire side: mirrors the registry into a [`Live`] snapshot and, in enforce mode,
//! makes PipeWire match the routing graph.
//!
//! Everything we create uses `object.linger = false`, so it belongs to this process:
//! if the daemon stops or crashes, its links and virtual devices vanish with it.

use crate::ipc::{Request, Response, Status};
use crate::live::{Dir, Live, LiveLink, LiveNode, LivePort};
use crate::filter::FilterNode;
use crate::model::{self, BusKind, BusSpec, Graph, Selections};
use crate::planner::{self, PortPair};
use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub enum Cmd {
    Ipc(Request, mpsc::Sender<Response>),
}

/// Select every switch pin bound to `key`. Returns how many switches changed.
pub fn press_key(graph: &Graph, selected: &mut Selections, key: &str) -> usize {
    let mut changed = 0;
    for n in &graph.nodes {
        let Some((name, pins)) = n.kind.switch() else { continue };
        if let Some(k) = pins.iter().position(|p| !p.key.is_empty() && p.key.eq_ignore_ascii_case(key)) {
            let k = k as u32;
            if crate::planner::selected_pin(&n.kind, selected) != Some(k) {
                changed += 1;
            }
            selected.insert(name.to_owned(), k);
        }
    }
    changed
}

pub struct EngineConfig {
    /// Apply the routing graph. When false, only observe.
    pub enforce: bool,
    /// Called on the PipeWire thread whenever the live graph changed.
    pub on_snapshot: Box<dyn Fn(&Live)>,
    /// Shown in status: programs paused for this session.
    pub paused: Vec<String>,
    /// Told the set of switch shortcut keys whenever it changes.
    pub hotkeys: Option<mpsc::Sender<BTreeSet<String>>>,
}

pub struct ExitInfo {
    pub restore_paused: bool,
}

struct OwnedLink {
    _proxy: pw::link::Link,
    created: Instant,
    seen: bool,
}

struct OwnedBus {
    _proxy: pw::node::Node,
    spec: BusSpec,
}

/// While the default output is being replaced (renamed, or its channels changed), a silent
/// stand-in is the default, so programs never fall back to a real device for a moment —
/// unprocessed and possibly far louder than the graph would make them.
struct Hold {
    _proxy: pw::node::Node,
    /// The live node being replaced; the default stays off it until a new one exists.
    replaced: u32,
    /// When the stand-in was made the default.
    default_at: Option<Instant>,
    /// When the default went to the new device; the stand-in goes a little after.
    done_at: Option<Instant>,
}

const HOLD_NAME: &str = "bsp-switching";

#[derive(Default)]
struct State {
    live: Live,
    dirty: bool,
    profile: Option<Graph>,
    selected: Selections,
    links: HashMap<PortPair, OwnedLink>,
    buses: HashMap<String, OwnedBus>,
    hold: Option<Hold>,
    /// Devices made elsewhere that we're replacing with our own (see `Request::TakeOver`),
    /// and when we asked PipeWire to remove the old one.
    take_over: BTreeMap<String, Option<Instant>>,
    filters: HashMap<String, FilterNode>,
    /// App streams we route ourselves. When we let go, they get linked back to the default.
    taken: BTreeSet<u32>,
    /// Clients we've bound to read their full properties (the executable name isn't in the
    /// registry announcement). Dropped when the client goes away.
    client_info: HashMap<u32, (pw::client::Client, pw::client::ClientListener)>,
    /// Defaults we last asked WirePlumber for (node names), so we only ask on change.
    default_sink: Option<String>,
    default_source: Option<String>,
    /// The defaults from before we changed them (live ids), restored when the daemon stops.
    original_sink: Option<u32>,
    original_source: Option<u32>,
    /// Foreign links we asked PipeWire to destroy, to avoid re-requesting every tick.
    removing: HashMap<u32, Instant>,
    last_error: Option<String>,
    restore_paused: bool,
    fatal: Option<String>,
    hotkeys: Option<mpsc::Sender<BTreeSet<String>>>,
    keys_sent: Option<BTreeSet<String>>,
}

fn parse_u32(props: &pw::spa::utils::dict::DictRef, key: &str) -> Option<u32> {
    props.get(key)?.parse().ok()
}

fn on_global(live: &mut Live, g: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>) -> bool {
    let Some(p) = g.props else { return false };
    match g.type_ {
        ObjectType::Node => {
            let name = p.get("node.name").unwrap_or_default().to_owned();
            let description = p
                .get("node.description")
                .or_else(|| p.get("node.nick"))
                .unwrap_or_default()
                .to_owned();
            // Level meters (pavucontrol, the volume applet) are streams too; never route them.
            let meter = p.get("stream.monitor") == Some("true") || p.get("stream.capture.sink") == Some("true");
            // The executable name lives on the client (see `bind_client`), which normally shows
            // up before its streams.
            let client_id = parse_u32(p, "client.id");
            let app_name = p.get("application.name").unwrap_or_default().to_owned();
            let app_binary = p
                .get("application.process.binary")
                .map(str::to_owned)
                .or_else(|| live.clients.get(&client_id?).cloned())
                .unwrap_or_default();
            live.nodes.insert(
                g.id,
                LiveNode {
                    id: g.id,
                    name,
                    description,
                    media_class: p.get("media.class").unwrap_or_default().to_owned(),
                    device_id: parse_u32(p, "device.id"),
                    app_binary,
                    app_name,
                    client_id,
                    meter,
                },
            );
            true
        }
        ObjectType::Port => {
            let Some(node_id) = parse_u32(p, "node.id") else { return false };
            let dir = match p.get("port.direction") {
                Some("in") => Dir::In,
                Some("out") => Dir::Out,
                _ => return false,
            };
            live.ports.insert(
                g.id,
                LivePort {
                    id: g.id,
                    node_id,
                    name: p.get("port.name").unwrap_or_default().to_owned(),
                    dir,
                    channel: p.get("audio.channel").unwrap_or_default().to_owned(),
                    index: parse_u32(p, "port.id").unwrap_or(0),
                    monitor: p.get("port.monitor") == Some("true"),
                },
            );
            true
        }
        ObjectType::Link => {
            let (Some(out_port), Some(in_port), Some(out_node), Some(in_node)) = (
                parse_u32(p, "link.output.port"),
                parse_u32(p, "link.input.port"),
                parse_u32(p, "link.output.node"),
                parse_u32(p, "link.input.node"),
            ) else {
                return false;
            };
            live.links.insert(g.id, LiveLink { id: g.id, out_port, in_port, out_node, in_node });
            true
        }
        _ => false,
    }
}

/// Bind a client to learn which program it is (`application.process.binary`), and label its
/// streams with it.
fn bind_client(
    st: &mut State,
    registry: &pw::registry::RegistryRc,
    state: &std::rc::Weak<RefCell<State>>,
    g: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>,
) {
    let Ok(client) = registry.bind::<pw::client::Client, _>(g) else { return };
    let id = g.id;
    let state = state.clone();
    let listener = client
        .add_listener_local()
        .info(move |info| {
            let Some(binary) = info.props().and_then(|p| p.get("application.process.binary")) else { return };
            let Some(state) = state.upgrade() else { return };
            let Ok(mut st) = state.try_borrow_mut() else { return };
            st.live.clients.insert(id, binary.to_owned());
            let mut changed = false;
            for n in st.live.nodes.values_mut().filter(|n| n.client_id == Some(id) && n.app_binary != binary) {
                n.app_binary = binary.to_owned();
                changed = true;
            }
            st.dirty |= changed;
        })
        .register();
    st.client_info.insert(id, (client, listener));
}

fn on_global_remove(live: &mut Live, id: u32) -> bool {
    live.nodes.remove(&id).is_some()
        || live.ports.remove(&id).is_some()
        || live.links.remove(&id).is_some()
        || live.clients.remove(&id).is_some()
}

fn create_bus(core: &pw::core::CoreRc, spec: &BusSpec) -> Result<pw::node::Node> {
    let position = match spec.channels {
        model::Channels::Mono => "[ MONO ]",
        model::Channels::Stereo => "[ FL FR ]",
    };
    let mut props = pw::properties::properties! {
        "factory.name" => "support.null-audio-sink",
        "node.name" => spec.name.as_str(),
        "node.description" => spec.display(),
        "media.class" => spec.media_class(),
        "audio.position" => position,
        // Not `node.virtual`: that makes PipeWire report it as a non-hardware device, and
        // desktop sound settings (KDE's included) hide those unless asked to show virtual ones.
        "object.linger" => "false",
    };
    if spec.kind == BusKind::Playback {
        props.insert("monitor.channel-volumes", "true");
        props.insert("monitor.passthrough", "true");
    }
    Ok(core.create_object::<pw::node::Node>("adapter", &props)?)
}

fn create_link(core: &pw::core::CoreRc, live: &Live, (out_port, in_port): PortPair, linger: bool) -> Result<pw::link::Link> {
    let out_node = live.ports.get(&out_port).ok_or_else(|| anyhow!("port {out_port} vanished"))?.node_id;
    let in_node = live.ports.get(&in_port).ok_or_else(|| anyhow!("port {in_port} vanished"))?.node_id;
    let props = pw::properties::properties! {
        "link.output.port" => out_port.to_string(),
        "link.input.port" => in_port.to_string(),
        "link.output.node" => out_node.to_string(),
        "link.input.node" => in_node.to_string(),
        "object.linger" => if linger { "true" } else { "false" },
    };
    Ok(core.create_object::<pw::link::Link>("link-factory", &props)?)
}

/// The node WirePlumber currently uses as default output (`sink`) or input.
fn current_default(sink: bool) -> Option<u32> {
    let target = if sink { "@DEFAULT_AUDIO_SINK@" } else { "@DEFAULT_AUDIO_SOURCE@" };
    let out = std::process::Command::new("wpctl").args(["inspect", target]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).strip_prefix("id ")?.split(',').next()?.trim().parse().ok()
}

/// Testing aid: `BSP_TEST_KEEP_HARDWARE=1` makes a daemon that only ever touches its own
/// throwaway virtual devices: links to real devices are left alone, streams are never handed to
/// the default devices, and the defaults are never changed.
fn test_mode() -> bool {
    std::env::var_os("BSP_TEST_KEEP_HARDWARE").is_some()
}

/// Give app streams we stop routing back to the default device, the way WirePlumber would have
/// linked them. These links outlive us (linger), so stopping the daemon never leaves an app mute.
fn hand_back(st: &mut State, core: &pw::core::CoreRc, streams: &[u32]) {
    // A test daemon must never send anything to the real default devices.
    if test_mode() {
        return;
    }
    for &id in streams {
        let Some(node) = st.live.nodes.get(&id) else { continue };
        let playback = node.is_playback_stream();
        let Some(target) = current_default(playback) else { continue };
        let pairs = if playback {
            planner::map_channels(&st.live.ports_of(id, Dir::Out), &st.live.ports_of(target, Dir::In))
        } else {
            planner::map_channels(&st.live.ports_of(target, Dir::Out), &st.live.ports_of(id, Dir::In))
        };
        log::info!("handing {} back to the default {}", node.label(), if playback { "output" } else { "input" });
        for pair in pairs {
            // The proxy is dropped right away; the link stays because it lingers.
            if let Err(e) = create_link(core, &st.live, pair, true) {
                log::warn!("handing back {}: {e}", node.label());
            }
        }
    }
}

/// Ask WirePlumber to make `id` the default output or input, if it isn't what we last asked for.
/// The first time, remember what the default was so it can be put back when we stop.
fn set_default(live: &Live, last: &mut Option<String>, original: &mut Option<u32>, id: Option<u32>, sink: bool) {
    let Some(node) = id.and_then(|id| live.nodes.get(&id)) else { return };
    if test_mode() {
        return;
    }
    if last.as_deref() == Some(node.name.as_str()) {
        return;
    }
    if last.is_none() && original.is_none() {
        *original = current_default(sink);
    }
    log::info!("default {} -> {}", if sink { "output" } else { "input" }, node.name);
    *last = Some(node.name.clone());
    let id = node.id.to_string();
    std::thread::spawn(move || {
        let _ = std::process::Command::new("wpctl").args(["set-default", &id]).status();
    });
}

/// A device we own that is the default output is about to be replaced (its settings changed).
/// First make a silent stand-in the default and give WirePlumber a moment to move programs
/// onto it. Returns true while the old device must be kept a little longer.
fn replace_default_safely(st: &mut State, core: &pw::core::CoreRc, wanted: &[BusSpec]) -> bool {
    if test_mode() || st.hold.is_some_and_ready() {
        return false;
    }
    let Some(hold) = &mut st.hold else {
        let is_default = |name: &str| st.default_sink.as_deref() == Some(name);
        let changed = st
            .buses
            .iter()
            .find(|(name, owned)| {
                owned.spec.kind == BusKind::Playback && is_default(name) && wanted.iter().any(|b| &b.name == *name && *b != owned.spec)
            })
            .map(|(name, _)| name)
            .or_else(|| st.take_over.keys().find(|name| is_default(name)));
        let Some(name) = changed else { return false };
        let Some(old) = st.live.node_by_name(name).map(|n| n.id) else { return false };
        let spec = BusSpec { description: "Switching…".into(), ..BusSpec::new(HOLD_NAME, BusKind::Playback, model::Channels::Stereo) };
        return match create_bus(core, &spec) {
            Ok(proxy) => {
                log::info!("replacing default output {name}: holding programs on a silent stand-in");
                st.hold = Some(Hold { _proxy: proxy, replaced: old, default_at: None, done_at: None });
                st.dirty = true;
                true
            }
            Err(e) => {
                log::warn!("no stand-in for replacing {name}: {e}");
                false
            }
        };
    };
    if hold.default_at.is_none() {
        let Some(id) = st.live.node_by_name(HOLD_NAME).map(|n| n.id) else { return true };
        // Wait for this to take effect before the old device goes.
        let _ = std::process::Command::new("wpctl").args(["set-default", &id.to_string()]).status();
        hold.default_at = Some(Instant::now());
        st.default_sink = Some(HOLD_NAME.into());
        return true;
    }
    hold.default_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300))
}

trait HoldReady {
    fn is_some_and_ready(&self) -> bool;
}
impl HoldReady for Option<Hold> {
    /// The stand-in already took over and the old device was let go.
    fn is_some_and_ready(&self) -> bool {
        self.as_ref().is_some_and(|h| h.default_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(300)))
    }
}

/// Bring PipeWire in line with the routing graph.
fn reconcile(st: &mut State, core: &pw::core::CoreRc, registry: &pw::registry::RegistryRc) {
    let Some(profile) = st.profile.clone() else {
        st.links.clear();
        st.buses.clear();
        st.hold = None;
        st.take_over.clear();
        st.filters.clear();
        let taken: Vec<u32> = std::mem::take(&mut st.taken).into_iter().collect();
        hand_back(st, core, &taken);
        return;
    };
    let keys: BTreeSet<String> =
        profile.nodes.iter().filter_map(|n| n.kind.switch()).flat_map(|s| s.1).filter(|p| !p.key.is_empty()).map(|p| p.key.clone()).collect();
    if st.keys_sent.as_ref() != Some(&keys)
        && let Some(tx) = &st.hotkeys
    {
        let _ = tx.send(keys.clone());
        st.keys_sent = Some(keys);
    }
    let mut plan = planner::plan(&profile, &st.live, &st.selected);
    if test_mode() {
        let hw = |id: u32| st.live.nodes.get(&id).is_some_and(|n| n.is_hardware());
        plan.remove.retain(|id| st.live.links.get(id).is_none_or(|l| !hw(l.out_node) && !hw(l.in_node)));
    }

    // Drop our links the profile no longer wants first (before any bus they touch goes away).
    // Dropping an OwnedLink destroys the link in PipeWire.
    let mut dropped = std::collections::HashSet::new();
    st.links.retain(|pair, _| {
        let keep = plan.links.contains(pair);
        if !keep {
            dropped.insert(*pair);
        }
        keep
    });
    // Their globals linger in `live` until PipeWire confirms; don't try to destroy them again.
    for l in st.live.links.values() {
        if dropped.contains(&(l.out_port, l.in_port)) {
            st.removing.insert(l.id, Instant::now());
        }
    }

    // Virtual devices: keep ours, adopt same-named ones that already exist (e.g. from your
    // existing pipewire.conf.d), create the rest, and drop ours that the profile no longer uses.
    let wait = replace_default_safely(st, core, &plan.buses);
    st.buses.retain(|name, owned| match plan.buses.iter().find(|b| &b.name == name) {
        None => false,
        Some(b) => *b == owned.spec || wait,
    });
    // Devices we're taking over: remove the old one, then (below) make ours once it's gone.
    if !wait {
        let names: Vec<String> = st.take_over.keys().cloned().collect();
        for name in names {
            let old = st.live.node_by_name(&name).map(|n| n.id);
            let asked = st.take_over[&name];
            match (old, asked) {
                _ if st.buses.contains_key(&name) || !plan.buses.iter().any(|b| b.name == name) => {
                    st.take_over.remove(&name);
                }
                (None, _) => {
                    log::info!("took over {name}");
                    st.take_over.remove(&name);
                }
                (Some(id), None) => {
                    log::info!("taking over {name}: removing the one made elsewhere");
                    registry.destroy_global(id);
                    st.take_over.insert(name, Some(Instant::now()));
                }
                (Some(_), Some(t)) if t.elapsed() > Duration::from_secs(3) => {
                    st.last_error = Some(format!("couldn't remove the old {name}; restart PipeWire to finish taking it over"));
                    st.take_over.remove(&name);
                }
                _ => {}
            }
        }
    }
    for spec in &plan.buses {
        if st.buses.contains_key(&spec.name) || st.live.node_by_name(&spec.name).is_some() {
            continue;
        }
        match create_bus(core, spec) {
            Ok(proxy) => {
                log::info!("created virtual device {}", spec.name);
                st.buses.insert(spec.name.clone(), OwnedBus { _proxy: proxy, spec: spec.clone() });
            }
            Err(e) => st.last_error = Some(format!("creating {}: {e}", spec.name)),
        }
    }

    // Processing nodes: retune in place when possible (no gap in the audio), rebuild otherwise.
    st.filters.retain(|name, node| plan.filters.iter().any(|f| f.name() == name && node.compatible(f)));
    for spec in &plan.filters {
        if let Some(node) = st.filters.get_mut(spec.name()) {
            node.update(spec);
            continue;
        }
        match FilterNode::new(core, spec) {
            Ok(node) => {
                log::info!("started {}", spec.name());
                st.filters.insert(spec.name().to_owned(), node);
            }
            Err(e) => st.last_error = Some(format!("starting {}: {e:#}", spec.name())),
        }
    }

    let existing: std::collections::HashSet<PortPair> =
        st.live.links.values().map(|l| (l.out_port, l.in_port)).collect();
    for &pair in &plan.links {
        if let Some(owned) = st.links.get_mut(&pair) {
            if existing.contains(&pair) {
                owned.seen = true;
                continue;
            }
            // Ours, but gone (someone removed it) or never appeared: try again.
            if !(owned.seen || owned.created.elapsed() > Duration::from_secs(2)) {
                continue;
            }
            st.links.remove(&pair);
        } else if existing.contains(&pair) {
            continue; // already satisfied by a link someone else made
        }
        match create_link(core, &st.live, pair, false) {
            Ok(proxy) => {
                st.links.insert(pair, OwnedLink { _proxy: proxy, created: Instant::now(), seen: false });
            }
            Err(e) => st.last_error = Some(format!("linking {pair:?}: {e}")),
        }
    }

    // Apps the profile no longer routes go back to the default device.
    let released: Vec<u32> = st.taken.difference(&plan.app_streams).copied().collect();
    st.taken = plan.app_streams.clone();
    hand_back(st, core, &released);
    // While the stand-in holds the default, only move the default to the replacement device.
    let hold_blocks = st.hold.as_ref().is_some_and(|h| h.done_at.is_none() && plan.default_sink.is_none_or(|id| id == h.replaced));
    if !hold_blocks {
        set_default(&st.live, &mut st.default_sink, &mut st.original_sink, plan.default_sink, true);
        if let Some(h) = &mut st.hold {
            h.done_at.get_or_insert_with(Instant::now);
        }
    }
    if st.hold.as_ref().is_some_and(|h| h.done_at.is_some_and(|t| t.elapsed() > Duration::from_millis(1500))) {
        log::info!("default output replaced; removing the stand-in");
        st.hold = None;
    }
    // Keep ticking until the replacement is done.
    st.dirty |= st.hold.is_some() || !st.take_over.is_empty();
    set_default(&st.live, &mut st.default_source, &mut st.original_source, plan.default_source, false);

    // Conflicting links made by others (qpwgraph, old sessions, ...).
    st.removing.retain(|id, t| st.live.links.contains_key(id) && t.elapsed() < Duration::from_secs(2));
    for id in plan.remove {
        let Some(l) = st.live.links.get(&id) else { continue };
        let pair = (l.out_port, l.in_port);
        if st.links.contains_key(&pair) || dropped.contains(&pair) || st.removing.contains_key(&id) {
            continue;
        }
        log::info!("removing conflicting link {id} ({} -> {})", l.out_node, l.in_node);
        registry.destroy_global(id);
        st.removing.insert(id, Instant::now());
    }
}

fn status(st: &State, cfg_paused: &[String]) -> Status {
    let mut buses_owned: Vec<String> = st.buses.keys().cloned().collect();
    buses_owned.sort();
    Status {
        graph_loaded: st.profile.is_some(),
        selected: st.selected.clone(),
        pid: std::process::id(),
        paused: cfg_paused.to_vec(),
        links_owned: st.links.len(),
        buses_owned,
        filters_running: {
            let mut v: Vec<String> = st.filters.keys().cloned().collect();
            v.sort();
            v
        },
        last_error: st.last_error.clone(),
    }
}

fn load_graph(st: &mut State) -> Result<()> {
    st.profile = Some(model::load_graph()?);
    st.dirty = true;
    Ok(())
}

/// Remember selections across restarts.
fn save_selected(st: &State) {
    let mut state = model::load_state();
    state.selected = st.selected.clone();
    if let Err(e) = model::save_state(&state) {
        log::warn!("saving selections: {e:#}");
    }
}

/// Run the PipeWire loop on the current thread until quit.
pub fn run(cfg: EngineConfig, cmd_rx: pw::channel::Receiver<Cmd>) -> Result<ExitInfo> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let state = Rc::new(RefCell::new(State { dirty: true, restore_paused: true, hotkeys: cfg.hotkeys.clone(), ..Default::default() }));
    if cfg.enforce {
        let mut st = state.borrow_mut();
        st.selected = model::load_state().selected;
        if !model::graph_exists() {
            log::warn!("no routing graph yet; nothing will be routed until you save one");
        } else if let Err(e) = load_graph(&mut st) {
            log::error!("{e:#}");
            st.last_error = Some(format!("{e:#}"));
        }
    }

    let _core_listener = {
        let state = state.clone();
        let ml = mainloop.downgrade();
        core.add_listener_local()
            .error(move |id, _seq, res, msg| {
                // -2 / -116 ("no global", "unknown resource") mean an object vanished before
                // PipeWire got to our request, e.g. a short sound that already ended. Harmless.
                if res == -2 || res == -116 {
                    log::debug!("pipewire: {msg} (object {id} already gone)");
                    return;
                }
                log::error!("pipewire error on {id}: {msg} ({res})");
                // Only a broken connection is fatal; other core errors (e.g. about a proxy we
                // just dropped) are noise.
                if id == pw::core::PW_ID_CORE && res == -32 {
                    state.borrow_mut().fatal = Some(format!("lost connection to PipeWire: {msg}"));
                    if let Some(ml) = ml.upgrade() {
                        ml.quit();
                    }
                } else {
                    state.borrow_mut().last_error = Some(msg.to_owned());
                }
            })
            .register()
    };

    let _reg_listener = {
        let s1 = state.clone();
        let s2 = state.clone();
        let s3 = Rc::downgrade(&state);
        let reg_for_bind = registry.clone();
        registry
            .add_listener_local()
            .global(move |g| {
                let mut st = s1.borrow_mut();
                if on_global(&mut st.live, g) {
                    st.dirty = true;
                }
                if g.type_ == ObjectType::Client {
                    bind_client(&mut st, &reg_for_bind, &s3, g);
                }
            })
            .global_remove(move |id| {
                let mut st = s2.borrow_mut();
                st.client_info.remove(&id);
                if on_global_remove(&mut st.live, id) {
                    st.dirty = true;
                }
            })
            .register()
    };

    let enforce = cfg.enforce;
    let paused = cfg.paused.clone();
    let _cmd = {
        let state = state.clone();
        let core_for_cmd = core.clone();
        let registry_for_cmd = registry.clone();
        let ml = mainloop.downgrade();
        cmd_rx.attach(mainloop.loop_(), move |cmd| {
            let mut st = state.borrow_mut();
            let reply = match cmd {
                Cmd::Ipc(req, tx) => {
                    let resp = match req {
                        Request::Status => Response::Status(status(&st, &paused)),
                        Request::Reload if enforce => match load_graph(&mut st) {
                            Ok(()) => Response::Ok,
                            Err(e) => Response::Error(format!("{e:#}")),
                        },
                        Request::Reload => Response::Ok,
                        Request::Select { switch, pin } if enforce => {
                            let exists = st.profile.as_ref().is_some_and(|g| {
                                g.nodes.iter().any(|n| n.kind.switch().is_some_and(|(name, pins)| name == switch && (pin as usize) < pins.len()))
                            });
                            if exists {
                                log::info!("{switch} -> pin {}", pin + 1);
                                st.selected.insert(switch, pin);
                                save_selected(&st);
                                reconcile(&mut st, &core_for_cmd, &registry_for_cmd);
                                Response::Ok
                            } else {
                                Response::Error(format!("no switch {switch:?} with a pin {}", pin + 1))
                            }
                        }
                        Request::Key { key } if enforce => {
                            let graph = st.profile.clone().unwrap_or_default();
                            let mut selected = st.selected.clone();
                            if graph.nodes.iter().filter_map(|n| n.kind.switch()).flat_map(|s| s.1).any(|p| p.key.eq_ignore_ascii_case(&key)) {
                                let changed = press_key(&graph, &mut selected, &key);
                                log::info!("key {key}: {changed} switch(es) changed");
                                st.selected = selected;
                                save_selected(&st);
                                reconcile(&mut st, &core_for_cmd, &registry_for_cmd);
                                Response::Ok
                            } else {
                                Response::Error(format!("no switch pin uses the key {key:?}"))
                            }
                        }
                        Request::Select { .. } | Request::Key { .. } => Response::Error("not enforcing".into()),
                        Request::Levels => Response::Levels(st.filters.iter().map(|(k, f)| (k.clone(), f.reading())).collect()),
                        Request::Preview { profile } if enforce => {
                            st.profile = Some(profile);
                            // Apply right away rather than on the next tick: this is what makes
                            // dragging an EQ band or a wire feel immediate.
                            reconcile(&mut st, &core_for_cmd, &registry_for_cmd);
                            Response::Ok
                        }
                        Request::Preview { .. } => Response::Error("not enforcing".into()),
                        Request::TakeOver { names } if enforce => {
                            for n in names {
                                st.take_over.insert(n, None);
                            }
                            reconcile(&mut st, &core_for_cmd, &registry_for_cmd);
                            Response::Ok
                        }
                        Request::TakeOver { .. } => Response::Error("not enforcing".into()),
                        Request::Stop { restore_paused } => {
                            st.restore_paused = restore_paused;
                            if let Some(ml) = ml.upgrade() {
                                ml.quit();
                            }
                            Response::Ok
                        }
                    };
                    (resp, tx)
                }
            };
            let _ = reply.1.send(reply.0);
        })
    };

    let quit_on_signal = |ml: pw::main_loop::MainLoopWeak| {
        move || {
            if let Some(ml) = ml.upgrade() {
                ml.quit();
            }
        }
    };
    // Signal handlers are only allowed on the main thread (the daemon); the GUI's observer
    // runs on a worker thread and simply dies with the process.
    let on_main = std::thread::current().name() == Some("main");
    let _sigint = on_main.then(|| {
        mainloop
            .loop_()
            .add_signal_local(pw::loop_::Signal::INT, quit_on_signal(mainloop.downgrade()))
    });
    let _sigterm = on_main.then(|| {
        mainloop
            .loop_()
            .add_signal_local(pw::loop_::Signal::TERM, quit_on_signal(mainloop.downgrade()))
    });

    // A short periodic tick batches bursts of registry events (a device appearing produces
    // dozens) into one reconcile, and retries anything that did not stick.
    let ticks = Rc::new(std::cell::Cell::new(0u64));
    let timer = {
        let state = state.clone();
        let core = core.clone();
        let registry = registry.clone();
        mainloop.loop_().add_timer(move |_| {
            ticks.set(ticks.get() + 1);
            let mut st = state.borrow_mut();
            let periodic = ticks.get() % 10 == 0;
            if !(st.dirty || (enforce && periodic)) {
                return;
            }
            let was_dirty = std::mem::take(&mut st.dirty);
            if enforce {
                reconcile(&mut st, &core, &registry);
            }
            if was_dirty {
                (cfg.on_snapshot)(&st.live);
            }
        })
    };
    timer
        .update_timer(Some(Duration::from_millis(50)), Some(Duration::from_millis(100)))
        .into_result()
        .map_err(|e| anyhow!("timer: {e}"))?;

    mainloop.run();

    let mut st = state.borrow_mut();
    let info = ExitInfo { restore_paused: st.restore_paused };
    if let Some(f) = st.fatal.take() {
        return Err(anyhow!(f));
    }
    // Apps we routed get linked back to the default first, so none is left mute. Wait until
    // PipeWire has made those links before letting go of ours.
    let taken: Vec<u32> = std::mem::take(&mut st.taken).into_iter().collect();
    hand_back(&mut st, &core, &taken);
    st.profile = None;
    // Put the system defaults back the way we found them.
    for id in [st.original_sink, st.original_source].into_iter().flatten() {
        if st.live.nodes.contains_key(&id) {
            let _ = std::process::Command::new("wpctl").args(["set-default", &id.to_string()]).status();
        }
    }
    drop(st);
    roundtrip(&mainloop, &core);
    // Explicitly drop our objects while still connected so PipeWire removes them right away.
    let mut st = state.borrow_mut();
    st.links.clear();
    st.filters.clear();
    st.buses.clear();
    drop(st);
    roundtrip(&mainloop, &core);
    Ok(info)
}

/// Run the loop until PipeWire has processed everything we sent so far (at most 2 seconds).
fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) {
    let Ok(pending) = core.sync(0) else { return };
    let ml = mainloop.downgrade();
    let _done = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending
                && let Some(ml) = ml.upgrade()
            {
                ml.quit();
            }
        })
        .register();
    let ml = mainloop.downgrade();
    let timeout = mainloop.loop_().add_timer(move |_| {
        if let Some(ml) = ml.upgrade() {
            ml.quit();
        }
    });
    let _ = timeout.update_timer(Some(Duration::from_secs(2)), None);
    mainloop.run();
}
