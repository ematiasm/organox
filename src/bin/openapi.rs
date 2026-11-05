//! Imprime el spec OpenAPI como JSON a stdout.
//!
//! Uso:
//!   cargo run --bin openapi > openapi.json
//!   npx openapi-typescript openapi.json -o frontend/src/lib/api/schema.d.ts

fn main() {
    let doc = admin_app::openapi::spec();
    match serde_json::to_string_pretty(&doc) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("error serializando el spec: {err}");
            std::process::exit(1);
        }
    }
}
