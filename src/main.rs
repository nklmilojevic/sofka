//! sofka — a Kubernetes TUI, reimagined in Rust.
//!
//! A from-scratch reimagining of k9s built on kube-rs + ratatui, async-first.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use crossterm::event::{Event, KeyEventKind};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use sofka::app::App;
use sofka::k8s::Cluster;
use sofka::{
    altscroll, app, applog, config, diagnostics, fleet, k8s, nsmem, providers, sortmem, store,
    theme, thresholds, ui, views,
};

mod terminal_title;

const EVENT_CHANNEL_CAP: usize = 4096;

/// sofka: navigate, observe, and inspect your Kubernetes clusters.
#[derive(Parser, Debug)]
#[command(name = "sofka", version, about)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Resource to open on launch (alias/plural/kind), e.g. pods, svc, dp.
    /// Use ctx or contexts to choose a context before connecting.
    /// Defaults to config `default_resource`, then "pods".
    resource: Option<String>,

    /// Namespace to start in.
    #[arg(short, long)]
    namespace: Option<String>,

    /// Start across all namespaces.
    #[arg(short = 'A', long)]
    all_namespaces: bool,

    /// Kubeconfig context to start in (defaults to the current context).
    #[arg(long)]
    context: Option<String>,

    /// Path to the kubeconfig file (sets $KUBECONFIG for the whole session,
    /// including kubectl shell-outs).
    #[arg(long, value_name = "PATH")]
    kubeconfig: Option<PathBuf>,

    /// Allow X.509 v1 client certificates for this run. Does not disable server checks.
    #[arg(long)]
    allow_v1_client_cert: bool,

    /// Disable TLS session resumption for this run, including context changes.
    #[arg(long)]
    no_tls_resumption: bool,

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

    /// Validate a plugin package without executing it or connecting to a cluster.
    #[arg(long, value_name = "DIR", conflicts_with_all = ["check", "snapshot", "info", "validate_plugin_report"])]
    validate_plugin: Option<PathBuf>,

    /// Validate and render a versioned plugin JSON report without a cluster.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["check", "snapshot", "info"])]
    validate_plugin_report: Option<PathBuf>,

    /// Deprecated alias for `sofka info --offline`.
    #[arg(long)]
    info: bool,

    /// Run a core plugin's adapter: read a plugin request on stdin, write its
    /// report on stdout. sofka spawns itself with this; it is not a user-facing
    /// entry point, which is why it is hidden from `--help`.
    #[arg(long, value_name = "NAME", hide = true)]
    plugin_adapter: Option<String>,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum Command {
    /// Print runtime diagnostics and exit: version and build, config sources,
    /// context/cluster/API server, discovery and Metrics API status, request
    /// latency, logging, and the directories sofka uses.
    ///
    /// Connects to the cluster (briefly) unless `--offline`. Identifiers,
    /// paths, and counts only — never credentials, tokens, or Secret values.
    Info(InfoArgs),
}

#[derive(clap::Args, Debug, Clone, Default)]
struct InfoArgs {
    /// Report only what can be known without a cluster: build, config sources,
    /// kubeconfig context, logging, and directories.
    #[arg(long)]
    offline: bool,
}

/// Heap profiling build (`--features dhat-heap`). dhat replaces the global
/// allocator and writes `dhat-heap.json` when `_profiler` drops, so the guard
/// has to outlive the whole run — hence the binding in `main` rather than a
/// helper. Attributes RSS to a call site, which is the only way to check the
/// memory claims against reality rather than arithmetic.
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() -> Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let args = Args::parse();
    // `--kubeconfig`: export for the whole process so every kubeconfig read
    // (kube-rs config inference, context listing/switching, `--info`) and
    // every kubectl shell-out sees the same file.
    // SAFETY: single-threaded here — the tokio runtime only spawns below.
    if let Some(path) = &args.kubeconfig {
        unsafe { std::env::set_var("KUBECONFIG", path) };
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting tokio runtime")?
        .block_on(run_main(args))
}

impl Args {
    fn launch_namespace(&self) -> Option<String> {
        if self.all_namespaces {
            Some(String::new())
        } else {
            self.namespace.clone()
        }
    }

