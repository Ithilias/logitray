// Build as a GUI-subsystem app so launching the tray doesn't pop a console
// window. The CLI modes (--once/--diag) re-attach to the parent console below
// so their stdout still reaches the terminal.
#![windows_subsystem = "windows"]

use logitray::app;

/// Re-attach stdout/stderr to the launching terminal's console (if any) so the
/// CLI debug modes remain usable despite the "windows" subsystem. No-op when
/// launched without a parent console (e.g. from Explorer / autostart).
#[cfg(windows)]
fn attach_parent_console() {
    // kernel32 is always linked on Windows; declare the one call we need.
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF; // (DWORD)-1
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    #[cfg(windows)]
    if args.iter().any(|a| a == "--once" || a == "--diag") {
        attach_parent_console();
    }

    let result = if args.iter().any(|a| a == "--diag") {
        logitray::hid::diag::run_diag();
        Ok(())
    } else if args.iter().any(|a| a == "--once") {
        app::run_once()
    } else {
        app::run_tray()
    };

    if let Err(err) = result {
        eprintln!("logitray error: {err:#}");
        std::process::exit(1);
    }
}
