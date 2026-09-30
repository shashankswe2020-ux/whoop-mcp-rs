fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Fatal error: {error}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = runtime.block_on(whoop_mcp::app::run_cli(args));
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::exit(code);
}
