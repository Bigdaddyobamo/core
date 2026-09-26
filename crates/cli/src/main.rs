use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use futures::future::join_all;
use reqwest::{Client, StatusCode};
use tokio::sync::watch;
use tracing::{info, warn};
use txwatch_config::AppConfig;
use txwatch_notifier::{build_client, send_webhook_simple, test_payload_with_network};

// ── CLI definition ────────────────────────────────────────────────────────────

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("TXWATCH_GIT_SHA"),
    " built ",
    env!("TXWATCH_BUILD_TIMESTAMP"),
    ")"
);

#[derive(Parser)]
#[command(
    name    = "txwatch",
    version = VERSION,
    about   = "Stellar Soroban contract monitor & webhook alert engine"
)]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "config/example.toml")]
    config: Option<PathBuf>,

    /// Log output format: human-readable text or one JSON object per line
    #[arg(long, global = true, value_enum, env = "TXWATCH_LOG_FORMAT", default_value = "text")]
    log_format: LogFormat,

    /// Override the Horizon base URL for every contract (e.g. a private Horizon instance)
    #[arg(long, global = true)]
    horizon_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Start the polling engine (watches all contracts in the config)
    ///
    /// With --once, exit codes: 0 = every poll and webhook delivery succeeded, 1 = otherwise
    Watch {
        /// Do not actually send webhooks; only log matched rules
        #[arg(long)]
        dry_run: bool,

        /// Run a single poll cycle, deliver alerts, save cursors and exit
        #[arg(long)]
        once: bool,
    },

    /// Parse and validate the config file, then print a summary
    ///
    /// Exit codes: 0 = valid config, 1 = invalid or missing config
    Validate {
        /// Send a HEAD/OPTIONS request to each webhook URL and warn on unreachable endpoints.
        #[arg(long)]
        check_webhooks: bool,

        /// Verify that each contract exists on its configured Horizon network.
        #[arg(long)]
        check_horizon: bool,

        /// Output format. `json` prints the parsed config (secrets redacted) or the
        /// validation error as a single JSON object on stdout.
        #[arg(long, value_enum, default_value = "text", conflicts_with_all = ["check_webhooks", "check_horizon"])]
        format: OutputFormat,
    },

    /// Send a test webhook payload to a URL and exit
    ///
    /// Exit codes: 0 = webhook delivered, 1 = delivery failed (unreachable or HTTP error)
    TestWebhook {
        /// The webhook URL to POST to
        #[arg(long)]
        url: Option<String>,

        /// Label to include in the test payload
        #[arg(long, default_value = "TxWatch Test")]
        label: String,

        #[arg(long, default_value = "testnet")]
        network: String,

        #[arg(long)]
        contract: Option<String>,

        #[arg(long)]
        secret: Option<String>,
    },

    /// Print the JSON Schema for the TOML configuration file.
    Schema,

    /// Evaluate a contract's rules against one historical transaction
    ///
    /// Prints the rules that matched and their webhook payloads. Nothing is sent
    /// unless --send is given. Exit codes: 0 = done, 1 = lookup or delivery failed
    Replay {
        /// Label of the configured contract whose rules to evaluate
        #[arg(long)]
        contract: String,

        /// Transaction hash to replay
        #[arg(long)]
        tx: String,

        /// Also deliver the resulting webhooks to the contract's webhook_url
        #[arg(long)]
        send: bool,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_format);

    match cli.command {
        Command::Validate { format: OutputFormat::Json, .. } => {
            match AppConfig::from_file(&required_config(&cli)?) {
                Ok(cfg) => println!("{}", serde_json::to_string_pretty(&config_summary_json(&cfg))?),
                Err(e) => {
                    let error = serde_json::json!({ "valid": false, "error": format!("{:#}", e) });
                    println!("{}", serde_json::to_string_pretty(&error)?);
                    std::process::exit(1);
                }
            }
        }

        Command::Validate { check_webhooks, check_horizon, .. } => {
            let cfg = AppConfig::from_file(&required_config(&cli)?)?;
            println!("Config is valid.");
            println!("  poll_interval_seconds : {}", cfg.poll_interval_seconds);
            println!("  contracts             : {}", cfg.contracts.len());
            println!();
            for c in &cfg.contracts {
                println!(
                    "  [{network}] {label}",
                    network = c.network.display_name(),
                    label = c.label
                );
                println!("    contract_id  : {}", c.contract_id);
                println!("    webhook_url  : {}", c.webhook_url);
                println!(
                    "    secret       : {}",
                    if c.webhook_secret.is_some() {
                        "set"
                    } else {
                        "none"
                    }
                );
                println!(
                    "    interval     : {}s{}",
                    c.effective_poll_interval(cfg.poll_interval_seconds),
                    if c.poll_interval_seconds.is_some() {
                        " (override)"
                    } else {
                        ""
                    }
                );
                println!("    rules        : {}", c.rules.len());
                for rule in &c.rules {
                    println!("      - {}", rule.label());
                }
                println!("    horizon      : {}", c.network.horizon_base_url());
                println!(
                    "    explorer     : {}/contract/{}",
                    c.network.explorer_base_url(),
                    c.contract_id
                );
            }

            if check_webhooks {
                let client = Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .context("failed to build HTTP client")?;
                let checks = join_all(cfg.contracts.iter().map(|c| async {
                    (c.label.clone(), c.webhook_url.clone(), check_webhook_reachable(&client, &c.webhook_url).await)
                })).await;
                let mut failed = false;
                println!("Webhook checks:");
                for (label, url, result) in checks {
                    match result {
                        Ok(status) => {
                            if status == "method not allowed" { failed = true; }
                            println!("  {:<18} {:<20} {}", label, status, url)
                        }
                        Err(error) => { failed = true; println!("  {:<18} {:<20} {} ({})", label, "unreachable", url, error); }
                    }
                }
                if failed { return Err(anyhow::anyhow!("one or more webhook checks failed")); }
            }
            if check_horizon {
                let client = Client::builder().timeout(Duration::from_secs(5)).build().context("failed to build HTTP client")?;
                let checks = join_all(cfg.contracts.iter().map(|c| check_horizon_contract(&client, c))).await;
                let mut failed = false;
                println!("Horizon checks:");
                for check in checks {
                    println!("  {:<10} {:<12} latest ledger: {} — {}", check.network, check.status, check.latest_ledger.map_or_else(|| "unknown".into(), |n| n.to_string()), check.message);
                    failed |= !check.reachable || !check.found;
                }
                if failed { return Err(anyhow::anyhow!("one or more Horizon checks failed")); }
            }
        }

        Command::TestWebhook { url, label, network, contract, secret } => {
            let configured = contract.as_ref().map(|wanted| {
                let path = cli.config.as_ref().ok_or_else(|| anyhow::anyhow!("--contract requires --config"))?;
                let cfg = AppConfig::from_file(path)?;
                cfg.contracts.into_iter().find(|c| c.label == *wanted).ok_or_else(|| anyhow::anyhow!("configured contract '{}' not found", wanted))
            }).transpose()?;
            let (url, network_name, horizon_base_url, secret) = if let Some(c) = configured {
                (c.webhook_url, c.network.as_str().to_owned(), c.network.horizon_base_url().to_owned(), secret.or(c.webhook_secret))
            } else {
                let selected = match network.as_str() { "mainnet" => txwatch_config::Network::Mainnet, "testnet" => txwatch_config::Network::Testnet, "futurenet" => txwatch_config::Network::Futurenet, other => return Err(anyhow::anyhow!("unknown network '{}'", other)) };
                (url.ok_or_else(|| anyhow::anyhow!("--url is required unless --contract is provided"))?, network, selected.horizon_base_url().to_owned(), secret)
            };
            let payload = test_payload_with_network(&label, &url, &network_name, &horizon_base_url);
            let client = build_client().context("failed to build HTTP client")?;

            info!(url = %url, "sending test webhook");
            let result = send_webhook_simple(&client, &url, &payload, secret.as_deref())
                .await
                .with_context(|| format!("test webhook to '{}' failed", url))?;
            println!("Test webhook delivered successfully to {} (status {}, attempts {})", url, result.final_status, result.attempts);
        }

        Command::Replay { ref contract, ref tx, send } => {
            let cfg = load_config(&cli)?;
            let contract = cfg
                .contracts
                .into_iter()
                .find(|c| c.label == *contract)
                .ok_or_else(|| anyhow::anyhow!("configured contract '{}' not found", contract))?;
            let client = build_client().context("failed to build HTTP client")?;

            let payloads = txwatch_poller::replay_transaction(&client, &contract, tx).await?;
            println!("{} rule(s) matched transaction {} for '{}'", payloads.len(), tx, contract.label);
            for payload in &payloads {
                println!();
                println!("  rule: {}", payload.rule_triggered);
                println!("{}", serde_json::to_string_pretty(payload)?);
            }

            if send {
                for payload in &payloads {
                    let result = send_webhook_simple(&client, &contract.webhook_url, payload, contract.webhook_secret.as_deref())
                        .await
                        .with_context(|| format!("webhook for rule '{}' to '{}' failed", payload.rule_triggered, contract.webhook_url))?;
                    println!("Delivered '{}' to {} (status {})", payload.rule_triggered, contract.webhook_url, result.final_status);
                }
            }
        }

        Command::Schema => println!("{}", serde_json::to_string_pretty(&schemars::schema_for!(txwatch_config::AppConfig))?),

        Command::Watch { dry_run, once } => {
            let cfg = load_config(&cli)?;

            if once {
                info!(version = VERSION, contracts = cfg.contracts.len(), dry_run, "running a single TxWatch poll cycle");
                let report = txwatch_poller::run_once(cfg, dry_run).await?;
                info!(
                    transactions = report.transactions,
                    alerts = report.alerts,
                    poll_failures = report.poll_failures,
                    webhook_failures = report.webhook_failures,
                    "poll cycle finished"
                );
                if !report.is_success() {
                    return Err(anyhow::anyhow!(
                        "poll cycle had {} failed contract poll(s) and {} failed webhook delivery(ies)",
                        report.poll_failures,
                        report.webhook_failures
                    ));
                }
                return Ok(());
            }

            // Graceful shutdown: allow the current poll cycle to finish before exiting.
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            tokio::spawn(async move {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    warn!(error = ?e, "failed to install Ctrl+C handler");
                    return;
                }
                let _ = shutdown_tx.send(true);
            });

            info!(
                version = VERSION,
                contracts = cfg.contracts.len(),
                interval_secs = cfg.poll_interval_seconds,
                dry_run = dry_run,
                "starting TxWatch"
            );
            txwatch_poller::run_with_shutdown(cfg, dry_run, shutdown_rx).await?;
        }
    }

    Ok(())
}
fn required_config(cli: &Cli) -> Result<PathBuf> {
    Ok(cli.config.clone().unwrap_or_else(|| PathBuf::from("config/example.toml")))
}

