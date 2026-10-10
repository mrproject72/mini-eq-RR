//! Mini EQ — main entry point.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use mini_eq_rr::core::default_bands;
use mini_eq_rr::dbus_control::MiniEqDBusControl;
use mini_eq_rr::pipewire_backend::PipeWireBackend;
use mini_eq_rr::remote_control::AppState;

#[derive(Parser)]
#[command(name = "mini-eq")]
#[command(about = "Compact PipeWire system-wide parametric equalizer")]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(long, global = true)]
    verbose: bool,

    #[arg(long, global = true)]
    background: bool,

    #[arg(long, global = true)]
    auto_route: bool,

    #[arg(long, global = true)]
    headless: bool,

    #[arg(long, global = true)]
    duration: Option<u64>,

    #[arg(long, global = true)]
    import_apo: Option<PathBuf>,

    #[arg(long, global = true)]
    check_deps: bool,

    #[arg(long, global = true)]
    output_sink: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    InstallDesktop,
    CheckDeps,
}

/// Shared application state for the D-Bus handler lives in
/// `mini_eq_rr::remote_control::AppState` — the window has to write into it on
/// every state mutation, so it cannot live in this binary crate.
// (see `AppState` there for the command queue that bridges the `Send` D-Bus
// vtable to the main-thread GTK objects)
fn main() {
    // Capture panics into the log before the default hook: the GTK event
    // closures abort the process on unwind, so without this a crash in a
    // draw/drag callback leaves no trace beyond a vanished process.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("PANIC: {info}");
        if let Some(loc) = info.location() {
            log::error!(
                "PANIC at {}:{}:{} (thread: {:?})",
                loc.file(),
                loc.line(),
                loc.column(),
                std::thread::current().name().unwrap_or("<unnamed>")
            );
        }
        default_hook(info);
    }));

    let cli = Cli::parse();

    // One-shot subcommands and `--check-deps` do not touch PipeWire, so they run
    // before the instance guard: they must keep working while the app is
    // already open.
    if let Some(command) = &cli.command {
        match command {
            Commands::InstallDesktop => {
                match mini_eq_rr::desktop_integration::install_desktop_integration() {
                    Ok(()) => println!("Installed desktop launcher and app icons."),
                    Err(e) => {
                        eprintln!("Failed to install desktop integration: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            Commands::CheckDeps => {
                print_dependency_report();
                return;
            }
        }
    }

    if cli.check_deps {
        print_dependency_report();
        return;
    }

    // Single-instance guard. Two instances would both try to own
    // `mini_eq_sink`, so the second must not start. The kernel drops the lock
    // if we die, so a crash never leaves the app unlaunchable.
    let _instance_guard = match mini_eq_rr::instance::InstanceGuard::try_acquire() {
        Ok(guard) => Some(guard),
        Err(mini_eq_rr::instance::AcquireError::AlreadyRunning(pid)) => {
            eprintln!(
                "mini-eq RR is already running{}. Use that window instead of starting a second one.",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            );
            std::process::exit(1);
        }
        Err(e) => {
            // A broken lock must not make the app unusable.
            eprintln!("Warning: {e}");
            None
        }
    };

    env_logger::init();

    if cli.headless {
        run_headless(cli.duration, cli.import_apo.as_deref());
        return;
    }

    // Launch GTK4 application
    launch_gui(cli.background, cli.auto_route, cli.output_sink);
}

fn print_dependency_report() {
    println!("Mini EQ dependency check");
    println!("  [OK] mini-eq v{}", env!("CARGO_PKG_VERSION"));
    println!("  [OK] GTK4 / Libadwaita (linked at build time)");

    let module = std::path::Path::new(
        "/usr/lib/x86_64-linux-gnu/pipewire-0.3/libpipewire-module-filter-chain.so",
    );
    let builtin = std::path::Path::new(
        "/usr/lib/x86_64-linux-gnu/spa-0.2/filter-graph/libspa-filter-graph-plugin-builtin.so",
    );
    println!(
        "  [{}] libpipewire-module-filter-chain",
        if module.exists() { "OK" } else { "MISSING" }
    );
    println!(
        "  [{}] libspa-filter-graph-plugin-builtin",
        if builtin.exists() { "OK" } else { "MISSING" }
    );

    let socket = std::env::var("XDG_RUNTIME_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join("pipewire-0"))
        .ok();
    let running = socket.as_deref().map(|p| p.exists()).unwrap_or(false);
    println!(
        "  [{}] PipeWire daemon socket",
        if running { "OK" } else { "MISSING" }
    );
}

fn run_headless(duration: Option<u64>, import_apo: Option<&std::path::Path>) {
    println!("Running headless (no GUI)");
    let bands = match import_apo {
        Some(path) => match mini_eq_rr::autoeq::parse_apo_file(path) {
            Ok((preamp, bands)) => {
                println!(
                    "Imported APO preset: {} band(s), preamp {:.1} dB",
                    bands.len(),
                    preamp
                );
                bands
            }
            Err(e) => {
                eprintln!("Failed to import APO preset: {}", e);
                std::process::exit(1);
            }
        },
        None => default_bands(),
    };
    let mut backend = match PipeWireBackend::new() {
        Ok(backend) => backend,
        Err(e) => {
            eprintln!("Failed to initialise PipeWire backend: {}", e);
            std::process::exit(1);
        }
    };

    let sink = backend
        .default_output_sink()
        .or_else(|| backend.list_output_sinks().first().map(|s| s.name.clone()));
    match sink {
        Some(sink) => match backend.ensure_device_chain(&sink, bands) {
            Ok(_) => println!("Filter-chain engine running -> {}", sink),
            Err(e) => eprintln!("Failed to create filter chain: {}", e),
        },
        None => eprintln!("Headless: no output sink detected"),
    }

    match duration {
        Some(secs) => {
            println!("Duration: {}s", secs);
            let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            while std::time::Instant::now() < end {
                backend.pump();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        None => {
            println!("Running until interrupted");
            loop {
                backend.pump();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

#[allow(clippy::collapsible_if)]
fn launch_gui(_background_mode: bool, auto_route: bool, output_sink: Option<String>) {
    let _ = adw::init();
    let app = adw::Application::new(
        Some("io.github.mrproject72.mini_eq_rr"),
        adw::gio::ApplicationFlags::empty(),
    );

    let app_state = AppState::new();

    // Initialize the PipeWire backend. It is shared with the window so UI edits
    // can be pushed to the filter-chain engine. It stays on the GTK thread —
    // the window's update loop pumps the PipeWire main loop via `pump()`.
    let shared_backend: Rc<RefCell<Option<PipeWireBackend>>> = Rc::new(RefCell::new(None));
    let mut engine_sink = String::new();
    {
        match PipeWireBackend::new() {
            Ok(mut backend) => {
                // The `default.audio.sink` metadata property typically arrives
                // just AFTER the bind roundtrip completes (observed: bind
                // replays None, the real value lands a tick later), so a
                // single read here usually misses it and the engine would
                // never start. Wait briefly for it, then fall back to the
                // first listed sink rather than giving up with no EQ at all.
                let mut sink = output_sink
                    .clone()
                    .or_else(|| backend.default_output_sink());
                if sink.is_none() {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    while sink.is_none() && std::time::Instant::now() < deadline {
                        backend.pump();
                        sink = backend.default_output_sink();
                        if sink.is_none() {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                    }
                }
                let sink =
                    sink.or_else(|| backend.list_output_sinks().first().map(|s| s.name.clone()));
                match sink {
                    Some(sink) => {
                        engine_sink = sink.clone();
                        // Honor the device's configured preset immediately, the
                        // same rule the per-device ON handler uses at EQ start.
                        let (bands, preamp) = match mini_eq_rr::core::output_preset_for_sink(&sink)
                        {
                            Some(name) => mini_eq_rr::core::load_preset_from_file(
                                &mini_eq_rr::core::preset_path_for_name(&name),
                            )
                            .map(|(p, b)| (b, p))
                            .unwrap_or_else(|_| (mini_eq_rr::core::default_bands(), 0.0)),
                            None => (mini_eq_rr::core::default_bands(), 0.0),
                        };
                        match backend.ensure_device_chain(&sink, bands) {
                            Ok(_) => {
                                log::info!("Filter-chain engine running -> {}", sink);
                                backend.set_current_sink(&sink);
                                backend.set_device_preamp(&sink, preamp);
                            }
                            Err(e) => log::warn!("Failed to load filter chain: {}", e),
                        }
                        if auto_route {
                            let eq = mini_eq_rr::core::eq_virtual_sink_for(&sink);
                            match backend.auto_route_to_sink(&eq) {
                                Ok(()) => {
                                    backend.set_device_eq_enabled(&sink, true);
                                    backend.set_selected_sink(&sink);
                                }
                                Err(e) => log::warn!("Failed to auto-route: {}", e),
                            }
                        }
                    }
                    None => log::warn!("No output sink detected; engine not started"),
                }
                *shared_backend.borrow_mut() = Some(backend);
            }
            Err(e) => log::warn!("Failed to connect to PipeWire: {}", e),
        }
    }

    // Register D-Bus control.
    // `output_sink` is resolved to the real engine sink above; publish it so
    // the first GetState already reports it instead of an empty string.
    if !engine_sink.is_empty() {
        *app_state.output_sink.lock().unwrap() = Some(engine_sink.clone());
    }
    // Catch SIGTERM/SIGINT so a `kill` or a session-manager stop still runs the
    // routing restore. The update loop turns the flag into a clean quit.
    mini_eq_rr::exit_guard::install_signal_handlers();

    let dbus_control = MiniEqDBusControl::new(app_state.clone());
    if let Err(e) = dbus_control.register() {
        log::warn!("Failed to register D-Bus control: {}", e);
    } else if let Some(conn) = dbus_control
        .connection_handle()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        // Hand the connection to AppState so the window can emit
        // StateChanged / AnalyzerLevelsChanged on UI-initiated changes.
        app_state.set_connection(conn);
    }

    let backend_for_activate = shared_backend.clone();
    let sink_for_activate = engine_sink.clone();
    let state_for_activate = app_state.clone();
    app.connect_activate(move |app| {
        let window = mini_eq_rr::window::MiniEqWindow::new(
            app,
            backend_for_activate.clone(),
            sink_for_activate.clone(),
            state_for_activate.clone(),
        );
        window.present();
    });

    // Clean up PipeWire routing when the app exits.
    //
    // The window's close-request handler covers the ordinary case, but the
    // application `shutdown` signal also fires on last-window-closed and any
    // other path that ends the main loop. This matters because the stream
    // targets live in PipeWire's shared `default` metadata: if the app dies
    // with playback streams still pointed at mini_eq_sink, those values
    // outlive the app, the sink gets destroyed with the filter-chain, and
    // every player goes silent -- the "audio stops when I close the app"
    // bug. Unrouting hands the streams back to the real default output.
    //
    // Idempotent with the window handler, so running both is harmless.
    {
        let backend_shutdown = shared_backend.clone();
        let sink_shutdown = engine_sink.clone();
        let restore = move |why: &str| {
            let sink = sink_shutdown.clone();
            if let Some(be) = backend_shutdown.borrow_mut().as_mut() {
                log::info!("{why}: handing playback streams back to the real output");
                if let Err(e) = be.restore_routing_on_exit(Some(&sink)) {
                    log::warn!("{why}: routing restore failed: {e}");
                }
                if be.monitor_enabled() {
                    be.stop_monitor();
                }
            }
        };

        app.connect_shutdown({
            let restore = restore.clone();
            move |_app| restore("app shutdown")
        });
    }

    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let _ = app.run_with_args_os(&args);
}