    fn validate_picker_context(&self, contexts: &[String]) -> Result<()> {
        if let Some(name) = &self.context {
            anyhow::ensure!(
                contexts.contains(name),
                "context '{name}' not found in kubeconfig"
            );
        }
        Ok(())
    }

    fn context_picker(&self) -> Result<bool> {
        let picker = matches!(self.resource.as_deref(), Some("ctx" | "contexts"));
        anyhow::ensure!(
            !picker || !(self.check || self.snapshot),
            "ctx and contexts require interactive mode; remove --check or --snapshot"
        );
        Ok(picker)
    }
}

async fn run_main(args: Args) -> Result<()> {
    // Before anything else: an adapter run owns stdout for its report and must
    // never load config, connect, or touch the terminal.
    if let Some(name) = &args.plugin_adapter {
        return match name.as_str() {
            "sanitize" => {
                sofka::sanitize::run(args.allow_v1_client_cert, args.no_tls_resumption).await
            }
            other => Err(anyhow::anyhow!("unknown core plugin adapter '{other}'")),
        };
    }
    if let Some(dir) = &args.validate_plugin {
        let plugin = sofka::plugins::read_package(dir).map_err(anyhow::Error::msg)?;
        sofka::plugins::available(&plugin).map_err(anyhow::Error::msg)?;
        println!("valid plugin: {}", plugin.name);
        return Ok(());
    }
    if let Some(path) = &args.validate_plugin_report {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take((sofka::plugins::MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        for line in sofka::plugins::render_report(&bytes).map_err(anyhow::Error::msg)? {
            println!("{line}");
        }
        return Ok(());
    }

    let (loader, mut config_warnings) = config::ConfigLoader::load();

    // Logging starts before the first cluster request, so the base config
    // decides it — a per-cluster override would arrive too late to record the
    // connection it belongs to.
    let base = loader.resolve("", "").config;
    start_logging(&base.logging, &mut config_warnings);

    // `sofka info` (and the deprecated `--info`): report and exit.
    if let Some(info) = info_request(&args) {
        let result = run_info(&info, &args, &loader, &mut config_warnings).await;
        applog::shutdown();
        return result;
    }

    let context_picker = args.context_picker()?;

    // Connect before taking over the terminal so errors are readable. An
    // unreachable current context isn't fatal for the interactive TUI: start
    // in the context picker instead (k9s behavior). Headless modes still exit
    // with the error, since there is no picker to fall back to.
    let (mut cluster, connect_error) = if context_picker {
        if args.context.is_some() {
            args.validate_picker_context(&Cluster::list_contexts().map_err(anyhow::Error::msg)?)?;
        }
        (Cluster::disconnected(args.context.as_deref()), None)
    } else {
        eprintln!("Connecting to cluster…");
        let connect = match args.context.as_deref() {
            Some(name) => {
                Cluster::connect_context(name, args.allow_v1_client_cert, args.no_tls_resumption)
                    .await
            }
            None => Cluster::connect(args.allow_v1_client_cert, args.no_tls_resumption).await,
        };
        match connect {
            Ok(c) => (c, None),
            Err(e) if args.check || args.snapshot => {
                eprintln!("\x1b[31merror:\x1b[0m {e:#}");
                applog::shutdown();
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("\x1b[33mwarning:\x1b[0m {e:#}");
                (
                    Cluster::disconnected(args.context.as_deref()),
                    Some(format!("{e:#}")),
                )
            }
        }
    };
    cluster.allow_v1_client_cert = args.allow_v1_client_cert;
    cluster.no_tls_resumption = args.no_tls_resumption;
    for w in cluster
        .discovery_fallback
        .iter()
        .chain(&cluster.discovery_warnings)
    {
        eprintln!("\x1b[33mwarning:\x1b[0m {w}");
    }
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
        println!(
            "  k8s rev:    {}",
            if cluster.server_version.is_empty() {
                "unknown"
            } else {
                &cluster.server_version
            }
        );
        println!("  namespace:  {}", cluster.default_namespace);
        println!(
            "  kinds:      {} resource types discovered",
            cluster.catalog.len()
        );
        let not_read = cluster.discovery_warnings.len();
        if not_read > 0 {
            let noun = if not_read == 1 { "group" } else { "groups" };
            println!("  not read:   {not_read} API {noun}. Refer to the warnings above.");
        }
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
        applog::shutdown();
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
    let panic_tx = tx.clone();
    let mut app = App::new(cluster, tx);
    app.config = loader;
    if let Err(error) = app.journal.configure(&cfg.journal) {
        eprintln!("warning: {error}");
        config_warnings.push(error);
    }
    match sofka::state_writer::StateWriter::new(app.tx.clone()) {
        Ok(writer) => app.state_writer = Some(writer),
        Err(e) => {
            eprintln!("warning: {e}; state writes will run synchronously");
            config_warnings.push(e);
        }
    }
    // Fleet marks (`space` in `:ctx`) persist under the state dir, overlaying
    // the `[fleet] contexts` config list across restarts.
    let fleet_marks_path = fleet::FleetMarks::default_path();
    app.fleet_marks = fleet::FleetMarks::load(&fleet_marks_path);
    app.fleet_marks_path = Some(fleet_marks_path);
    // Remembered sort columns (`S`/`I`/header clicks) persist the same way,
    // restored per kind on every view start.
    let sort_memory_path = sortmem::SortMemory::default_path();
    app.sort_memory = sortmem::SortMemory::load(&sort_memory_path);
    app.sort_memory_path = Some(sort_memory_path);
    app.remember_sort = cfg.remember_sort.unwrap_or(true);
    app.mouse_scroll_lines =
        config::mouse_scroll_lines(cfg.mouse_scroll_lines, &mut config_warnings);
    app.hide_header = cfg.hide_header;
    app.compact = cfg.compact_mode;
    app.terminal_title = cfg.terminal_title.unwrap_or(true);
    // The last namespace picked per context persists too, so a relaunch (or
    // a `:ctx` switch back) lands where you left off.
    let namespace_memory_path = nsmem::NamespaceMemory::default_path();
    app.namespace_memory = nsmem::NamespaceMemory::load(&namespace_memory_path);
    app.namespace_memory_path = Some(namespace_memory_path);
    // Kubeconfig contexts are stable for the session; cache them once so the
    // palette can complete `:ctx <name>` without re-reading the file per keystroke.
    match Cluster::list_contexts() {
        Ok(contexts) => app.all_contexts = contexts,
        Err(e) => {
            eprintln!("warning: {e}");
            config_warnings.push(e);
        }
    }
    app.user_aliases = cfg.aliases.clone();
    app.namespace_favorites = cfg.favorite_namespaces.clone();
    app.plugins = cfg.plugins.clone();
    app.bookmarks = cfg.bookmarks.clone();
    app.workspaces = cfg.workspaces.clone();
    app.guardrails = cfg.guardrails.clone();
    app.debug = cfg.debug.clone();
    app.bundle_cfg = cfg.bundle.clone();
    app.pvc_cfg = cfg.pvc_explore.clone();
    app.logs_cfg = cfg.logs.clone();
    // Seed the session toggle once; later `F` presses (and per-context config
    // reloads) don't fight the user's in-session choice.
    app.logs.fullscreen = cfg.logs.fullscreen;
    app.fleet_cfg = cfg.fleet.clone();
    app.drain_cfg = cfg.drain;
    app.forwards_cfg = cfg.forwards.clone();
    app.notify_cfg = cfg.notify.clone();
    for w in config::plugin_warnings(&app.plugins)
        .into_iter()
        .chain(config::bookmark_warnings(&app.bookmarks))
        .chain(config::workspace_warnings(&app.workspaces))
        .chain(config::guardrail_warnings(&app.guardrails))
        .chain(config::forward_warnings(&app.forwards_cfg))
        .chain(config::notify_warnings(&app.notify_cfg))
        .chain(config::pvc_explore_warnings(&app.pvc_cfg))
    {
        eprintln!("warning: {w}");
        config_warnings.push(w);
    }
    let key_warnings = app.configure_keys(&cfg.keys);
    for w in key_warnings {
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
    let (metrics_provider, metrics_warnings) =
        providers::compile_metrics(cfg.providers.metrics.as_ref());
    for w in &metrics_warnings {
        eprintln!("warning: {w}");
    }
    app.metrics_provider = metrics_provider;
    app.skin_colors = cfg.skin.colors.clone();
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
    // CLI flags win; then the namespace remembered for this context from the
    // last session; then the config default; then the kubeconfig's.
    let launch_namespace = args.launch_namespace();
    if let Some(ns) = &launch_namespace {
        app.namespace = ns.clone();
    } else if let Some(ns) = app.namespace_memory.get(&app.cluster.context) {
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
        Some(err) => app.start_disconnected(err, launch_namespace),
        None if context_picker => app.start_context_picker(launch_namespace),
        None => {
            app.switch_kind(&resource);
            app.start_autostart_forwards();
        }
    }
    // View config problems must be visible inside the TUI, not only on the
    // (about-to-be-hidden) stderr.
    if let Some(w) = view_warnings.first() {
        app.flash = w.clone();
        app.flash_err = true;
    }
    app.flash_discovery_warnings();

    if args.snapshot {
        let result = snapshot(&mut app, &mut rx).await;
        applog::shutdown();
        return result;
    }

    let mouse = cfg.mouse.unwrap_or(true);
    let mut terminal = ratatui::init();
    if mouse {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    }
    install_panic_hook(panic_tx);
    let result = run(&mut terminal, &mut app, &mut rx, mouse).await;
    // Still inside the runtime, so this actually completes — a spawned delete
    // would not, and the helper pod would sit out its TTL holding the volume.
    app.shutdown_pvc_helper().await;
    // Disable before leaving the alternate screen so the shell never sees
    // mouse-report sequences (harmless if capture was never enabled).
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    terminal_title::clear();
    ratatui::restore();
    // `restore()` leaves the alternate screen but never re-shows the cursor
    // that `draw` hid, so without this the user's shell prompt has no cursor.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    if let Some(error) = app.journal.shutdown() {
        eprintln!("warning: {error}");
    }
    sofka::log_info!("shutdown", quit = app.should_quit);
    applog::shutdown();
    result
}

/// Route background-task panics into the TUI instead of through ratatui's
/// restore. Tokio catches a panic in a spawned task and the process survives —
/// but ratatui's hook has already torn the terminal down by then, leaving a
/// live TUI drawing into the primary screen with raw mode off and no usable
/// input. Report those as an in-app error flash and keep the terminal alone.
/// A main-thread panic is fatal, so there the ratatui hook (restore + default
/// report) is right; we only add the cursor it forgets.
///
/// Must be installed after `ratatui::init()` so the previous hook is ratatui's.
fn install_panic_hook(tx: mpsc::Sender<store::Msg>) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            // Mouse capture must go first: ratatui's restore leaves the
            // alternate screen, and stray mouse reports would land in the
            // shell after a crash.
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
            terminal_title::clear();
            prev(info);
            let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
        } else {
            let _ = tx.try_send(store::Msg::Panic(info.to_string()));
        }
    }));
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
                app.detail = app::Scrollable::doc(
                    "web — diff (last-applied → live)".into(),
                    vec![
                        " spec:".into(),
                        "   replicas: 3".into(),
                        "-  image: web:v1.2.0".into(),
                        "+  image: web:v1.3.0".into(),
                        "   ports:".into(),
                        "-  - containerPort: 8080".into(),
                        "+  - containerPort: 9090".into(),
                    ],
                );
                app.mode = app::Mode::Diff;
            }
        }
        // Built last so the latency table has the session's real requests in
        // it — the numbers are half of what this view is for.
        Ok("info") => app.open_info(),
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

