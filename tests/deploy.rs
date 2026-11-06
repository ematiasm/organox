//! Deployment artifacts as contracts: `compose.prod.yaml` and its template.
//!
//! These are cheap structural tests, not a deployment. The rendered cases shell
//! out to `docker compose config`, which is the same path the operator runs, so
//! a value that never reaches the container fails here instead of in production.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Render `compose.prod.yaml` with a throwaway env file and return the JSON.
///
/// Returns `None` when the Docker CLI is unavailable: the structural tests below
/// still run, and a machine without Docker has no deployment to protect.
///
/// Two isolation measures, both load-bearing:
///
///  * the temp directory is created **beside** `compose.prod.yaml`, because
///    compose loads the `.env` sitting next to the file and would otherwise read
///    the developer's own configuration; and
///  * the inheritable environment is cleared, because `docker compose` lets the
///    process environment win over the values written here. Without that, a shell
///    that exports `APP_COOKIE_SECURE=true` makes the S17 assertion fail against
///    a correct compose file.
fn render(env_body: &str) -> Option<Result<serde_json::Value, String>> {
    let probe = Command::new("docker")
        .args(["compose", "version"])
        .output()
        .ok()?;
    if !probe.status.success() {
        return None;
    }

    // A directory per call, not per process: tests run in parallel and a shared
    // path would let one test read another's env file.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let repo = root();
    let dir = repo.join(format!(".compose-test-{}-{seq}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    // A panicking test leaves this behind; the sweep keeps the tree clean even
    // then, without relying on a Drop guard that could mask the failure.
    sweep_stale_test_dirs(&repo);
    // An empty `.env` next to the compose file: any project `.env` would be read
    // from here, so this is the file that neutralizes it.
    fs::write(dir.join(".env"), "").expect("write empty .env");
    let env_file = dir.join("deploy.env");
    fs::write(&env_file, env_body).expect("write env file");

    let out = Command::new("docker")
        .args(["compose", "-f"])
        .arg(repo.join("compose.prod.yaml"))
        .arg("--env-file")
        .arg(&env_file)
        .args(["config", "--format", "json"])
        .current_dir(&dir)
        .env_clear()
        .output()
        .expect("spawn docker compose");
    let _ = fs::remove_dir_all(&dir);

    Some(if out.status.success() {
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).map_err(|e| e.to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    })
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Remove scratch directories a previous run abandoned mid-test.
///
/// Runs once per process and only removes directories owned by a **different**
/// process. Both parts matter: tests run in parallel, and a sweep that removed
/// its own siblings deleted a live test's `.env` mid-render, which is exactly
/// the interference the per-call directory exists to prevent.
fn sweep_stale_test_dirs(repo: &std::path::Path) {
    use std::sync::Once;
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        let Ok(entries) = fs::read_dir(repo) else {
            return;
        };
        let own_pid = std::process::id().to_string();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // The exact shape this module creates: `.compose-test-<pid>-<seq>`,
            // both numeric. Anything else sharing the prefix belongs to the
            // developer and is never deleted.
            let Some(rest) = name.strip_prefix(".compose-test-") else {
                continue;
            };
            let Some((pid, seq)) = rest.split_once('-') else {
                continue;
            };
            let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
            if !numeric(pid) || !numeric(seq) || pid == own_pid {
                continue;
            }
            let _ = fs::remove_dir_all(entry.path());
        }
    });
}

/// `S14` — the server never migrates at startup. Only the `migrate` binary
/// embeds the migrations, so a restarting process cannot race itself.
#[test]
fn only_the_migrate_binary_embeds_migrations() {
    let migrate_bin = root().join("src/bin/migrate.rs");
    let mut offenders = Vec::new();

    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut sources = Vec::new();
    walk(&root().join("src"), &mut sources);

    for file in &sources {
        if *file == migrate_bin {
            continue;
        }
        if fs::read_to_string(file).unwrap().contains("sqlx::migrate!") {
            offenders.push(file.display().to_string());
        }
    }

    assert!(
        offenders.is_empty(),
        "only the migrate binary may embed migrations, found them in: {offenders:?}"
    );
    assert!(
        fs::read_to_string(&migrate_bin)
            .unwrap()
            .contains("sqlx::migrate!(\"./migrations\")"),
        "the image would ship no migrations"
    );
}

/// `S16` — every key the template documents must actually reach a container.
///
/// This is the failure that motivated the test: a documented knob that compose
/// never forwards is silently ignored, and the operator believes it took effect.
#[test]
fn every_documented_env_key_is_forwarded() {
    let template = fs::read_to_string(root().join(".env.prod.example")).unwrap();
    let compose = fs::read_to_string(root().join("compose.prod.yaml")).unwrap();

    let mut missing = Vec::new();
    for raw in template.lines() {
        // A commented-out key is still a documented option.
        let line = raw.trim();
        let line = line.strip_prefix('#').map(str::trim).unwrap_or(line);
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let is_key = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !is_key || key == "POSTGRES_PASSWORD" {
            continue; // wired explicitly and covered by the rendering test
        }
        if !compose.contains(key) {
            missing.push(key.to_string());
        }
    }

    assert!(
        missing.is_empty(),
        "documented in .env.prod.example but never forwarded by compose.prod.yaml: {missing:?}"
    );
}

