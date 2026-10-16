mod cli;
mod cli_more;
mod cli_op;
mod cmdline;
mod client;
mod config;
mod daemon;
mod dotenv;
mod fmt;
mod launchd;
mod op;
mod paths;
mod scrub;
mod setup;
mod tui;
mod types;

fn main() {
    // Rust ignores SIGPIPE by default; restore it so `lstack ls | head` exits quietly.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cli::main(args));
}