/// Emit the configured bell + desktop-notification sequences for a `:notify`
/// event (see [`config::NotifyConfig`] and `app::notify::notification_sequence`).
fn ring_notification(text: &str, cfg: &config::NotifyConfig) {
    let seq = app::notification_sequence(text, cfg);
    if !seq.is_empty() {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::style::Print(seq));
    }
}

/// Leave the alt-screen/raw-mode TUI, run an interactive command with inherited
/// stdio (kubectl exec/edit/port-forward), then restore the TUI. Toggles the
/// terminal modes directly rather than `ratatui::restore()`/`init()`: `init()`
/// stacks another panic hook on every call, and the hook installed at startup
/// must stay the outermost one. `captured` is whether mouse capture is on
/// right now (not just the config flag), so the pre-suspend state is restored
/// exactly.
fn suspend_and_run(terminal: &mut ratatui::DefaultTerminal, argv: &[String], captured: bool) {
    use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    if argv.is_empty() {
        return;
    }
    if captured {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status();
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), EnterAlternateScreen);
    if captured {
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    }
    let _ = terminal.clear();
}

/// Which diagnostics report the CLI asked for, if any. The `--info` flag is
/// the deprecated spelling and keeps its documented contract: no connection.
fn info_request(args: &Args) -> Option<InfoArgs> {
    match &args.command {
        Some(Command::Info(info)) => Some(info.clone()),
        None if args.info => Some(InfoArgs { offline: true }),
        None => None,
    }
}