/// Load the config and apply `--horizon-url`, if given, to every contract.
fn load_config(cli: &Cli) -> Result<AppConfig> {
    let mut cfg = AppConfig::from_file(&required_config(cli)?)?;
    if let Some(url) = &cli.horizon_url {
        let url = url.trim_end_matches('/');
        for c in &mut cfg.contracts {
            c.horizon_base_url_override = Some(url.to_string());
        }
    }
    Ok(cfg)
}

/// Machine-readable `validate` summary. Webhook secrets are never printed;
/// only whether one is set.
fn config_summary_json(cfg: &AppConfig) -> serde_json::Value {
    let contracts: Vec<_> = cfg
        .contracts
        .iter()
        .map(|c| {
            serde_json::json!({
                "label": c.label,
                "contract_id": c.contract_id,
                "network": c.network.as_str(),
                "poll_interval_seconds": c.effective_poll_interval(cfg.poll_interval_seconds),
                "webhook_url": c.webhook_url,
                "webhook_secret_set": c.webhook_secret.is_some(),
                "rules": c.rules,
                "horizon_url": c.network.horizon_base_url(),
                "explorer_url": format!("{}/contract/{}", c.network.explorer_base_url(), c.contract_id),
            })
        })
        .collect();
    serde_json::json!({
        "valid": true,
        "poll_interval_seconds": cfg.poll_interval_seconds,
        "cursor_file": cfg.cursor_file,
        "contracts": contracts,
    })
}

