//! `rkat-tui-mob` binary entry point (experimental): the workspace's mob
//! daemon, serving Meerkat's JSON-RPC methods over loopback TCP.

use clap::Parser;
use rkat_tui::{DaemonCli, run_daemon};
use std::process::ExitCode;

const BINARY_NAME: &str = "rkat-tui-mob";

fn main() -> ExitCode {
    // SAFETY: the first statement of main: no other thread exists yet and
    // nothing has used SQLite, as the switch requires.
    #[allow(unsafe_code)]
    let _ = unsafe { meerkat_sqlite::disable_memory_statistics_at_process_start() };
    let cli = DaemonCli::parse();
    match meerkat_runtime::host_stack::run_host(BINARY_NAME, || async move {
        match run_daemon(cli).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{BINARY_NAME}: {err}");
                ExitCode::FAILURE
            }
        }
    }) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{BINARY_NAME}: {err}");
            ExitCode::from(2)
        }
    }
}
