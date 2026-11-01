mod dsp;
mod engine;
mod eqnode;
mod gui;
mod ipc;
mod live;
mod model;
mod planner;
mod session;

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
        /// Programs to pause. Default: qpwgraph, plus easyeffects if your profile doesn't use it.
        #[arg(long)]
        pause: Vec<String>,
    },
    /// Stop the daemon and give control back to your old setup.
    Stop,
    /// Switch to a profile instantly (bind this to a hotkey).
    Switch { profile: String },
    /// Show daemon status.
    Status,
    /// Dry run: show what a profile would change right now, without touching anything.
    Plan { profile: Option<String> },
    /// List profiles.
    Profiles,
    /// Make the new system start at login. Moves the given programs' autostart entries aside.
    Approve {
        /// Default: qpwgraph, plus easyeffects if your profile doesn't use it.
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
        Cmd::Switch { profile } => {
            if !model::list_profiles().contains(&profile) {
                bail!("no profile named {profile:?}; have: {}", model::list_profiles().join(", "));
            }
            if ipc::status().is_some() {
                match ipc::request(&ipc::Request::Switch { profile })? {
                    ipc::Response::Error(e) => bail!(e),
                    _ => Ok(()),
                }
            } else {
                model::save_state(&model::AppState { active_profile: Some(profile.clone()) })?;
                println!("daemon not running; {profile} will be used next time it starts");
                Ok(())
            }
        }
        Cmd::Status => {
            match ipc::status() {
                Some(s) => println!("{}", serde_json::to_string_pretty(&s)?),
                None => println!("daemon not running (your normal setup is in charge)"),
            }
            println!("approved: {}", session::is_approved());
            Ok(())
        }
        Cmd::Plan { profile } => {
            let name = profile
                .or(model::load_state().active_profile)
                .ok_or_else(|| anyhow::anyhow!("no profile given and none active"))?;
            print_plan(&name)
        }
        Cmd::Profiles => {
            let active = model::load_state().active_profile;
            for p in model::list_profiles() {
                let mark = if Some(&p) == active.as_ref() { "*" } else { " " };
                println!("{mark} {p}");
            }
            Ok(())
        }
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

fn print_plan(name: &str) -> Result<()> {
    let profile = model::load_profile(name)?;
    let live = snapshot()?;
    let plan = planner::plan(&profile, &live);
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
    println!("profile {name}\n\nvirtual devices:");
    for b in &plan.buses {
        let state = if live.node_by_name(&b.name).is_some() { "exists (adopted)" } else { "would create" };
        println!("  {:<24} {state}", b.name);
    }
    if !plan.eqs.is_empty() {
        println!("\nequalizers:");
        for e in &plan.eqs {
            let state = if live.node_by_name(&e.name).is_some() { "running" } else { "would start (links through it are made then)" };
            println!("  {:<24} {} band(s), {state}", e.name, e.bands.len());
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
