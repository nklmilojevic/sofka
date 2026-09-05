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
    altscroll, app, config, diagnostics, fleet, k8s, nsmem, providers, snapshot, sortmem, store,
    theme, thresholds, ui, views,
};

const EVENT_CHANNEL_CAP: usize = 4096;

/// Messages one drain pass may handle before yielding to input and rendering.
/// Large enough that an ordinary burst still costs a single frame, small
/// enough that a sustained producer cannot starve the event loop.
const MSG_DRAIN_BUDGET: usize = 256;

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

    /// Kubeconfig context to start in (defaults to the current context).
    #[arg(long)]
    context: Option<String>,

    /// Path to the kubeconfig file (sets $KUBECONFIG for the whole session,
    /// including kubectl shell-outs).
    #[arg(long, value_name = "PATH")]
    kubeconfig: Option<PathBuf>,

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

    /// Print version/build, config sources, directories, and the current
    /// kubeconfig context, then exit (no cluster connection). For live
    /// discovery/metrics/watch status, use `:info` inside the TUI.
    #[arg(long)]
    info: bool,
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

async fn run_main(args: Args) -> Result<()> {
    let (loader, mut config_warnings) = config::ConfigLoader::load();

    // `--info`: print static diagnostics (no cluster connection) and exit.
    if args.info {
        print_info(&loader, &config_warnings);
        return Ok(());
    }

    // Connect before taking over the terminal so errors are readable. An
    // unreachable current context isn't fatal for the interactive TUI: start
    // in the context picker instead (k9s behavior). Headless modes still exit
    // with the error, since there is no picker to fall back to.
    eprintln!("Connecting to cluster…");
    let connect = match args.context.as_deref() {
        Some(name) => Cluster::connect_context(name).await,
        None => Cluster::connect().await,
    };
    let (mut cluster, connect_error) = match connect {
        Ok(c) => (c, None),
        Err(e) if args.check || args.snapshot => {
            eprintln!("\x1b[31merror:\x1b[0m {e:#}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("\x1b[33mwarning:\x1b[0m {e:#}");
            (
                Cluster::disconnected(args.context.as_deref()),
                Some(format!("{e:#}")),
            )
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
    let panic_tx = tx.clone();
    let mut app = App::new(cluster, tx);
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
    app.logs_cfg = cfg.logs.clone();
    // Seed the session toggle once; later `F` presses (and per-context config
    // reloads) don't fight the user's in-session choice.
    app.logs.fullscreen = cfg.logs.fullscreen;
    app.fleet_cfg = cfg.fleet.clone();
    app.forwards_cfg = cfg.forwards.clone();
    app.notify_cfg = cfg.notify.clone();
    for w in config::plugin_warnings(&app.plugins)
        .into_iter()
        .chain(config::bookmark_warnings(&app.bookmarks))
        .chain(config::workspace_warnings(&app.workspaces))
        .chain(config::guardrail_warnings(&app.guardrails))
        .chain(config::forward_warnings(&app.forwards_cfg))
        .chain(config::notify_warnings(&app.notify_cfg))
    {
        eprintln!("warning: {w}");
        config_warnings.push(w);
    }
    let (palette_keys, key_warnings) = config::compile_palette_keys(&cfg.keys);
    for w in key_warnings {
        eprintln!("warning: {w}");
        config_warnings.push(w);
    }
    app.palette_keys = palette_keys;
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
    // CLI flags win; then the namespace remembered for this context from the
    // last session; then the config default; then the kubeconfig's.
    if args.all_namespaces {
        app.namespace = String::new();
    } else if let Some(ns) = args.namespace {
        app.namespace = ns;
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
        Some(err) => app.start_disconnected(err),
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

    if args.snapshot {
        return snapshot(&mut app, &mut rx).await;
    }

    let mouse = cfg.mouse.unwrap_or(true);
    let mut terminal = ratatui::init();
    if mouse {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    }
    install_panic_hook(panic_tx);
    let result = run(&mut terminal, &mut app, &mut rx, mouse).await;
    // Disable before leaving the alternate screen so the shell never sees
    // mouse-report sequences (harmless if capture was never enabled).
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    // `restore()` leaves the alternate screen but never re-shows the cursor
    // that `draw` hid, so without this the user's shell prompt has no cursor.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
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

    // Wait for the frame's sources rather than for the clock: a `--snapshot`
    // that has its initial list (and its metrics, and its dashboard) has
    // nothing left to render, and sitting out the rest of the window is time
    // every CI run and every local smoke check pays for nothing. The deadline
    // stays as the fallback for a cluster that never syncs; a quiet window
    // before finishing keeps a burst of watch events from cutting the wait
    // short mid-list.
    const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(3);
    const SNAPSHOT_QUIET: Duration = Duration::from_millis(250);
    let deadline = tokio::time::Instant::now() + SNAPSHOT_DEADLINE;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout(SNAPSHOT_QUIET.min(deadline - now), rx.recv()).await {
            Ok(Some(msg)) => app.handle_msg(msg),
            Ok(None) => break,
            // A whole quiet window with nothing arriving: stop if what the
            // frame shows has landed, otherwise keep waiting for it.
            Err(_) if snapshot_ready(app) => break,
            Err(_) => {}
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

/// Whether a headless snapshot has everything it is going to draw: the watch's
/// initial list, the metrics the CPU/MEM columns show (pods/nodes only), and
/// the dashboard `PUP_DEMO` asked for. Deliberately narrow — anything not
/// rendered by the frame must not hold the snapshot open.
fn snapshot_ready(app: &App) -> bool {
    if !app.store.synced {
        return false;
    }
    if app.metrics_columns() && app.metrics.is_empty() {
        return false;
    }
    match app.mode {
        app::Mode::Pulse => app.pulse.nodes_total + app.pulse.pods_total > 0,
        app::Mode::Xray => !app.xray_items.is_empty(),
        _ => true,
    }
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

/// `--info`: print static runtime diagnostics (no cluster connection). Emits
/// only identifiers and paths — never credentials, tokens, or Secret values.
fn print_info(loader: &config::ConfigLoader, warnings: &[String]) {
    println!("sofka v{}", diagnostics::VERSION);
    println!("  build: {}", diagnostics::build_line());

    let (context, cluster, server) = k8s::current_context_info()
        .unwrap_or_else(|| ("(none)".into(), String::new(), String::new()));
    println!();
    println!("Kubeconfig");
    println!("  current context: {context}");
    println!(
        "  cluster:         {}",
        if cluster.is_empty() {
            "(unknown)"
        } else {
            &cluster
        }
    );
    println!(
        "  api server:      {}",
        if server.is_empty() {
            "(unknown)"
        } else {
            &server
        }
    );

    println!();
    println!("Config sources");
    match loader.base_path() {
        Some(path) => {
            let state = if loader.has_base() {
                "loaded"
            } else if path.exists() {
                "invalid — using defaults"
            } else {
                "absent — using defaults"
            };
            println!("  {} ({state})", path.display());
        }
        None => println!("  no config directory — using defaults"),
    }
    for path in loader.override_paths(&context, &cluster) {
        println!("  {} ({})", path.display(), config::file_state(&path));
    }

    println!();
    println!("Directories");
    println!("  state:     {}", diagnostics::state_dir().display());
    println!("  snapshots: {}", snapshot::snapshots_dir().display());
    println!("  bundles:   {}", std::env::temp_dir().display());

    if warnings.is_empty() {
        println!("\nNo config validation warnings.");
    } else {
        println!("\nWarnings [{}]", warnings.len());
        for w in warnings {
            println!("  • {w}");
        }
    }
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
        if let Some(app::Suspend::Shell(argv)) = app.pending.take() {
            suspend_and_run(terminal, &argv, captured);
            app.flash = format!("ran: {}", argv.join(" "));
            app.flash_err = false;
        }
    }
    terminal.draw(|f| ui::draw(f, app))?;
    Ok(true)
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

    terminal.draw(|f| ui::draw(f, app))?;
    loop {
        if app.should_quit {
            return Ok(());
        }

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
                        if let Some(app::Suspend::Shell(argv)) = app.pending.take() {
                            suspend_and_run(terminal, &argv, captured);
                            app.flash = format!("ran: {}", argv.join(" "));
                            app.flash_err = false;
                        }
                        dirty = true;
                    }
                    Some(Err(_)) | None => return Ok(()),
                    // Resize (and any other terminal event) still needs a
                    // redraw, just not an urgent one.
                    _ => dirty = true,
                }
            }
            Some(msg) = rx.recv() => {
                app.handle_msg(msg);
                // Batch any other queued updates before the next redraw — but
                // only a bounded number of them. Producers refill the channel
                // while it drains, so "until empty" is not a bound at all
                // under a watch or log storm: the loop can stay in this branch
                // indefinitely, and every message it handles there is one the
                // key reader and the frame timer do not get. What is left over
                // stays queued, in order, for the next pass.
                let mut budget = MSG_DRAIN_BUDGET;
                while budget > 0 {
                    let Ok(m) = rx.try_recv() else { break };
                    app.handle_msg(m);
                    budget -= 1;
                }
                if let Some(text) = app.take_notification() {
                    app.run_notify_command(&text);
                    ring_notification(&text, &app.notify_cfg);
                }
                dirty = true;
            }
            _ = frame.tick(), if dirty => {
                terminal.draw(|f| ui::draw(f, app))?;
                dirty = false;
            }
            _ = tick.tick() => {
                // age columns + drop dead forwards
                let changed = app.reap_port_forwards() | app.expire_flash();
                // A document view has nothing that moves with the clock, so a
                // tick there is not a reason to redraw it — only an actual
                // change is. Every other view can be showing an elapsed time.
                dirty = dirty || changed || !app.static_between_events();
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
