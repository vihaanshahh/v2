#[cfg(feature = "daemon")]
use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use v2::*;
use v2::models::Quant;
use v2::sources::{LoadOptions, ModelSource};

#[derive(Parser)]
#[command(
    name = "v2",
    version,
    about = "Which LLMs can you run — and how do you share that compute safely?",
    long_about = "v2 detects your hardware and tells you which models fit and how fast, \
                  manages them through Ollama, and can pool compute across your org over a \
                  secure mesh.\n\nRun `v2 about` for a guided overview, or `v2 <command> --help` \
                  for any command.",
    after_help = "Examples:\n  v2                     scan and rank models\n  v2 pull qwen3:8b       fit-check then download\n  v2 serve               meter local usage\n  v2 mesh join <ticket>  join an org compute mesh\n\nDocs: https://github.com/vihaanshahh/v2"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    #[arg(long, short, default_value_t = 4096, global = true)]
    ctx: u32,

    #[arg(long, global = true)]
    json: bool,

    #[arg(long, short, global = true)]
    verbose: bool,

    #[arg(long, short)]
    family: Option<String>,

    #[arg(long, value_enum, default_value_t = SourceArg::Auto, global = true)]
    source: SourceArg,

    #[arg(long, default_value = "", global = true)]
    ollama_host: String,

    #[arg(long, global = true)]
    accepted: Option<PathBuf>,

    #[arg(long, global = true)]
    enterprise: bool,
}

#[derive(Clone, Copy, ValueEnum, Default)]
enum SourceArg {
    #[default]
    Auto,
    Catalog,
    Ollama,
    All,
}