/// Start the structured log, turning a failure into a warning rather than a
/// failed launch: a log sofka cannot write is not a reason to refuse to run.
fn start_logging(cfg: &config::LoggingConfig, warnings: &mut Vec<String>) {
    for w in config::logging_warnings(cfg) {
        eprintln!("warning: {w}");
        warnings.push(w);
    }
    let (level, env_warning) = applog::resolve_level(&cfg.level);
    if let Some(w) = env_warning {
        eprintln!("warning: {w}");
        warnings.push(w);
    }
    if let Err(e) = applog::init(level, cfg.path(), cfg.max_bytes()) {
        eprintln!("warning: {e}");
        warnings.push(e);
        return;
    }
    sofka::log_info!(
        "startup",
        version = diagnostics::VERSION,
        build = diagnostics::build_line(),
        logging = level.as_str()
    );
}

/// `sofka info`: runtime diagnostics to stdout.
///
/// Connects briefly unless `--offline`, because discovery and Metrics API
/// status are the half of this report you cannot get from disk. A failed
/// connection is reported and the rest of the report still prints — the
/// command's job is to explain a broken setup, not to fail with it.
///
/// Emits identifiers, paths, and counts only; every value that could carry a
/// credential is redacted first.
async fn run_info(
    info: &InfoArgs,
    args: &Args,
    loader: &config::ConfigLoader,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let cluster = if info.offline {
        None
    } else {
        eprintln!("Connecting to cluster…");
        match args.context.as_deref() {
            Some(name) => {
                Cluster::connect_context(name, args.allow_v1_client_cert, args.no_tls_resumption)
                    .await
            }
            None => Cluster::connect(args.allow_v1_client_cert, args.no_tls_resumption).await,
        }
        .inspect_err(|e| {
            eprintln!(
                "\x1b[33mwarning:\x1b[0m {}",
                diagnostics::safe(&format!("{e:#}"))
            )
        })
        .ok()
    };

    // Identity comes from the live connection when there is one, and from the
    // kubeconfig otherwise, so the config-source section resolves the same
    // per-cluster overrides the TUI would.
    let (context, cluster_name, server) = match &cluster {
        Some(c) => (
            c.context.clone(),
            c.cluster_name.clone(),
            c.cluster_url.clone(),
        ),
        None => k8s::context_info(args.context.as_deref())
            .unwrap_or_else(|| ("(none)".into(), String::new(), String::new())),
    };

    let resolved = loader.resolve(&context, &cluster_name);
    warnings.extend(resolved.warnings.clone());
    let cfg = resolved.config;

    // Exactly what a launch with these flags would open, so the probe below
    // exercises the view the user would actually land on.
    let resource = args
        .resource
        .clone()
        .or_else(|| cfg.default_resource.clone())
        .unwrap_or_else(|| "pods".into());
    let namespace = starting_namespace(
        args,
        &cfg,
        &context,
        cluster
            .as_ref()
            .map(|c| c.default_namespace.clone())
            .or_else(|| k8s::context_namespace(&context)),
    );

    let mut lines = diagnostics::version_lines();

    lines.push(String::new());
    lines.push("Cluster".into());
    lines.push(format!(
        "  connected:   {}",
        match &cluster {
            Some(_) => "yes",
            None if info.offline => "not attempted (--offline)",
            None => "no",
        }
    ));
    lines.push(format!(
        "  context:     {}",
        diagnostics::safe_or(&context, "(none)")
    ));
    lines.push(format!(
        "  cluster:     {}",
        diagnostics::safe_or(&cluster_name, "(unknown)")
    ));
    lines.push(format!(
        "  api server:  {}",
        diagnostics::safe_or(&server, "(unknown)")
    ));
    if let Some(cluster) = &cluster {
        lines.push(format!(
            "  k8s rev:     {}",
            diagnostics::safe_or(&cluster.server_version, "(unknown)")
        ));
        lines.push(format!(
            "  discovery:   {} resource kinds",
            cluster.catalog.len()
        ));
        for warning in cluster
            .discovery_fallback
            .iter()
            .chain(&cluster.discovery_warnings)
        {
            lines.push(format!("    • {}", diagnostics::safe(warning)));
        }
        lines.push(format!(
            "  metrics API: {}",
            if cluster.resolve("pods.metrics.k8s.io").is_some() {
                "discovered (metrics.k8s.io)"
            } else {
                "not installed"
            }
        ));
    }
    lines.push(format!(
        "  namespace:   {}",
        if namespace.is_empty() {
            "(all)"
        } else {
            &namespace
        }
    ));

    // Section order matches `:info` so the two reports can be read against
    // each other. A headless report has no session to count watches over, so
    // it runs one instead: the same watch a launch would open, which is the
    // failure this command exists to explain.
    lines.push(String::new());
    lines.push("Watch health".into());
    match &cluster {
        Some(cluster) => {
            lines.push(format!(
                "  probe:       {resource} in {}",
                if namespace.is_empty() {
                    "all namespaces"
                } else {
                    &namespace
                }
            ));
            let probe = cluster
                .probe_watch(&resource, &namespace, diagnostics::WATCH_PROBE_TIMEOUT)
                .await;
            lines.extend(diagnostics::watch_probe_lines(
                &probe,
                &resource,
                diagnostics::WATCH_PROBE_TIMEOUT,
            ));
        }
        None if info.offline => lines.push("  not probed (--offline)".into()),
        None => lines.push("  not probed — no connection".into()),
    }

    let latency = diagnostics::latency_lines();
    if !latency.is_empty() {
        lines.push(String::new());
        lines.extend(latency);
    }

    lines.push(String::new());
    lines.extend(diagnostics::config_source_lines(
        loader,
        &context,
        &cluster_name,
    ));

    lines.push(String::new());
    lines.push("Active config".into());
    lines.push(format!(
        "  skin:       {}",
        cfg.skin.name.as_deref().unwrap_or("auto")
    ));
    lines.push(format!("  readonly:   {}", cfg.readonly));
    lines.push(format!("  aliases:    {}", cfg.aliases.len()));
    lines.push(diagnostics::named_line(
        "plugins",
        cfg.plugins.iter().map(|p| p.name.as_str()),
        cfg.plugins.len(),
    ));
    lines.push(diagnostics::named_line(
        "views",
        cfg.views.keys().map(String::as_str),
        cfg.views.len(),
    ));
    lines.push(format!("  bookmarks:  {}", cfg.bookmarks.len()));
    lines.push(format!("  guardrails: {}", cfg.guardrails.len()));

    lines.push(String::new());
    lines.extend(diagnostics::logging_lines());

    lines.push(String::new());
    lines.extend(diagnostics::directory_lines());

    warnings.extend(
        config::plugin_warnings(&cfg.plugins)
            .into_iter()
            .chain(config::bookmark_warnings(&cfg.bookmarks))
            .chain(config::workspace_warnings(&cfg.workspaces))
            .chain(config::guardrail_warnings(&cfg.guardrails))
            .chain(config::forward_warnings(&cfg.forwards))
            .chain(config::notify_warnings(&cfg.notify)),
    );
    lines.push(String::new());
    lines.extend(diagnostics::warning_lines(warnings));

    for line in lines {
        println!("{}", diagnostics::safe(&line));
    }
    Ok(())
}

