//! Apply the migrations embedded in the binary, then exit.
//!
//! Usage:
//!   DATABASE_URL=postgres://... migrate
//!
//! Production has no `sqlx-cli` on the host. The image that serves the app also
//! ships this binary, and `compose.prod.yaml` runs it as a one-shot service that
//! the server waits for.
//!
//! Migrations are embedded at compile time, so the runtime image never needs the
//! `migrations/` directory.
//!
//! The server deliberately does not migrate on startup. A restarting process
//! that migrates races itself, and a fresh instance would apply migrations
//! concurrently with the old one. One guarded step, then the server.

use std::process::ExitCode;

use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL is not set, so there is no database to migrate")?;

    // One connection is all a migration run needs, and it makes the lock that
    // sqlx holds during the run unambiguous.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await?;

    let migrator = sqlx::migrate!("./migrations");
    migrator.run(&pool).await?;

    println!("migrations applied");
    Ok(())
}
