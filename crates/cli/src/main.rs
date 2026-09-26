use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
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

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the polling engine (watches all contracts in the config)
    Watch {
        /// Do not actually send webhooks; only log matched rules
        #[arg(long)]
        dry_run: bool,

        /// Serve Prometheus /metrics (plus /healthz and /readyz) on this address, e.g. 127.0.0.1:9090
        #[cfg(feature = "metrics")]
        #[arg(long)]
        metrics_addr: Option<std::net::SocketAddr>,
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

    /// Print a shell completion script, e.g. `txwatch completions bash > /etc/bash_completion.d/txwatch`
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },

    /// Print the txwatch(1) man page in roff format, e.g. `txwatch man > txwatch.1`
    Man,

    /// Write a starter config file for one contract
    ///
    /// Missing values are prompted for on stdin. The result is validated before
    /// it is written, and an existing file is never overwritten without --force.
    Init {
        /// Soroban contract address to watch (56 characters, starts with 'C')
        #[arg(long)]
        contract_id: Option<String>,

        /// Stellar network: mainnet, testnet or futurenet
        #[arg(long)]
        network: Option<String>,

        /// URL that receives webhook alerts
        #[arg(long)]
        webhook_url: Option<String>,

        /// Where to write the config
        #[arg(long, default_value = "txwatch.toml")]
        output: PathBuf,

        /// Overwrite the output file if it already exists
        #[arg(long)]
        force: bool,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Command::Validate { check_webhooks, check_horizon } => {
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

        Command::Init { ref contract_id, ref network, ref webhook_url, ref output, force } => {
            if output.exists() && !force {
                return Err(anyhow::anyhow!(
                    "'{}' already exists; pass --force to overwrite it",
                    output.display()
                ));
            }
            let contract_id = value_or_prompt(contract_id, "Contract ID (C...)")?;
            let network = match network {
                Some(n) => n.clone(),
                None => prompt("Network [testnet]")?.filter(|n| !n.is_empty()).unwrap_or_else(|| "testnet".into()),
            };
            let webhook_url = value_or_prompt(webhook_url, "Webhook URL")?;

            let raw = render_init_config(&contract_id, &network, &webhook_url)?;
            // Validate with the config crate before touching the filesystem.
            AppConfig::parse(&raw, output).context("the generated config is not valid")?;
            fs::write(output, raw).with_context(|| format!("failed to write '{}'", output.display()))?;
            println!("Wrote {}. Check it with: txwatch --config {} validate", output.display(), output.display());
        }

        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "txwatch", &mut std::io::stdout());
        }

        Command::Man => {
            match clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout()) {
                // The reader (e.g. `| head`) closing early isn't an error.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                other => other.context("failed to render the man page")?,
            }
        }

        Command::Schema => println!("{}", serde_json::to_string_pretty(&schemars::schema_for!(txwatch_config::AppConfig))?),

        Command::Watch {
            dry_run,
            #[cfg(feature = "metrics")]
            metrics_addr,
        } => {
            let cfg = AppConfig::from_file(&required_config(&cli)?)?;

            // Graceful shutdown: allow the current poll cycle to finish before exiting.
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            tokio::spawn(async move {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    warn!(error = ?e, "failed to install Ctrl+C handler");
                    return;
                }
                let _ = shutdown_tx.send(true);
            });

            #[cfg(feature = "metrics")]
            if let Some(addr) = metrics_addr {
                txwatch_poller::serve_metrics(addr, shutdown_rx.clone()).await?;
            }

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
/// Read one trimmed line from stdin after printing `label`; `None` on EOF.
fn prompt(label: &str) -> Result<Option<String>> {
    use std::io::{BufRead, Write};
    eprint!("{label}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line).context("failed to read from stdin")?;
    Ok((read > 0).then(|| line.trim().to_owned()))
}

fn value_or_prompt(value: &Option<String>, label: &str) -> Result<String> {
    match value {
        Some(v) => Ok(v.clone()),
        None => prompt(label)?
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{label} is required")),
    }
}

/// Starter config for `txwatch init`: one contract and an AnyTransaction rule.
fn render_init_config(contract_id: &str, network: &str, webhook_url: &str) -> Result<String> {
    // TOML basic strings share JSON's escaping, so serde_json quotes values safely.
    let quote = |s: &str| serde_json::to_string(s);
    Ok(format!(
        r#"# Generated by `txwatch init`. See docs/configuration.md for every option.
poll_interval_seconds = 10

[[contracts]]
label       = "My Contract"
contract_id = {contract_id}
network     = {network}
webhook_url = {webhook_url}
# webhook_secret = "${{TXWATCH_WEBHOOK_SECRET}}"   # optional: signs each webhook

  [[contracts.rules]]
  type = "AnyTransaction"
"#,
        contract_id = quote(contract_id)?,
        network = quote(network)?,
        webhook_url = quote(webhook_url)?,
    ))
}

fn required_config(cli: &Cli) -> Result<PathBuf> {
    Ok(cli.config.clone().unwrap_or_else(|| PathBuf::from("config/example.toml")))
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

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}
