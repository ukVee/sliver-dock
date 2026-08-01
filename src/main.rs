//! sliver-dock — Wayfire bottom-edge dock daemon.
//!
//! No args  → run as daemon (GTK main loop + layer-shell window + IPC server).
//! One verb → send the verb to the running daemon and exit.

mod app_grid;
mod apps;
mod battery;
mod config;
mod cpu;
mod drag;
mod ipc;
mod panel;
mod state;
mod wayfire;
mod zones;

use std::process::ExitCode;

const CLI_VERBS: &[&str] = &[
    "show",
    "hide",
    "toggle-detailed",
    "quit",
    "get-state",
    "drag-begin",
    "drag",
    "drag-end",
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        return panel::run_daemon();
    };
    if !CLI_VERBS.contains(&first.as_str()) {
        eprintln!("sliver-dock: unknown command: {first}");
        eprintln!("usage: sliver-dock [{}]", CLI_VERBS.join(" | "));
        return ExitCode::from(2);
    }
    let mut line = first;
    for arg in args {
        line.push(' ');
        line.push_str(&arg);
    }
    match ipc::send(&line) {
        Ok(reply) => {
            if !reply.is_empty() {
                print!("{reply}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sliver-dock: {e}");
            ExitCode::from(1)
        }
    }
}
