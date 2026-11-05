//! Crea el primer usuario (o cualquier usuario) desde la terminal.
//!
//! Es el único mecanismo de alta de usuarios fuera de la API. No hay seed
//! automático al arrancar el servidor: un seed al bootear deja la contraseña en
//! texto plano en el `.env` para siempre, resucita usuarios borrados y pisa
//! contraseñas cambiadas.
//!
//! Uso:
//!   create-user --email yo@pyme.com --role owner
//!   printf '%s' "$PW" | create-user --email yo@pyme.com --role owner --password-stdin
//!
//! La contraseña nunca va por argumento: quedaría visible en `ps aux`.

use std::process::ExitCode;

use admin_app::auth::model::Role;
use admin_app::auth::{password, repository, validation};
use clap::Parser;
use sqlx::postgres::PgPoolOptions;

#[derive(Parser, Debug)]
#[command(
    name = "create-user",
    about = "Crea un usuario en el administrador",
    long_about = None
)]
struct Args {
    /// Email del usuario
    #[arg(long)]
    email: String,

    /// Rol: owner o encargado
    #[arg(long, value_parser = parse_role)]
    role: Role,

    /// Lee la contraseña de stdin en lugar de pedirla interactivamente.
    /// Obligatorio en Docker (`docker compose exec -T`).
    #[arg(long)]
    password_stdin: bool,

    /// URL de conexión. Si se omite, se usa DATABASE_URL del entorno.
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

    // Validar ANTES de conectarse: no tiene sentido abrir un pool para
    // descubrir que el email está mal escrito.
    let email = validation::ValidatedEmail::parse(&args.email)?;
    let plain = read_password(args.password_stdin)?;

    // Hash antes de conectar: es la parte cara y no depende de la DB.
    eprintln!("generando hash (argon2id, puede tardar un segundo)...");
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
            format!("el email {} ya está registrado", email.as_str())
        }
        other => other.to_string(),
    })?;

    println!(
        "usuario creado: {} ({}, rol {})",
        email.as_str(),
        id,
        args.role
    );
    Ok(())
}

/// Lee la contraseña de stdin o la pide sin eco por consola.
fn read_password(from_stdin: bool) -> Result<String, Box<dyn std::error::Error>> {
    let plain = if from_stdin {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        buf
    } else {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err(
                "stdin no es una terminal: usá --password-stdin para leer la contraseña de una tubería"
                    .into(),
            );
        }
        let first = rpassword::prompt_password("contraseña: ")?;
        let second = rpassword::prompt_password("repetir contraseña: ")?;
        if first != second {
            return Err("las contraseñas no coinciden".into());
        }
        first
    };

    // Recorta el salto de línea que agrega `echo`.
    Ok(plain.trim_end_matches(['\r', '\n']).to_string())
}
