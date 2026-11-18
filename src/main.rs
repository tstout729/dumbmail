use std::io::{BufRead, Write};

use clap::{Parser, Subcommand};
use dumbmail::config::Config;
use dumbmail::{app, jobs, AppState, MIGRATOR};
use sqlx::postgres::PgPoolOptions;

#[derive(Parser)]
#[command(name = "dumbmail", version, about = "Open-source email and SMS marketing")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Parses only server settings, for running `dumbmail` with no subcommand.
#[derive(Parser)]
struct ServeOnly {
    #[command(flatten)]
    cfg: Config,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run migrations, the web server and the background workers (default).
    Serve(Box<Config>),
    /// Apply database migrations and exit.
    Migrate {
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
    },
    /// Operator: add (or with a negative amount, remove) prepaid credit for the account a user owns.
    AdminCredit {
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        /// Email of a user on the account
        #[arg(long)]
        email: String,
        /// Amount in USD
        #[arg(long, allow_hyphen_values = true)]
        amount: f64,
        #[arg(long, default_value = "Credit added by DumbMail")]
        note: String,
    },
    /// Bridge stdio MCP clients to a DumbMail server's HTTP MCP endpoint.
    McpStdio {
        /// Server base URL, e.g. https://dumbmail.ai
        #[arg(long, env = "DUMBMAIL_URL")]
        url: String,
        /// Private API key (pl_sk_...)
        #[arg(long, env = "DUMBMAIL_API_KEY", hide_env_values = true)]
        api_key: String,
    },
}

fn main() -> anyhow::Result<()> {
    // A sandboxed integration script run: no async runtime, no config, no database (see
    // `features::integrations::sandbox`). Checked before anything else starts.
    if std::env::args_os().nth(1).is_some_and(|a| a == dumbmail::features::integrations::sandbox::SUBCOMMAND) {
        std::process::exit(dumbmail::features::integrations::sandbox::child_main());
    }
    run()
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cmd = match cli.cmd {
        Some(c) => c,
        None => Cmd::Serve(Box::new(ServeOnly::parse_from(["dumbmail"]).cfg)),
    };
    match cmd {
        Cmd::Serve(cfg) => serve(*cfg).await,
        Cmd::Migrate { database_url } => {
            init_tracing();
            let db = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
            MIGRATOR.run(&db).await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Cmd::McpStdio { url, api_key } => mcp_stdio(&url, &api_key).await,
        Cmd::AdminCredit { database_url, email, amount, note } => {
            let db = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
            let account: Option<(uuid::Uuid, String)> = sqlx::query_as(
                "SELECT a.id, a.name FROM accounts a JOIN memberships m ON m.account_id = a.id JOIN users u ON u.id = m.user_id WHERE u.email = $1 ORDER BY m.created_at LIMIT 1",
            )
            .bind(email.trim().to_lowercase())
            .fetch_optional(&db)
            .await?;
            let (id, name) = account.ok_or_else(|| anyhow::anyhow!("no account for {email}"))?;
            dumbmail::billing::post_entry(&db, id, "adjustment", dumbmail::billing::to_micros(amount), &note, &uuid::Uuid::now_v7().to_string())
                .await
                .map_err(|e| anyhow::anyhow!(e.public_message()))?;
            let bal = dumbmail::billing::balance(&db, id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            println!("{name}: balance now ${:.4}", dumbmail::billing::to_usd(bal));
            Ok(())
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn,tower_http=info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

async fn serve(cfg: Config) -> anyhow::Result<()> {
    init_tracing();
    let db = PgPoolOptions::new().max_connections(20).connect(&cfg.database_url).await?;
    MIGRATOR.run(&db).await?;
    let local = ["http://localhost", "http://127.0.0.1", "http://0.0.0.0"].iter().any(|p| cfg.public_url.starts_with(p));
    let weak = cfg.secret_key == "dev-secret-change-me" || cfg.secret_key.len() < 32 || cfg.webhook_secret == "dev-webhook-secret" || cfg.webhook_secret.len() < 16;
    if weak && !local {
        anyhow::bail!("PUBLIC_URL is not local, so SECRET_KEY (32+ chars) and WEBHOOK_SECRET (16+ chars) must be set to random values (openssl rand -base64 48)");
    }
    if weak {
        tracing::warn!("using development secrets; set SECRET_KEY and WEBHOOK_SECRET before exposing this server");
    }
    let bind = cfg.bind.clone();
    let state = AppState::build(cfg, db).await?;
    tracing::info!(email = state.email.name(), sms = state.sms.name(), "providers ready");
    jobs::spawn_workers(state.clone());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("DumbMail listening on http://{bind}");
    axum::serve(listener, app(state)).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

/// Read newline-delimited JSON-RPC from stdin, forward each message to POST {url}/mcp, print replies.
async fn mcp_stdio(url: &str, api_key: &str) -> anyhow::Result<()> {
    let endpoint = format!("{}/mcp", url.trim_end_matches('/'));
    let http = reqwest::Client::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if !line.trim().is_empty() && tx.blocking_send(line).is_err() {
                break;
            }
        }
    });
    while let Some(line) = rx.recv().await {
        let res = http
            .post(&endpoint)
            .bearer_auth(api_key)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(line.clone())
            .send()
            .await;
        let out = match res {
            Ok(r) if r.status() == reqwest::StatusCode::ACCEPTED => None,
            Ok(r) => Some(r.text().await.unwrap_or_default()),
            Err(e) => {
                let id = serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v.get("id").cloned()).unwrap_or(serde_json::Value::Null);
                Some(serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": format!("could not reach DumbMail: {e}")}}).to_string())
            }
        };
        if let Some(o) = out.filter(|o| !o.trim().is_empty()) {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", o.replace('\n', ""))?;
            stdout.flush()?;
        }
    }
    Ok(())
}