async fn check_webhook_reachable(client: &Client, url: &str) -> Result<&'static str> {
    let response = client.head(url).send().await;
    match response {
        Ok(resp) if resp.status().is_success() => Ok("reachable"),
        Ok(resp)
            if resp.status() == StatusCode::METHOD_NOT_ALLOWED
                || resp.status() == StatusCode::NOT_IMPLEMENTED =>
        {
            let resp = client.request(reqwest::Method::OPTIONS, url).send().await?;
            if resp.status().is_success() { Ok("reachable (OPTIONS)") } else { Ok("method not allowed") }
        }
        Ok(_) => Ok("method not allowed"),
        Err(err) => {
            if err.is_builder() {
                return Err(err.into());
            }
            Err(err.into())
        }
    }
}

struct HorizonCheck {
    network: String,
    status: &'static str,
    latest_ledger: Option<u64>,
    reachable: bool,
    found: bool,
    message: String,
}

async fn check_horizon_contract(client: &Client, contract: &txwatch_config::WatchedContract) -> HorizonCheck {
    let base = contract.horizon_base_url_override.as_deref().unwrap_or_else(|| contract.network.horizon_base_url());
    let root = client.get(base).send().await;
    let (reachable, latest_ledger) = match root {
        Ok(response) if response.status().is_success() => {
            let json = response.json::<serde_json::Value>().await.unwrap_or_default();
            (true, json.get("core_latest_ledger").and_then(|value| value.as_u64().or_else(|| value.as_str().and_then(|text| text.parse().ok()))))
        }
        _ => (false, None),
    };
    if !reachable {
        return HorizonCheck { network: contract.network.as_str().into(), status: "unreachable", latest_ledger, reachable: false, found: false, message: format!("Horizon {} is unreachable", base) };
    }
    // Horizon exposes Soroban contract activity through the same account-style
    // transactions collection used by the poller. A 404 means the contract is
    // not known on this network; a successful empty collection is still valid.
    let found = client.get(format!("{}/accounts/{}/transactions?limit=1", base, contract.contract_id)).send().await.map(|response| response.status().is_success()).unwrap_or(false);
    HorizonCheck { network: contract.network.as_str().into(), status: if found { "found" } else { "not found" }, latest_ledger, reachable, found, message: if found { format!("{} exists on {}", contract.label, contract.network) } else { format!("{} not found on {}", contract.contract_id, contract.network) } }
}
// ── Tracing initialisation ────────────────────────────────────────────────────

fn init_tracing(format: LogFormat) {
    use tracing_subscriber::{fmt, EnvFilter};
    let builder = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false);
    match format {
        LogFormat::Text => builder.init(),
        // One JSON object per line, with the current span and the full span
        // list so fields such as `contract` and `tx` stay structured.
        LogFormat::Json => builder
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .init(),
    }
}