/// `S2`, `S14`, `S16`, `S17` — the rendered contract.
///
/// One test, because each case needs a fresh `docker compose config` run over
/// the same file: the assertions are cheap and independent, the render is not.
#[test]
fn rendered_deployment_contract() {
    let Some(rendered) =
        render("POSTGRES_PASSWORD=Sup3r_Secret\nDATABASE_URL=postgres://stray@elsewhere/x\n")
    else {
        eprintln!("skipped: docker compose is not available");
        return;
    };
    let cfg = rendered.expect("compose must render with a password");

    // S16: one source. A stray DATABASE_URL in the environment cannot win.
    let derived = "postgres://organox:Sup3r_Secret@db:5432/organox";
    assert_eq!(
        cfg["services"]["db"]["environment"]["POSTGRES_PASSWORD"],
        "Sup3r_Secret"
    );
    for service in ["app", "migrate"] {
        assert_eq!(
            cfg["services"][service]["environment"]["DATABASE_URL"], derived,
            "{service} must derive DATABASE_URL from POSTGRES_PASSWORD"
        );
    }

    // S14: migrations are a one-shot step the server waits for.
    assert_eq!(
        cfg["services"]["migrate"]["entrypoint"],
        serde_json::json!(["migrate"])
    );
    assert_eq!(cfg["services"]["migrate"]["restart"], "no");
    assert_eq!(
        cfg["services"]["app"]["depends_on"]["migrate"]["condition"],
        "service_completed_successfully"
    );
    assert_eq!(
        cfg["services"]["app"]["depends_on"]["db"]["condition"],
        "service_healthy"
    );

    // S17: plain HTTP, so the cookie must not be marked Secure by default.
    assert_eq!(
        cfg["services"]["app"]["environment"]["APP_COOKIE_SECURE"],
        "false"
    );

    // S2: production cannot collide with the development stack.
    assert_eq!(cfg["name"], "organox-prod");
    assert_eq!(cfg["volumes"]["db_data"]["name"], "organox-prod_db_data");
    assert!(
        cfg["services"]["db"].get("ports").is_none(),
        "the database must not be published"
    );
    let published: Vec<String> = cfg["services"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|service| {
            service
                .get("ports")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|port| port["published"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(published, ["3000"], "only the app may publish a port");
}

/// `S18` — the first owner is created by an explicitly named service, never by
/// `up`, and it can read the password from a terminal.
///
/// The failure this prevents: a service that starts on every `up` would recreate
/// a user that was deleted, and an `environment` password would sit in
/// `docker inspect` in plaintext forever.
#[test]
fn bootstrap_is_profiled_and_carries_no_password() {
    let compose = fs::read_to_string(root().join("compose.prod.yaml")).unwrap();
    let block = compose
        .split("\n  bootstrap:")
        .nth(1)
        .expect("compose.prod.yaml must define a bootstrap service");
    let block = block.split("\n  app:").next().unwrap_or(block);
    // Comments explain the rules; only real configuration counts.
    let config_lines = || {
        block
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
    };

    assert!(
        config_lines().any(|line| line.starts_with("profiles:")),
        "bootstrap must sit under a profile, or `up -d` would run it"
    );
    assert!(
        config_lines().any(|line| line.contains("create-user")),
        "bootstrap must run the create-user binary"
    );
    assert!(
        !config_lines().any(|line| line.contains("--password-stdin")),
        "bootstrap must stay interactive so the password never reaches a shell history"
    );
    // `DATABASE_URL` interpolates the database password; anything else named
    // PASSWORD in this block would be a credential held in the container.
    let offending: Vec<&str> = config_lines()
        .filter(|line| line.contains("PASSWORD") && !line.contains("DATABASE_URL"))
        .collect();
    assert!(
        offending.is_empty(),
        "the bootstrap service must not carry a password in its environment: {offending:?}"
    );

    // The service only exists when the profile is named, which is what keeps it
    // out of `up -d`.
    let Some(rendered) = render("POSTGRES_PASSWORD=x\n") else {
        eprintln!("skipped: docker compose is not available");
        return;
    };
    let cfg = rendered.expect("compose must render with a password");
    let services = cfg["services"].as_object().expect("services");
    assert!(
        !services.contains_key("bootstrap"),
        "bootstrap must not be part of the default service set"
    );
    assert_eq!(
        cfg["services"]["app"]["depends_on"]
            .as_object()
            .map(|deps| deps.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["db".to_string(), "migrate".to_string()]),
        "app must not depend on bootstrap: it is a manual step"
    );
}

/// `S16` — a missing password refuses to render instead of starting a database
/// with an empty one.
#[test]
fn missing_postgres_password_fails_to_render() {
    for body in ["APP_COOKIE_SECURE=false\n", "POSTGRES_PASSWORD=\n"] {
        let Some(rendered) = render(body) else {
            eprintln!("skipped: docker compose is not available");
            return;
        };
        let err = rendered.expect_err("compose must not render without a password");
        assert!(
            err.contains("required variable POSTGRES_PASSWORD"),
            "the error must name the variable, got: {err}"
        );
    }
}
