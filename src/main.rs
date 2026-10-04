//! `genome` binary entry point.

/// fsqlite drives deeply nested async state machines; give the worker thread a
/// generous stack so debug builds don't overflow.
const STACK_SIZE: usize = 256 * 1024 * 1024;

fn main() {
    let code = std::thread::Builder::new()
        .name("genome".into())
        .stack_size(STACK_SIZE)
        .spawn(|| genome_cli::run(std::env::args_os()))
        .expect("spawn main thread")
        .join()
        .unwrap_or(1);
    std::process::exit(code);
}
