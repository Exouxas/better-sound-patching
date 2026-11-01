//! The PipeWire side: mirrors the registry into a [`Live`] snapshot and, in enforce mode,
//! makes the graph match the active profile.
//!
//! Everything we create uses `object.linger = false`, so it belongs to this process:
//! if the daemon stops or crashes, its links and virtual devices vanish with it.

use crate::ipc::{Request, Response, Status};
use crate::live::{Dir, Live, LiveLink, LiveNode, LivePort};
use crate::model::{self, BusKind, BusSpec, Profile};
use crate::planner::{self, PortPair};
use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub enum Cmd {
    Ipc(Request, mpsc::Sender<Response>),
}

pub struct EngineConfig {
    /// Apply the active profile. When false, only observe.
    pub enforce: bool,
    /// Called on the PipeWire thread whenever the live graph changed.
    pub on_snapshot: Box<dyn Fn(&Live)>,
    /// Shown in status: programs paused for this session.
    pub paused: Vec<String>,
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

#[derive(Default)]
struct State {
    live: Live,
    dirty: bool,
    profile_name: Option<String>,
    profile: Option<Profile>,
    links: HashMap<PortPair, OwnedLink>,
    buses: HashMap<String, OwnedBus>,
    /// Foreign links we asked PipeWire to destroy, to avoid re-requesting every tick.
    removing: HashMap<u32, Instant>,
    last_error: Option<String>,
    restore_paused: bool,
    fatal: Option<String>,
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
            live.nodes.insert(
                g.id,
                LiveNode {
                    id: g.id,
                    name,
                    description,
                    media_class: p.get("media.class").unwrap_or_default().to_owned(),
                    device_id: parse_u32(p, "device.id"),
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

fn on_global_remove(live: &mut Live, id: u32) -> bool {
    live.nodes.remove(&id).is_some() || live.ports.remove(&id).is_some() || live.links.remove(&id).is_some()
}

fn create_bus(core: &pw::core::CoreRc, spec: &BusSpec) -> Result<pw::node::Node> {
    let position = match spec.channels {
        model::Channels::Mono => "[ MONO ]",
        model::Channels::Stereo => "[ FL FR ]",
    };
    let mut props = pw::properties::properties! {
        "factory.name" => "support.null-audio-sink",
        "node.name" => spec.name.as_str(),
        "node.description" => spec.name.as_str(),
        "media.class" => spec.media_class(),
        "audio.position" => position,
        "node.virtual" => "true",
        "object.linger" => "false",
    };
    if spec.kind == BusKind::Playback {
        props.insert("monitor.channel-volumes", "true");
        props.insert("monitor.passthrough", "true");
    }
    Ok(core.create_object::<pw::node::Node>("adapter", &props)?)
}

fn create_link(core: &pw::core::CoreRc, live: &Live, (out_port, in_port): PortPair) -> Result<pw::link::Link> {
    let out_node = live.ports.get(&out_port).ok_or_else(|| anyhow!("port {out_port} vanished"))?.node_id;
    let in_node = live.ports.get(&in_port).ok_or_else(|| anyhow!("port {in_port} vanished"))?.node_id;
    let props = pw::properties::properties! {
        "link.output.port" => out_port.to_string(),
        "link.input.port" => in_port.to_string(),
        "link.output.node" => out_node.to_string(),
        "link.input.node" => in_node.to_string(),
        "object.linger" => "false",
    };
    Ok(core.create_object::<pw::link::Link>("link-factory", &props)?)
}

/// Bring the live graph in line with the active profile.
fn reconcile(st: &mut State, core: &pw::core::CoreRc, registry: &pw::registry::RegistryRc) {
    let Some(profile) = st.profile.clone() else {
        st.links.clear();
        st.buses.clear();
        return;
    };
    let plan = planner::plan(&profile, &st.live);

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
    st.buses.retain(|name, owned| plan.buses.iter().any(|b| &b.name == name && *b == owned.spec));
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
        match create_link(core, &st.live, pair) {
            Ok(proxy) => {
                st.links.insert(pair, OwnedLink { _proxy: proxy, created: Instant::now(), seen: false });
            }
            Err(e) => st.last_error = Some(format!("linking {pair:?}: {e}")),
        }
    }

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
        active_profile: st.profile_name.clone(),
        pid: std::process::id(),
        paused: cfg_paused.to_vec(),
        links_owned: st.links.len(),
        buses_owned,
        last_error: st.last_error.clone(),
    }
}

fn load_active(st: &mut State, name: &str) -> Result<()> {
    let profile = model::load_profile(name)?;
    st.profile = Some(profile);
    st.profile_name = Some(name.to_owned());
    st.dirty = true;
    Ok(())
}

/// Run the PipeWire loop on the current thread until quit.
pub fn run(cfg: EngineConfig, cmd_rx: pw::channel::Receiver<Cmd>) -> Result<ExitInfo> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let state = Rc::new(RefCell::new(State { dirty: true, restore_paused: true, ..Default::default() }));
    if cfg.enforce {
        let mut st = state.borrow_mut();
        if let Some(name) = model::load_state().active_profile {
            if let Err(e) = load_active(&mut st, &name) {
                log::error!("{e:#}");
                st.last_error = Some(format!("{e:#}"));
            }
        } else {
            log::warn!("no active profile; nothing will be routed until you switch to one");
        }
    }

    let _core_listener = {
        let state = state.clone();
        let ml = mainloop.downgrade();
        core.add_listener_local()
            .error(move |id, _seq, res, msg| {
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
        registry
            .add_listener_local()
            .global(move |g| {
                let mut st = s1.borrow_mut();
                if on_global(&mut st.live, g) {
                    st.dirty = true;
                }
            })
            .global_remove(move |id| {
                let mut st = s2.borrow_mut();
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
        let ml = mainloop.downgrade();
        cmd_rx.attach(mainloop.loop_(), move |cmd| {
            let mut st = state.borrow_mut();
            let reply = match cmd {
                Cmd::Ipc(req, tx) => {
                    let resp = match req {
                        Request::Status => Response::Status(status(&st, &paused)),
                        Request::Reload => match st.profile_name.clone() {
                            Some(name) => match load_active(&mut st, &name) {
                                Ok(()) => Response::Ok,
                                Err(e) => Response::Error(format!("{e:#}")),
                            },
                            None => Response::Ok,
                        },
                        Request::Switch { profile } if enforce => match load_active(&mut st, &profile) {
                            Ok(()) => {
                                log::info!("switched to profile {profile}");
                                let _ = model::save_state(&model::AppState { active_profile: Some(profile) });
                                Response::Ok
                            }
                            Err(e) => Response::Error(format!("{e:#}")),
                        },
                        Request::Switch { .. } => Response::Error("not enforcing".into()),
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
    // Explicitly drop our objects while still connected so PipeWire removes them right away.
    st.links.clear();
    st.buses.clear();
    drop(st);
    let _ = core.sync(0);
    Ok(info)
}