impl From<SourceArg> for ModelSource {
    fn from(v: SourceArg) -> Self {
        match v {
            SourceArg::Auto => ModelSource::Auto,
            SourceArg::Catalog => ModelSource::Catalog,
            SourceArg::Ollama => ModelSource::Ollama,
            SourceArg::All => ModelSource::All,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List models from the configured source
    Models,
    /// Check a specific model
    Check {
        query: String,
        #[arg(long, short)]
        quant: Option<String>,
    },
    /// Show the logo, version, and a command overview
    About,
    /// Run the metering proxy (and mesh serving, if a member)
    #[cfg(feature = "daemon")]
    Serve {
        /// Address to listen on for local apps
        #[arg(long, default_value = "127.0.0.1:11435")]
        listen: String,
        /// Also accept mesh peers on this address (requires membership)
        #[arg(long)]
        mesh_listen: Option<String>,
        /// Register with a relay and serve peers through it — no inbound port or
        /// exposed IP needed (e.g. relay.example:4840)
        #[arg(long)]
        relay: Option<String>,
        /// Cap CPU Ollama uses: a thread count (e.g. 4) or percent (e.g. 50%)
        #[arg(long)]
        cpu: Option<String>,
        /// Run the blocking proxy with no interactive panel (for systemd/daemons)
        #[arg(long)]
        headless: bool,
    },
    /// Print the OpenAI-compatible endpoint (Base URL + API key + models) to
    /// paste into any OpenAI tool. Auto-creates a key if none exists.
    #[cfg(feature = "daemon")]
    Endpoint {
        /// The address v2 serves on (must match your `v2 serve --listen`)
        #[arg(long, default_value = "127.0.0.1:11435")]
        listen: String,
    },
    /// Show models currently loaded in Ollama
    #[cfg(feature = "daemon")]
    Top,
    /// Summarize recorded usage (local + mesh)
    #[cfg(feature = "daemon")]
    Usage,
    /// Fit-check and download a model
    #[cfg(feature = "daemon")]
    Pull {
        model: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// Ensure a model is installed, then chat with it
    #[cfg(feature = "daemon")]
    Run {
        model: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// List installed models with fit info
    #[cfg(feature = "daemon")]
    Ps,
    /// Remove an installed model
    #[cfg(feature = "daemon")]
    Rm { model: String },
    /// Diagnose the v2 + Ollama + mesh setup
    #[cfg(feature = "daemon")]
    Doctor,
    /// Org mesh: share and use compute securely across a team
    #[cfg(feature = "daemon")]
    Mesh {
        #[command(subcommand)]
        cmd: MeshCmd,
    },
}

#[cfg(feature = "daemon")]
#[derive(Subcommand)]
enum MeshCmd {
    /// Create a new org (you become the admin)
    Init,
    /// Mint a one-time invite ticket pointing at your mesh address
    Invite {
        /// Your reachable direct mesh address, e.g. 203.0.113.5:4830. Omit when
        /// using --via-relay.
        addr: Option<String>,
        /// Reach you through a relay instead of a direct address — the ticket
        /// embeds relay://<relay>/<your-node-id>, hiding your IP.
        #[arg(long)]
        via_relay: Option<String>,
        /// Ticket lifetime in seconds (default 24h)
        #[arg(long, default_value_t = 86_400)]
        ttl: u64,
    },
    /// Run a relay: an org-agnostic rendezvous that brokers connections without
    /// exposing anyone's IP (forwards only encrypted traffic)
    Relay {
        /// Address to listen on, e.g. 0.0.0.0:4840
        #[arg(long, default_value = "0.0.0.0:4840")]
        listen: String,
    },
    /// Join an org using an invite ticket
    Join { ticket: String },
    /// Show this node's mesh identity and peers
    Status,
    /// Fetch and list reachable peers' node cards
    Peers,
    /// Add a peer address (host:port)
    PeerAdd { addr: String },
    /// Revoke a node by its id (admin only)
    Revoke { node: String },
    /// Stop accepting remote work and cancel in-flight jobs
    Pause,
    /// Resume offering compute to the mesh
    Resume,
    /// Run a model on the best available org peer
    Run {
        model: String,
        /// Prompt (remaining words)
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Print this node's public id
    Id,
    /// Trust another org with a scoped model allowlist (federation)
    FederationAdd {
        /// The other org's public id
        org: String,
        #[arg(long, default_value = "")]
        note: String,
        /// Comma-separated model globs the org may use here
        #[arg(long, value_delimiter = ',')]
        models: Vec<String>,
    },
    /// List federated orgs and their scopes
    FederationList,
    /// Verify and reconcile stored usage receipts
    Receipts,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let hw = hardware::detect();
    let accepted = accepted::AcceptedModels::load(cli.accepted.as_deref())?;
    let host = if cli.ollama_host.trim().is_empty() {
        ollama::default_host()
    } else {
        cli.ollama_host
    };
    let load_opts = LoadOptions {
        source: cli.source.into(),
        ollama_host: &host,
        accepted: accepted.as_ref(),
        enterprise: cli.enterprise,
    };

    match cli.command {
        None => {
            if cli.json {
                // The scan JSON already embeds a "hardware" block, so emit only
                // that — one valid JSON document, not two concatenated objects.
                run_scan(&hw, &load_opts, cli.ctx, cli.verbose, cli.family.as_deref(), true)?;
            } else {
                display::print_hardware(&hw, cli.ctx, &load_opts, accepted.as_ref());
                run_scan(&hw, &load_opts, cli.ctx, cli.verbose, cli.family.as_deref(), false)?;
            }
        }
        Some(Cmd::About) => {
            print_about();
        }
        Some(Cmd::Models) => {
            display::print_model_list(&sources::load(&load_opts)?);
        }
        Some(Cmd::Check { query, quant }) => {
            let quant_filter = quant.as_deref().and_then(Quant::from_label);
            if quant.is_some() && quant_filter.is_none() {
                return Err(format!(
                    "unknown quant {:?}. Valid: Q2_K Q3_K_M Q4_K_M Q5_K_M Q6_K Q8_0 F16",
                    quant.unwrap()
                ));
            }

            let q = query.to_lowercase();
            let matches: Vec<_> = sources::load(&load_opts)?
                .into_iter()
                .filter(|m| {
                    m.match_keys().iter().any(|k| k.contains(&q))
                        || m.display_name().to_lowercase().contains(&q)
                        || m.family.to_lowercase().contains(&q)
                })
                .collect();

            if matches.is_empty() {
                return Err(format!("no models matched {query:?}"));
            }

            if !cli.json {
                display::print_hardware(&hw, cli.ctx, &load_opts, accepted.as_ref());
            }

            let results: Vec<_> = matches
                .iter()
                .map(|m| engine::evaluate(m, &hw, cli.ctx, quant_filter))
                .collect();

            if cli.json {
                display::print_json(&hw, &results, cli.ctx);
            } else {
                display::print_results(&results, cli.verbose, &hw, cli.ctx);
            }
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Serve { listen, mesh_listen, relay, cpu, headless }) => {
            let activity = activity::Activity::new();
            let hw = std::sync::Arc::new(hw);
            let cores = proxy::cpu_cores();
            let initial_cpu = proxy::parse_cpu_spec(cpu.as_deref().unwrap_or(""), cores)?;
            let cpu_limit: proxy::CpuLimit = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(initial_cpu));
            // Lockdown: the proxy is meant to be loopback-only. Shout if it isn't.
            if !proxy::is_loopback(&listen) {
                eprintln!(
                    "warning: --listen {listen} is reachable from the network — anyone who can reach \
                     this host can use your Ollama. Bind 127.0.0.1 unless you mean to expose it."
                );
            }
            let mesh_addr = mesh_listen.clone();
            if mesh_listen.is_some() || relay.is_some() {
                let hw_arc = hw.clone();
                let host2 = host.clone();
                let act2 = activity.clone();
                let ml = mesh_listen.clone();
                let relay = relay.clone();
                std::thread::spawn(move || {
                    if let Err(e) = mesh::serve::daemon_with_relay(
                        &host2,
                        hw_arc,
                        act2,
                        ml.as_deref(),
                        relay.as_deref(),
                    ) {
                        eprintln!("v2 mesh: not serving to peers: {e}");
                    }
                });
            }
            let interactive = !headless
                && std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal();
            if interactive {
                // Proxy in the background; interactive control panel in front.
                let host2 = host.clone();
                let listen2 = listen.clone();
                let act2 = activity.clone();
                let cpu2 = cpu_limit.clone();
                std::thread::spawn(move || {
                    if let Err(e) = proxy::serve(&listen2, &host2, act2, cpu2) {
                        eprintln!("v2 proxy: {e}");
                    }
                });
                console::run(&host, hw.as_ref(), cli.ctx, &listen, mesh_addr.as_deref(), &cpu_limit, cores)?;
            } else {
                proxy::serve(&listen, &host, activity, cpu_limit)?;
            }
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Endpoint { listen }) => {
            proxy::print_endpoint_banner(&listen, &host, true)?;
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Top) => {
            let running = ollama_api::ps(&host)?;
            if running.is_empty() {
                ui::section("loaded");
                println!("  nothing loaded in Ollama right now");
            } else {
                ui::section(&format!("loaded  ({})", running.len()));
                for m in running {
                    let total = m.size as f64 / (1024.0 * 1024.0 * 1024.0);
                    let gpu_frac = if m.size > 0 { m.size_vram as f64 / m.size as f64 } else { 0.0 };
                    println!(
                        "  {}  {:>5.1}G  gpu {}",
                        ui::pad(&m.name, 26),
                        total,
                        ui::bar(gpu_frac, 12),
                    );
                }
            }
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Usage) => {
            usage::print_summary(&usage::read_all(), cli.json);
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Pull { model, yes }) => {
            manage::pull(&host, &hw, cli.ctx, &model, yes)?;
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Run { model, yes }) => {
            manage::run(&host, &hw, cli.ctx, &model, yes)?;
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Ps) => {
            manage::ps_installed(&host, &hw, cli.ctx)?;
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Rm { model }) => {
            manage::rm(&host, &model)?;
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Doctor) => {
            doctor(&host, &hw);
        }
        #[cfg(feature = "daemon")]
        Some(Cmd::Mesh { cmd }) => {
            run_mesh(cmd, &hw, cli.ctx)?;
        }
    }

    Ok(())
}

#[cfg(feature = "daemon")]
fn run_mesh(cmd: MeshCmd, hw: &hardware::HardwareInfo, ctx: u32) -> Result<(), String> {
    use mesh::client;
    match cmd {
        MeshCmd::Init => client::init(),
        MeshCmd::Invite { addr, via_relay, ttl } => client::invite(addr.as_deref(), via_relay.as_deref(), ttl),
        MeshCmd::Relay { listen } => mesh::relay::run_relay(&listen),
        MeshCmd::Join { ticket } => client::join(&ticket),
        MeshCmd::Status => client::status(),
        MeshCmd::Peers => client::peers(),
        MeshCmd::PeerAdd { addr } => client::peer_add(&addr),
        MeshCmd::Revoke { node } => client::revoke(&node),
        MeshCmd::Pause => client::pause(),
        MeshCmd::Resume => client::resume(),
        MeshCmd::Id => {
            let node = mesh::identity::NodeKey::load_or_create()?;
            println!("{}", node.public_b64());
            Ok(())
        }
        MeshCmd::Run { model, prompt } => {
            if prompt.is_empty() {
                return Err("provide a prompt: v2 mesh run <model> <prompt...>".into());
            }
            client::remote_run(hw, &model, ctx, &prompt.join(" "))
        }
        MeshCmd::FederationAdd { org, note, models } => client::federation_add(&org, &note, &models),
        MeshCmd::FederationList => client::federation_list(),
        MeshCmd::Receipts => client::receipts(),
    }
}

/// One badged line per subsystem: Ollama, identity, membership, policy.
#[cfg(feature = "daemon")]
fn doctor(host: &str, _hw: &hardware::HardwareInfo) {
    use doctor::Status;
    use ui::Badge;
    ui::section("doctor");

    let line = |status: Status, label: &str, msg: &str| {
        let b = match status {
            Status::Ok => Badge::Ok,
            Status::Warn => Badge::Warn,
            Status::Bad => Badge::Bad,
        };
        println!("  {}  {}  {}", ui::badge(b), ui::pad(label, 9), msg);
    };

    let report = doctor::doctor_report(host);
    line(report.ollama.status, &report.ollama.label, &report.ollama.message);
    line(report.identity.status, &report.identity.label, &report.identity.message);
    line(report.mesh.status, &report.mesh.label, &report.mesh.message);
    line(report.policy.status, &report.policy.label, &report.policy.message);
    line(report.abuse.status, &report.abuse.label, &report.abuse.message);
    println!();
}

fn print_about() {
    use colored::Colorize;
    println!("{}", ui::logo());
    println!("  {} {}", "v2".bold(), format!("v{}", ui::version()).dimmed());
    println!(
        "  {}\n",
        "which LLMs can you run — and how do you share that compute safely?".dimmed()
    );

    ui::section("common commands");
    let cmds = [
        ("v2", "scan hardware, rank models by fit + speed"),
        ("v2 check <model>", "check one model at every quant"),
        ("v2 pull <model>", "fit-check, then download"),
        ("v2 run <model>", "chat with a local model"),
        ("v2 ps / top", "installed models / what's loaded now"),
        ("v2 serve", "metering proxy (+ mesh serving)"),
        ("v2 usage", "recorded token usage"),
        ("v2 mesh init|join", "create or join an org compute mesh"),
        ("v2 mesh run <model>", "run on the best org peer"),
        ("v2 doctor", "diagnose ollama / identity / policy"),
    ];
    for (c, d) in cmds {
        println!("  {}  {}", ui::pad(&c.cyan().to_string(), 22), d.dimmed());
    }

    ui::section("learn more");
    println!("  {}  run any command with {}", ui::pad("help", 22), "--help".cyan());
    println!("  {}  {}", ui::pad("docs", 22), "https://github.com/vihaanshahh/v2".cyan());
    println!();
}

fn run_scan(
    hw: &hardware::HardwareInfo,
    load_opts: &LoadOptions<'_>,
    ctx: u32,
    verbose: bool,
    family: Option<&str>,
    json: bool,
) -> Result<(), String> {
    let models: Vec<_> = sources::load(load_opts)?
        .into_iter()
        .filter(|m| {
            family
                .map(|f| m.family.to_lowercase().contains(&f.to_lowercase()))
                .unwrap_or(true)
        })
        .collect();

    let results: Vec<_> = models
        .iter()
        .map(|m| engine::evaluate(m, hw, ctx, None))
        .collect();

    if json {
        display::print_json(hw, &results, ctx);
    } else {
        display::print_results(&results, verbose, hw, ctx);
    }
    Ok(())
}
