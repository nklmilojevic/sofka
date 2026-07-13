//! sofka — a Kubernetes TUI, reimagined in Rust.
//!
//! A from-scratch reimagining of k9s built on kube-rs + ratatui, async-first.

mod app;
mod columns;
mod config;
mod explain;
mod filter;
mod gitops;
mod helm;
mod journal;
mod k8s;
mod keys;
mod providers;
mod store;
mod theme;
mod thresholds;
mod timeline;
mod ui;
mod views;

use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{Event, KeyEventKind};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::app::App;
use crate::k8s::Cluster;

const EVENT_CHANNEL_CAP: usize = 4096;

/// sofka: navigate, observe, and inspect your Kubernetes clusters.
#[derive(Parser, Debug)]
#[command(name = "sofka", version, about)]
struct Args {
    /// Resource to open on launch (alias/plural/kind), e.g. pods, svc, dp.
    /// Defaults to config `default_resource`, then "pods".
    resource: Option<String>,

    /// Namespace to start in.
    #[arg(short, long)]
    namespace: Option<String>,

    /// Start across all namespaces.
    #[arg(short = 'A', long)]
    all_namespaces: bool,

    /// Disable every action that could modify the cluster (delete, edit,
    /// scale, shell, plugins, …). Overrides the config `readonly` option,
    /// including per-cluster/per-context overrides, for the whole session.
    #[arg(long, conflicts_with = "write")]
    readonly: bool,

    /// Force write mode, overriding any config `readonly` option for the
    /// whole session.
    #[arg(long)]
    write: bool,

    /// Connect, run discovery, print a summary, and exit (no TUI). Useful for
    /// verifying cluster connectivity in CI or a headless shell.
    #[arg(long)]
    check: bool,

    /// Render a single frame of the resource view to stdout and exit (no TTY
    /// needed). Lets you eyeball the UI headlessly.
    #[arg(long)]
    snapshot: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (loader, mut config_warnings) = config::ConfigLoader::load();

