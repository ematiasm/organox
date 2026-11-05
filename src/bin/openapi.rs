//! Print the OpenAPI spec as JSON to stdout.
//!
//! Usage:
//!   cargo run --bin openapi > openapi.json
//!   npx openapi-typescript openapi.json -o frontend/src/lib/api/schema.d.ts

fn main() {
    let doc = organox::openapi::spec();
    match serde_json::to_string_pretty(&doc) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("error serializing the spec: {err}");
            std::process::exit(1);
        }
    }
}
