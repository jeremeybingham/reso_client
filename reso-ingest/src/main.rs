//! The `reso-ingest` command line; see [`reso_ingest::cli`]

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code =
        reso_ingest::cli::run_main(&args, &mut std::io::stdout(), &mut std::io::stderr()).await;
    std::process::exit(code);
}
