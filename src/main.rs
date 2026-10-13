mod cli;
mod cli_more;
mod cmdline;
mod client;
mod config;
mod daemon;
mod fmt;
mod launchd;
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
