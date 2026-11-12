mod app;
mod cli;
mod config;
mod exec;
mod ssh;
mod tui;
mod ui;

use std::process::ExitCode;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("dse: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(cli::run())
}