/// The namespace a launch with these flags would start in, resolved the same
/// way the TUI resolves it (flags, then remembered, then config, then
/// kubeconfig) — a report that disagreed with the app would be worse than
/// none. Empty means all namespaces.
fn starting_namespace(
    args: &Args,
    cfg: &config::Config,
    context: &str,
    kubeconfig_default: Option<String>,
) -> String {
    if args.all_namespaces {
        return String::new();
    }
    if let Some(ns) = &args.namespace {
        return ns.clone();
    }
    if let Some(ns) =
        nsmem::NamespaceMemory::load(&nsmem::NamespaceMemory::default_path()).get(context)
    {
        return ns;
    }
    if let Some(ns) = &cfg.default_namespace {
        return ns.clone();
    }
    // Kubernetes' own fallback when nothing pins one.
    kubeconfig_default
        .filter(|ns| !ns.is_empty())
        .unwrap_or_else(|| "default".into())
}

/// Feed keys to the app and redraw. Returns whether anything was dispatched,
/// so a repair that swallowed its input costs no frame.
fn dispatch(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    keys: Vec<crossterm::event::KeyEvent>,
    captured: bool,
) -> Result<bool> {
    if keys.is_empty() {
        return Ok(false);
    }
    for key in keys {
        app.handle_key(key)?;
        take_suspend(terminal, app, captured);
    }
    terminal.draw(|f| ui::draw(f, app))?;
    Ok(true)
}

