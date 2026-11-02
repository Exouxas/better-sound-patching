mod dsp;
mod engine;
mod filter;
mod gui;
mod hotkeys;
mod ipc;
mod live;
mod model;
mod planner;
mod session;
mod takeover;
mod tray;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bsp", about = "Node-based PipeWire routing that just works")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the editor and mixer (default).
    Gui,
    /// Run the routing daemon in the foreground.
    Daemon {
        /// Pause a conflicting program (qpwgraph, easyeffects) while running; relaunched on exit.
        #[arg(long)]
        pause: Vec<String>,
    },
    /// Start a trial session in the background (nothing persistent changes).
    Trial {
        /// Programs to pause. Default: qpwgraph, plus easyeffects if your graph doesn't use it.
        #[arg(long)]
        pause: Vec<String>,
    },
    /// Stop the daemon and give control back to your old setup.
    Stop,
    /// Make a switch node use one of its pins, e.g. `bsp select Output 2`.
    Select {
        switch: String,
        /// Pin number as shown in the editor (1 = first).
        pin: u32,
    },
    /// Act as if a switch shortcut was pressed: selects every pin bound to it, e.g. `bsp key Ctrl+Alt+1`.
    Key { key: String },
    /// Show daemon status.
    Status,
    /// Dry run: show what the graph would change right now, without touching anything.
    Plan,
    /// Make the new system start at login. Moves the given programs' autostart entries aside.
    Approve {
        /// Default: qpwgraph, plus easyeffects if your graph doesn't use it.
        #[arg(long)]
        disable: Vec<String>,
    },
    /// Undo `approve` and bring the old setup back.
    Revert,
    /// Relaunch programs a trial paused (e.g. after a crash).
    Restore,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,bsp=info")).init();
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Gui) {
        Cmd::Gui => gui::run(),
        Cmd::Daemon { pause } => session::run_daemon(&pause),
        Cmd::Trial { pause } => {
            let pause = if pause.is_empty() { session::default_conflicts() } else { pause };
            session::start_trial(&pause)?;
            println!("trial running (paused: {}). `bsp stop` (or a reboot) brings your old setup back", pause.join(", "));
            Ok(())
        }
        Cmd::Stop => session::stop_daemon(true),
        Cmd::Select { switch, pin } => {
            if pin == 0 {
                bail!("pins are numbered from 1");
            }
            let graph = model::load_graph()?;
            let Some((name, pins)) = graph.nodes.iter().filter_map(|n| n.kind.switch()).find(|(n, _)| n.eq_ignore_ascii_case(&switch)) else {
                let names: Vec<_> = graph.nodes.iter().filter_map(|n| n.kind.switch()).map(|s| s.0).collect();
                bail!("no switch named {switch:?}; have: {}", names.join(", "));
            };
            if pin as usize > pins.len() {
                bail!("{name} has only {} pins", pins.len());
            }
            let req = ipc::Request::Select { switch: name.to_owned(), pin: pin - 1 };
            if ipc::status().is_some() {
                if let ipc::Response::Error(e) = ipc::request(&req)? {
                    bail!(e);
                }
            } else {
                let mut state = model::load_state();
                state.selected.insert(name.to_owned(), pin - 1);
                model::save_state(&state)?;
                println!("daemon not running; {name} will use pin {pin} next time it starts");
            }
            Ok(())
        }
        Cmd::Key { key } => {
            if ipc::status().is_some() {
                if let ipc::Response::Error(e) = ipc::request(&ipc::Request::Key { key })? {
                    bail!(e);
                }
            } else {
                let graph = model::load_graph()?;
                let mut state = model::load_state();
                let n = engine::press_key(&graph, &mut state.selected, &key);
                model::save_state(&state)?;
                println!("daemon not running; {n} switch(es) changed for next time");
            }
            Ok(())
        }
        Cmd::Status => {
            match ipc::status() {
                Some(s) => println!("{}", serde_json::to_string_pretty(&s)?),
                None => println!("daemon not running (your normal setup is in charge)"),
            }
            println!("approved: {}", session::is_approved());
            Ok(())
        }
        Cmd::Plan => print_plan(),
        Cmd::Approve { disable } => {
            let disable = if disable.is_empty() { session::default_conflicts() } else { disable };
            session::approve(&disable)?;
            println!("approved: the routing daemon now starts at login. `revert` undoes this.");
            Ok(())
        }
        Cmd::Revert => {
            session::revert()?;
            println!("reverted to your old setup");
            Ok(())
        }
        Cmd::Restore => {
            let names = session::restore_paused()?;
            println!("restored: {}", if names.is_empty() { "nothing".into() } else { names.join(", ") });
            Ok(())
        }
    }
}

