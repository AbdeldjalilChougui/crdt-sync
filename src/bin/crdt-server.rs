use clap::Parser;

/// Sync server: websocket endpoint per document at /sync/{doc}, JSON views at /docs.
#[derive(Parser)]
#[command(version)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:3030")]
    listen: String,
    /// sqlite database file.
    #[arg(long, default_value = "crdt-server.db")]
    db: std::path::PathBuf,
}

#[tokio::main]
async fn main() {
    let a = Args::parse();
    let store = crdt_sync::store::Store::open(&a.db).unwrap_or_else(|e| {
        eprintln!("error: opening {}: {e}", a.db.display());
        std::process::exit(1);
    });
    let listener = tokio::net::TcpListener::bind(&a.listen).await.unwrap_or_else(|e| {
        eprintln!("error: bind {}: {e}", a.listen);
        std::process::exit(1);
    });
    eprintln!("crdt-server on ws://{}/sync/<doc> (db {})", listener.local_addr().unwrap(), a.db.display());
    tokio::select! {
        r = crdt_sync::server::serve(listener, store) => if let Err(e) = r { eprintln!("error: {e}") },
        _ = tokio::signal::ctrl_c() => eprintln!("bye"),
    }
}
