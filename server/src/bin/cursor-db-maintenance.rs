//! Runs migrations and one database cleanup against one database file without starting the server.
//!
//! Usage: `cursor-db-maintenance <database path>`
use cursor_server::{store::Store, Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: cursor-db-maintenance <database path>");
        std::process::exit(2);
    };
    let store = Store::connect(&format!("sqlite://{path}")).await?;
    println!("before: {} bytes", store.database_bytes().await?);
    let cleanup = store.clean_database().await?;
    println!(
        "after:  {} bytes (freed {})",
        cleanup.bytes, cleanup.freed_bytes
    );
    Ok(())
}