/// Wait for the registry to settle and return a snapshot of the live graph.
fn snapshot() -> Result<live::Live> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (keep, crx) = pipewire::channel::channel::<engine::Cmd>();
    std::thread::spawn(move || {
        let cfg = engine::EngineConfig {
            enforce: false,
            on_snapshot: Box::new(move |l| {
                let _ = tx.send(l.clone());
            }),
            paused: vec![],
            hotkeys: None,
            on_master: None,
        };
        let _ = engine::run(cfg, crx);
    });
    let mut live = rx.recv_timeout(std::time::Duration::from_secs(3))?;
    while let Ok(l) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
        live = l;
    }
    drop(keep);
    Ok(live)
}

fn print_plan() -> Result<()> {
    let profile = model::load_graph()?;
    let selected = ipc::status().map(|s| s.selected).unwrap_or_else(|| model::load_state().selected);
    let live = snapshot()?;
    let plan = planner::plan(&profile, &live, &selected);
    let port = |id: u32| {
        live.ports
            .get(&id)
            .map(|p| {
                let node = live.nodes.get(&p.node_id).map(|n| n.label()).unwrap_or("?");
                format!("{node}:{}", p.name)
            })
            .unwrap_or_else(|| format!("port {id}"))
    };
    let existing: std::collections::HashSet<_> = live.links.values().map(|l| (l.out_port, l.in_port)).collect();
    println!("virtual devices:");
    for b in &plan.buses {
        let state = if live.node_by_name(&b.name).is_some() { "exists (adopted)" } else { "would create" };
        println!("  {:<24} {state}", b.name);
    }
    if !plan.filters.is_empty() {
        println!("\nprocessing nodes:");
        for f in &plan.filters {
            let state = if live.node_by_name(f.name()).is_some() { "running" } else { "would start (links through it are made then)" };
            println!("  {:<24} {state}", f.name());
        }
    }
    let switches: Vec<_> = profile.nodes.iter().filter_map(|n| Some((n.kind.switch()?, plan.active_slots.get(&n.id).copied().flatten()))).collect();
    if !switches.is_empty() {
        println!("\nswitches:");
        for ((name, pins), sel) in switches {
            let pin = sel.map(|k| {
                let label = &pins[k as usize].label;
                if label.is_empty() { format!("pin {}", k + 1) } else { format!("pin {} ({label})", k + 1) }
            });
            println!("  {:<24} {}", name, pin.unwrap_or_else(|| "no pins".into()));
        }
    }
    println!("\nlinks:");
    for &(o, i) in &plan.links {
        let state = if existing.contains(&(o, i)) { "ok     " } else { "create " };
        println!("  {state} {} -> {}", port(o), port(i));
    }
    println!("\nconflicting links that would be removed:");
    for id in &plan.remove {
        let l = live.links[id];
        println!("  remove  {} -> {}", port(l.out_port), port(l.in_port));
    }
    if !plan.missing.is_empty() {
        println!();
    }
    for id in &plan.missing {
        match profile.nodes.iter().find(|n| n.id == *id).map(|n| &n.kind) {
            Some(model::NodeKind::Input { device: Some(d) } | model::NodeKind::Output { device: Some(d) }) => {
                println!("waiting for device: {} (not plugged in)", d.description)
            }
            Some(model::NodeKind::App { app: Some(a) }) => println!("waiting for app: {} (not playing or recording)", a.binary),
            _ => {}
        }
    }
    let name_of = |id: Option<u32>| id.and_then(|id| live.nodes.get(&id)).map_or("unchanged".into(), |n| n.label().to_owned());
    println!("\ndefault output: {}\ndefault input:  {}", name_of(plan.default_sink), name_of(plan.default_source));
    Ok(())
}
