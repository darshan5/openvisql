//! `openvisql-cli`: the command-line half of OpenViSQL as a console program.
//!
//! The main `openvisql` binary is a GUI program on Windows, so shells don't wait for it, its
//! output doesn't reach the console and exit codes get lost. This one is a normal console
//! program: cmd and PowerShell wait for it, print what it writes and see its exit code. It
//! takes the same arguments as `openvisql query`, with or without the word `query`.

// Only `query` is here; the GUI's half of the runtime (SSH tunnels) goes unused.
#[allow(dead_code)]
#[path = "runtime.rs"]
mod runtime;

#[path = "cli.rs"]
mod cli;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match args.first().map(String::as_str) {
        Some("query") => &args[1..],
        _ => &args[..],
    };
    std::process::exit(cli::main(args));
}
