//! Create the first user (or any user) from the terminal.
//!
//! This is the only way to create users outside the API. There is no automatic
//! seed on server startup: seeding at boot leaves the password in plaintext in
//! `.env` forever, resurrects deleted users, and overwrites changed passwords.
//!
//! Usage:
//!   create-user --email yo@pyme.com --role owner
//!   printf '%s' "$PW" | create-user --email yo@pyme.com --role owner --password-stdin
//!
//! The password is never passed as an argument: it would be visible in `ps aux`.

use std::process::ExitCode;

use clap::Parser;
use organox::auth::model::Role;
use organox::auth::{password, repository, validation};
use sqlx::postgres::PgPoolOptions;

#[derive(Parser, Debug)]
#[command(
    name = "create-user",
    about = "Create a user in the administrator",
    long_about = None
)]
struct Args {
    /// User email
    #[arg(long)]
    email: String,

    /// Role: owner or manager
    #[arg(long, value_parser = parse_role)]
    role: Role,

    /// Read the password from stdin instead of prompting interactively.
    /// Required in Docker (`docker compose exec -T`).
    #[arg(long)]
    password_stdin: bool,

    /// Connection URL. If omitted, DATABASE_URL from the environment is used.
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
}

fn parse_role(raw: &str) -> Result<Role, String> {
    raw.parse::<Role>()
}

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
    let args = Args::parse();

    // Validate BEFORE connecting: there is no point opening a pool to
    // discover that the email is malformed.
    let email = validation::ValidatedEmail::parse(&args.email)?;
    let plain = read_password(args.password_stdin)?;

    // Hash before connecting: this is the expensive part and does not depend on the DB.
    eprintln!("generating hash (argon2id, this may take a second)...");
    let hash = password::hash(&plain)?;
    drop(plain);

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&args.database_url)
        .await?;

    let id = repository::create_user(
        &pool,
        repository::NewUser {
            email: &email,
            password_hash: &hash,
            role: args.role,
        },
    )
    .await
    .map_err(|err| match err {
        repository::RepoError::EmailTaken => {
            format!("email {} is already registered", email.as_str())
        }
        other => other.to_string(),
    })?;

    println!(
        "user created: {} ({}, role {})",
        email.as_str(),
        id,
        args.role
    );
    Ok(())
}

/// Read the password from stdin or prompt on the console without echo.
fn read_password(from_stdin: bool) -> Result<String, Box<dyn std::error::Error>> {
    let plain = if from_stdin {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        buf
    } else {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err(
                "stdin is not a terminal: use --password-stdin to read the password from a pipe"
                    .into(),
            );
        }
        let first = rpassword::prompt_password("password: ")?;
        let second = rpassword::prompt_password("repeat password: ")?;
        if first != second {
            return Err("passwords do not match".into());
        }
        first
    };

    // Trim the newline added by `echo`.
    Ok(plain.trim_end_matches(['\r', '\n']).to_string())
}