    // Connect before taking over the terminal so errors are readable. An
    // unreachable current context isn't fatal for the interactive TUI: start
    // in the context picker instead (k9s behavior). Headless modes still exit
    // with the error, since there is no picker to fall back to.
    eprintln!("Connecting to cluster…");
    let (mut cluster, connect_error) = match Cluster::connect().await {
        Ok(c) => (c, None),
        Err(e) if args.check || args.snapshot => {
            eprintln!("\x1b[31merror:\x1b[0m {e:#}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("\x1b[33mwarning:\x1b[0m {e:#}");
            (Cluster::disconnected(), Some(format!("{e:#}")))
        }
    };
    // Per-cluster/per-context override files merge over the base config.
    let resolved = loader.resolve(&cluster.context, &cluster.cluster_name);
    for w in &resolved.warnings {
        eprintln!("warning: {w}");
    }
    config_warnings.extend(resolved.warnings.clone());
    let cfg = resolved.config;
    cluster.add_aliases(&cfg.aliases);

    if args.check {
        println!("✓ connected");
        println!("  context:    {}", cluster.context);
        println!("  cluster:    {}", cluster.cluster_name);
        println!("  server:     {}", cluster.cluster_url);
        println!("  namespace:  {}", cluster.default_namespace);
        println!(
            "  kinds:      {} resource types discovered",
            cluster.catalog.len()
        );
        for alias in ["pods", "po", "dp", "svc", "no", "ns", "cm"] {
            match cluster.resolve(alias) {
                Some(k) => println!(
                    "  resolve {alias:<5} → {} (namespaced={})",
                    k.title(),
                    k.namespaced
                ),
                None => println!("  resolve {alias:<5} → <unresolved>"),
            }
        }
        match cluster.namespaces().await {
            Ok(ns) => println!("  namespaces: {}", ns.len()),
            Err(e) => println!("  namespaces: error: {e}"),
        }
        return Ok(());
    }

    // Install the initial color skin before anything renders. Auto-detecting
    // dark/light mode queries the terminal directly, so it must run before
    // ratatui switches to the alternate screen. A skin named by an override
    // file for the starting context wins over the base/auto-detected skin,
    // but only the base skin becomes the session skin, so switching away
    // from an overridden context falls back correctly.
    let session_skin = loader
        .resolve("", "")
        .config
        .skin
        .name
        .unwrap_or_else(|| theme::auto_skin_name().to_string());
    let initial_skin = resolved
        .skin_override
        .clone()
        .unwrap_or_else(|| session_skin.clone());
    theme::init(theme::resolve_skin(Some(&initial_skin), &cfg.skin.colors));
    theme::set_background(cfg.skin.background);

    let (tx, mut rx) = mpsc::channel(EVENT_CHANNEL_CAP);
    let mut app = App::new(cluster, tx);
    // Kubeconfig contexts are stable for the session; cache them once so the
    // palette can complete `:ctx <name>` without re-reading the file per keystroke.
    app.all_contexts = Cluster::list_contexts();
    app.user_aliases = cfg.aliases.clone();
    app.namespace_favorites = cfg.favorite_namespaces.clone();
    app.plugins = cfg.plugins.clone();
    app.bookmarks = cfg.bookmarks.clone();
    app.workspaces = cfg.workspaces.clone();
    app.guardrails = cfg.guardrails.clone();
    for w in config::plugin_warnings(&app.plugins)
        .into_iter()
        .chain(config::bookmark_warnings(&app.bookmarks))
        .chain(config::workspace_warnings(&app.workspaces))
        .chain(config::guardrail_warnings(&app.guardrails))
    {
        eprintln!("warning: {w}");
        config_warnings.push(w);
    }
    let (user_views, view_warnings) = views::compile(&cfg.views);
    for w in &view_warnings {
        eprintln!("warning: {w}");
    }
    app.user_views = user_views;
    let (thresholds, threshold_warnings) = thresholds::compile(&cfg.thresholds);
    for w in &threshold_warnings {
        eprintln!("warning: {w}");
    }
    app.thresholds = thresholds;
    let (log_provider, provider_warnings) = providers::compile(cfg.providers.logs.as_ref());
    for w in &provider_warnings {
        eprintln!("warning: {w}");
    }
    app.log_provider = log_provider;
    app.skin_colors = cfg.skin.colors.clone();
    app.config = loader;
    app.session_skin = Some(session_skin);
    app.active_skin = Some(initial_skin);
    // Keep initial-load validation problems visible in-app (`:config`), not
    // just on the stderr that the alternate screen is about to cover.
    config_warnings.extend(threshold_warnings);
    app.config_warnings = config_warnings;
    // CLI flags pin the mode for the whole session; otherwise config decides,
    // re-resolved per context on every `:ctx` switch.
    app.readonly_override = match (args.readonly, args.write) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    app.readonly = app.readonly_override.unwrap_or(cfg.readonly);
    if args.all_namespaces {
        app.namespace = String::new();
    } else if let Some(ns) = args.namespace {
        app.namespace = ns;
    } else if let Some(ns) = cfg.default_namespace.clone() {
        app.namespace = ns;
    }
    let resource = args
        .resource
        .or(cfg.default_resource)
        .unwrap_or_else(|| "pods".into());
    match &connect_error {
        // No cluster to watch — open the context picker over the empty table;
        // a successful pick connects and lands on the default resource.
        Some(err) => app.start_disconnected(err),
        None => app.switch_kind(&resource),
    }
    // View config problems must be visible inside the TUI, not only on the
    // (about-to-be-hidden) stderr.
    if let Some(w) = view_warnings.first() {
        app.flash = w.clone();
        app.flash_err = true;
    }

    if args.snapshot {
        return snapshot(&mut app, &mut rx).await;
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &mut rx).await;
    ratatui::restore();
    result
}

/// Populate the store from the watch for a short window, then render one frame
/// to an in-memory backend and print it. Headless UI smoke test.
async fn snapshot(app: &mut App, rx: &mut mpsc::Receiver<store::Msg>) -> Result<()> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    match std::env::var("PUP_DEMO").as_deref() {
        Ok("pulse") => app.open_pulse(),
        Ok("xray") => app.open_xray(),
        _ => {}
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Ok(Some(msg)) => app.handle_msg(msg),
            Ok(None) => break,
            Err(_) => {} // keep polling until the deadline (let metrics arrive)
        }
    }

    // Optional overlay demo for headless visual verification of popups.
    match std::env::var("PUP_DEMO").as_deref() {
        Ok("prompt") => {
            app.prompt_label = "Scale sherlock to replicas (current 1):".into();
            app.prompt_input = "3".into();
            app.mode = app::Mode::Prompt;
        }
        Ok("ns") => {
            app.ns_list = vec![
                "<all>".into(),
                "default".into(),
                "kube-system".into(),
                "sherlock".into(),
            ];
            app.ns_state.select(Some(0));
            app.mode = app::Mode::Namespaces;
        }
        Ok("logs") => {
            app.logs.view.title = "sherlock — logs".into();
            app.logs.view.lines = (1..=40)
                .map(|i| {
                    format!(
                        "2026-06-15T12:0{}:00Z [info] request {i} handled in {}ms",
                        i % 6,
                        i * 3
                    )
                })
                .collect();
            app.logs.follow = true;
            app.mode = app::Mode::Logs;
        }
        Ok("diff") => {
            // Try a real diff; fall back to synthetic content to show the view.
            app.table_state.select(Some(0));
            app.open_diff();
            if app.mode != app::Mode::Diff {
                app.detail = app::Scrollable {
                    title: "web — diff (last-applied → live)".into(),
                    lines: vec![
                        " spec:".into(),
                        "   replicas: 3".into(),
                        "-  image: web:v1.2.0".into(),
                        "+  image: web:v1.3.0".into(),
                        "   ports:".into(),
                        "-  - containerPort: 8080".into(),
                        "+  - containerPort: 9090".into(),
                    ]
                    .into(),
                    ..Default::default()
                };
                app.mode = app::Mode::Diff;
            }
        }
        Ok("palette") => {
            app.command = "de".into();
            app.cmd_suggestions = [
                "deployments",
                "daemonsets",
                "endpoints",
                "endpointslices",
                "events",
            ]
            .into_iter()
            .map(|s| app::Suggestion {
                label: s.into(),
                kind: app::SuggestKind::Resource,
            })
            .collect();
            app.cmd_sel = 0;
            app.mode = app::Mode::Command;
        }
        Ok("image") => {
            app.container_list = vec!["app".into(), "istio-proxy".into()];
            app.image_values = vec![
                "registry.litehub.io/sherlock:v2.4.1".into(),
                "docker.io/istio/proxyv2:1.22.0".into(),
            ];
            app.container_state.select(Some(0));
            app.mode = app::Mode::SetImage;
        }
        _ => {}
    }

    let mut terminal = Terminal::new(TestBackend::new(120, 32))?;
    terminal.draw(|f| ui::draw(f, app))?;
    let buffer = terminal.backend().buffer().clone();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

/// Leave the alt-screen/raw-mode TUI, run an interactive command with inherited
/// stdio (kubectl exec/edit/port-forward), then restore the TUI.
fn suspend_and_run(terminal: &mut ratatui::DefaultTerminal, argv: &[String]) {
    if argv.is_empty() {
        return;
    }
    ratatui::restore();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    let _ = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status();
    *terminal = ratatui::init();
    let _ = terminal.clear();
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::Receiver<store::Msg>,
) -> Result<()> {
    let mut reader = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key)?;
                        if let Some(app::Suspend::Shell(argv)) = app.pending.take() {
                            suspend_and_run(terminal, &argv);
                            app.flash = format!("ran: {}", argv.join(" "));
                            app.flash_err = false;
                        }
                    }
                    Some(Err(_)) | None => return Ok(()),
                    _ => {}
                }
            }
            Some(msg) = rx.recv() => {
                app.handle_msg(msg);
                // Batch any other queued updates before the next redraw.
                while let Ok(m) = rx.try_recv() {
                    app.handle_msg(m);
                }
            }
            _ = tick.tick() => app.reap_port_forwards(), // age columns + drop dead forwards
        }
    }
}