/// Run whatever interactive command the app just queued, if any. Called after
/// every path that can queue one — a keystroke, a mouse click, and a background
/// message (the PVC browser resolves which pod to exec into asynchronously, so
/// its shell is requested from a message, not from the keystroke that asked
/// for it).
fn take_suspend(terminal: &mut ratatui::DefaultTerminal, app: &mut App, captured: bool) {
    if let Some(app::Suspend::Shell(argv)) = app.pending.take() {
        suspend_and_run(terminal, &argv, captured);
        app.flash = format!("ran: {}", argv.join(" "));
        app.flash_err = false;
        app.after_suspend();
        terminal_title::set(app.terminal_title().as_deref());
    }
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::Receiver<store::Msg>,
    mouse: bool,
) -> Result<()> {
    let mut reader = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // Watch messages mark the frame dirty and the redraw waits for this
    // interval, so a rollout storm costs at most ~60 renders a second instead
    // of one per message. Key events still redraw immediately for input
    // latency.
    let mut frame = tokio::time::interval(Duration::from_millis(16));
    frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = false;
    // Tracks whether mouse capture is currently on, so it can follow the mode:
    // document views release it for native text selection (see
    // `wants_mouse_capture`). Always false when the config disabled the mouse.
    let mut captured = mouse;
    // Reassembles cursor-key escape sequences split mid-read; only ever fed
    // while capture is released, which is the only time they can arrive.
    let mut repair = altscroll::Repair::default();

    let mut title = terminal_title::Title::default();
    terminal.draw(|f| ui::draw(f, app))?;
    loop {
        if app.should_quit {
            return Ok(());
        }

        title.update(app.terminal_title());

        if mouse && app.wants_mouse_capture() != captured {
            captured = !captured;
            if captured {
                let _ =
                    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
            } else {
                let _ =
                    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
            }
            // No more alternate-scroll sequences either way: release any Esc
            // still waiting for a tail that can no longer come.
            if dispatch(terminal, app, repair.flush(), captured)? {
                dirty = false;
            }
        }

        tokio::select! {
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        // With capture released the wheel reaches us as
                        // cursor-key escape sequences, and a fast burst can
                        // split one across reads; `altscroll` puts those back
                        // together. Nothing to repair while capture is on, so
                        // the key goes straight through.
                        let keys = if captured {
                            vec![key]
                        } else {
                            repair.push(key)
                        };
                        if dispatch(terminal, app, keys, captured)? {
                            dirty = false;
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => {
                        app.handle_mouse(m)?;
                        take_suspend(terminal, app, captured);
                        dirty = true;
                    }
                    Some(Err(_)) | None => return Ok(()),
                    Some(Ok(Event::Resize(_, _))) => {
                        ui::resize(terminal, app)?;
                        dirty = false;
                    }
                    _ => dirty = true,
                }
            }
            Some(msg) = rx.recv() => {
                app.handle_msg(msg);
                // Batch any other queued updates before the next redraw.
                while let Ok(m) = rx.try_recv() {
                    app.handle_msg(m);
                }
                if let Some(text) = app.take_notification() {
                    app.run_notify_command(&text);
                    ring_notification(&text, &app.notify_cfg);
                }
                take_suspend(terminal, app, captured);
                dirty = true;
            }
            _ = frame.tick(), if dirty || app.scrollbar_activity.is_some() => {
                let expired = app.expire_scrollbars();
                if dirty || expired {
                    terminal.draw(|f| ui::draw(f, app))?;
                    dirty = false;
                }
            }
            _ = tick.tick() => {
                app.reap_port_forwards(); // age columns + drop dead forwards
                app.expire_flash();
                app.check_journal_error();
                dirty = true;
            }
            // A held Esc was a real keypress after all, not the head of a
            // split escape sequence: act on it.
            _ = tokio::time::sleep(altscroll::Repair::TIMEOUT), if repair.pending() => {
                if dispatch(terminal, app, repair.flush(), captured)? {
                    dirty = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_picker_launch_validates_explicit_context() {
        for alias in ["ctx", "contexts"] {
            let args = Args::try_parse_from(["sofka", alias, "--context", "prod"]).unwrap();
            assert!(args.validate_picker_context(&["prod".into()]).is_ok());
            for contexts in [vec![], vec!["other".into()]] {
                let error = args.validate_picker_context(&contexts).unwrap_err();
                assert!(error.to_string().contains("context 'prod' not found"));
            }
            let args = Args::try_parse_from(["sofka", alias]).unwrap();
            assert!(args.validate_picker_context(&["other".into()]).is_ok());
        }
    }

    #[test]
    fn context_picker_launch_namespace_flags() {
        for alias in ["ctx", "contexts"] {
            for (flags, expected) in [
                (vec![], None),
                (vec!["--namespace", "payments"], Some("payments")),
                (vec!["--all-namespaces"], Some("")),
                (vec!["-n", "payments", "-A"], Some("")),
            ] {
                let args = Args::try_parse_from(["sofka", alias].into_iter().chain(flags)).unwrap();
                assert_eq!(args.launch_namespace().as_deref(), expected);
            }
        }
    }

    #[test]
    fn context_picker_launch_aliases_and_context_flag() {
        for alias in ["ctx", "contexts"] {
            let args = Args::try_parse_from(["sofka", alias, "--context", "prod"]).unwrap();
            assert!(args.context_picker().unwrap());
            assert_eq!(args.context.as_deref(), Some("prod"));
            for flag in ["--check", "--snapshot"] {
                let args = Args::try_parse_from(["sofka", alias, flag]).unwrap();
                assert!(args.context_picker().is_err());
            }
        }
        for argv in [
            vec!["sofka"],
            vec!["sofka", "dp"],
            vec!["sofka", "--context", "prod"],
        ] {
            assert!(
                !Args::try_parse_from(argv)
                    .unwrap()
                    .context_picker()
                    .unwrap()
            );
        }
    }

    #[test]
    fn tls_resumption_is_disabled_only_when_requested() {
        assert!(!Args::try_parse_from(["sofka"]).unwrap().no_tls_resumption);
        for mode in ["--check", "--snapshot", "pods"] {
            let args = Args::try_parse_from(["sofka", mode, "--no-tls-resumption"]).unwrap();
            assert!(args.no_tls_resumption);
            assert!(!args.allow_v1_client_cert);
        }
        let args = Args::try_parse_from([
            "sofka",
            "--no-tls-resumption",
            "--plugin-adapter",
            "sanitize",
        ])
        .unwrap();
        assert!(args.no_tls_resumption);
    }

    #[test]
    fn v1_client_cert_requires_an_explicit_cli_flag() {
        assert!(
            !Args::try_parse_from(["sofka"])
                .unwrap()
                .allow_v1_client_cert
        );
        for mode in ["--check", "--snapshot"] {
            let args = Args::try_parse_from(["sofka", mode, "--allow-v1-client-cert"]).unwrap();
            assert!(args.allow_v1_client_cert);
        }
        let args = Args::try_parse_from([
            "sofka",
            "--plugin-adapter",
            "sanitize",
            "--allow-v1-client-cert",
        ])
        .unwrap();
        assert!(args.allow_v1_client_cert);
    }
}
