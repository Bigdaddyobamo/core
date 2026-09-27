#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use std::{collections::BTreeMap, env, fmt, fs, path::Path};
use url::Url;

const MAX_LARGE_TRANSFER_THRESHOLD_XLM: u64 = 1_000_000_000;

/// Soroban function names are symbols: at most 32 characters from `[a-zA-Z0-9_]`.
const MAX_SOROBAN_SYMBOL_LEN: usize = 32;

/// Maximum length of a contract label, in characters.
pub const MAX_LABEL_LEN: usize = 128;

pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 10;
const MIN_POLL_INTERVAL_SECONDS: u64 = 5;
const MAX_POLL_INTERVAL_SECONDS: u64 = 3600;

pub const DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST: usize = 10;
const MAX_HTTP_POOL_MAX_IDLE_PER_HOST: usize = 100;
pub const DEFAULT_HTTP_TCP_KEEPALIVE_SECS: u64 = 30;
const MAX_HTTP_TCP_KEEPALIVE_SECS: u64 = 7200;

/// Rejects names that can never match a Soroban function: blank, longer than
/// 32 characters, or containing anything outside `[a-zA-Z0-9_]`.
fn validate_function_name(name: &str, rule: &str, contract_label: &str) -> Result<()> {
    if name.len() > MAX_SOROBAN_SYMBOL_LEN
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!(
            "contract '{}': {} function name {:?} is not a valid Soroban symbol \
             (at most {} characters from [a-zA-Z0-9_])",
            contract_label,
            rule,
            name,
            MAX_SOROBAN_SYMBOL_LEN
        );
    }
    Ok(())
}

fn validate_poll_interval(value: u64, field: &str) -> Result<()> {
    if value < MIN_POLL_INTERVAL_SECONDS {
        bail!("{} must be >= {}", field, MIN_POLL_INTERVAL_SECONDS);
    }
    if value > MAX_POLL_INTERVAL_SECONDS {
        bail!(
            "{} must be <= {} (1 hour)",
            field,
            MAX_POLL_INTERVAL_SECONDS
        );
    }
    Ok(())
}

// ── Network ───────────────────────────────────────────────────────────────────

/// The Stellar network a contract lives on: one of the public networks by name
/// (`network = "testnet"`), or a custom / local network such as
/// `stellar/quickstart --local` given as an inline table
/// (`network = { horizon_url = "http://localhost:8000" }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
    Futurenet,
    Custom(CustomNetwork),
}

/// A standalone or private network reached through its own Horizon instance.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CustomNetwork {
    /// Horizon base URL, e.g. `http://localhost:8000` for stellar/quickstart.
    pub horizon_url: String,
    /// Optional block-explorer base URL; transaction links are `<explorer_url>/tx/<hash>`.
    #[serde(default)]
    pub explorer_url: Option<String>,
    /// Optional network passphrase, e.g. `Standalone Network ; February 2017`.
    #[serde(default)]
    pub passphrase: Option<String>,
}

const NAMED_NETWORKS: &[&str] = &["mainnet", "testnet", "futurenet"];

impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NetworkVisitor;

        impl<'de> serde::de::Visitor<'de> for NetworkVisitor {
            type Value = Network;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`mainnet`, `testnet`, `futurenet` or a { horizon_url = \"…\" } table")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Network, E> {
                match value {
                    "mainnet" => Ok(Network::Mainnet),
                    "testnet" => Ok(Network::Testnet),
                    "futurenet" => Ok(Network::Futurenet),
                    other => Err(E::unknown_variant(other, NAMED_NETWORKS)),
                }
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Network, A::Error> {
                CustomNetwork::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(Network::Custom)
            }
        }

        deserializer.deserialize_any(NetworkVisitor)
    }
}

impl Serialize for Network {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Network::Custom(custom) => custom.serialize(serializer),
            named => serializer.serialize_str(named.as_str()),
        }
    }
}

/// A public Stellar network name, or a custom network table with `horizon_url`.
#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum NetworkSchema {
    Named(NamedNetworkSchema),
    Custom(CustomNetwork),
}

#[derive(JsonSchema)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
enum NamedNetworkSchema {
    Mainnet,
    Testnet,
    Futurenet,
}

impl JsonSchema for Network {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Network".into()
    }

    fn json_schema(gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        NetworkSchema::json_schema(gen)
    }
}

impl Network {
    pub fn horizon_base_url(&self) -> &str {
        match self {
            Network::Mainnet => "https://horizon.stellar.org",
            Network::Testnet => "https://horizon-testnet.stellar.org",
            Network::Futurenet => "https://horizon-futurenet.stellar.org",
            Network::Custom(custom) => &custom.horizon_url,
        }
    }

    /// Network name used in logs and the `network` field of alert payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
            Network::Futurenet => "futurenet",
            Network::Custom(_) => "custom",
        }
    }

    /// Human-readable display name shown in logs and CLI output.
    pub fn display_name(&self) -> &'static str {
        match self {
            Network::Mainnet => "Stellar Mainnet",
            Network::Testnet => "Stellar Testnet",
            Network::Futurenet => "Stellar Futurenet",
            Network::Custom(_) => "Custom Network",
        }
    }

    /// Explorer base URL for this network (Stellar Expert for the public
    /// networks); `None` for a custom network without `explorer_url`.
    pub fn explorer_base_url(&self) -> Option<&str> {
        match self {
            Network::Mainnet => Some("https://stellar.expert/explorer/public"),
            Network::Testnet => Some("https://stellar.expert/explorer/testnet"),
            Network::Futurenet => Some("https://stellar.expert/explorer/futurenet"),
            Network::Custom(custom) => custom.explorer_url.as_deref(),
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── AlertRule ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type")]
pub enum AlertRule {
    AnyTransaction,
    TransactionFailed,
    LargeTransfer {
        threshold_xlm: u64,
    },
    FunctionCalled {
        function_name: String,
    },
    AdminFunctionCalled {
        function_names: Vec<String>,
    },
    /// Fires when the transaction's fee exceeds the threshold.
    /// Specify either `threshold_stroops` (raw stroops) or `threshold_xlm` (whole XLM,
    /// converted to stroops during validation); the two are mutually exclusive.
    HighFee {
        #[serde(default)]
        threshold_stroops: u64,
        #[serde(default)]
        threshold_xlm: Option<u64>,
    },
}

impl AlertRule {
    pub fn validate(&mut self, contract_label: &str) -> Result<()> {
        match self {
            AlertRule::LargeTransfer { threshold_xlm } => {
                if *threshold_xlm == 0 {
                    bail!(
                        "contract '{}': LargeTransfer threshold_xlm must be > 0",
                        contract_label
                    );
                }
                if *threshold_xlm > MAX_LARGE_TRANSFER_THRESHOLD_XLM {
                    bail!(
                        "contract '{}': LargeTransfer threshold_xlm must be <= {}",
                        contract_label,
                        MAX_LARGE_TRANSFER_THRESHOLD_XLM
                    );
                }
            }
            AlertRule::FunctionCalled { function_name } => {
                if function_name.trim().is_empty() {
                    bail!(
                        "contract '{}': FunctionCalled function_name must not be empty",
                        contract_label
                    );
                }
                validate_function_name(function_name, "FunctionCalled", contract_label)?;
            }
            AlertRule::AdminFunctionCalled { function_names } => {
                if function_names.is_empty() {
                    bail!(
                        "contract '{}': AdminFunctionCalled function_names must not be empty",
                        contract_label
                    );
                }
                for name in function_names.iter_mut() {
                    if name.trim().is_empty() {
                        bail!(
                            "contract '{}': AdminFunctionCalled contains a blank function name",
                            contract_label
                        );
                    }
                    validate_function_name(name, "AdminFunctionCalled", contract_label)?;
                    *name = name.to_lowercase();
                }
            }
            AlertRule::AnyTransaction | AlertRule::TransactionFailed => {}
            AlertRule::HighFee {
                threshold_stroops,
                threshold_xlm,
            } => match (*threshold_xlm, *threshold_stroops) {
                (Some(_), s) if s > 0 => bail!(
                    "contract '{}': HighFee: specify either threshold_stroops or \
                         threshold_xlm, not both",
                    contract_label
                ),
                (None, 0) => bail!(
                    "contract '{}': HighFee threshold_stroops must be > 0",
                    contract_label
                ),
                (Some(0), _) => bail!(
                    "contract '{}': HighFee threshold_xlm must be > 0",
                    contract_label
                ),
                (Some(xlm), 0) => {
                    *threshold_stroops = xlm.checked_mul(10_000_000).with_context(|| {
                        format!(
                            "contract '{}': HighFee threshold_xlm overflow",
                            contract_label
                        )
                    })?;
                }
                _ => {}
            },
        }
        Ok(())
    }
    pub fn label(&self) -> String {
        match self {
            AlertRule::AnyTransaction => "AnyTransaction".into(),
            AlertRule::TransactionFailed => "TransactionFailed".into(),
            AlertRule::LargeTransfer { threshold_xlm } => {
                format!("LargeTransfer(>={}XLM)", threshold_xlm)
            }
            AlertRule::FunctionCalled { function_name } => {
                format!("FunctionCalled({})", function_name)
            }
            AlertRule::AdminFunctionCalled { function_names } => {
                format!("AdminFunctionCalled([{}])", function_names.join(", "))
            }
            AlertRule::HighFee {
                threshold_stroops,
                threshold_xlm,
            } => {
                if let Some(xlm) = threshold_xlm {
                    format!("HighFee(>={} XLM)", xlm)
                } else {
                    format!("HighFee(>={} stroops)", threshold_stroops)
                }
            }
        }
    }
}

// ── Webhook destinations ──────────────────────────────────────────────────────

/// Printed in place of secret values (header values, secrets, routing keys).
pub const REDACTED: &str = "<redacted>";

/// Body shape sent to a webhook destination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WebhookFormat {
    /// TxWatch's own alert JSON.
    #[default]
    Txwatch,
    /// Slack incoming webhook (`text` plus Block Kit `blocks`).
    Slack,
    /// Discord webhook (`content` plus one embed).
    Discord,
    /// PagerDuty Events API v2 `trigger` event; requires `routing_key`.
    Pagerduty,
}

impl WebhookFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            WebhookFormat::Txwatch => "txwatch",
            WebhookFormat::Slack => "slack",
            WebhookFormat::Discord => "discord",
            WebhookFormat::Pagerduty => "pagerduty",
        }
    }
}

impl fmt::Display for WebhookFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Extra HTTP headers sent with every POST to a destination, e.g.
/// `{ "Authorization" = "Bearer ${TOKEN}" }`. Values often hold credentials,
/// so `Debug` prints only the header names.
#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct WebhookHeaders(pub BTreeMap<String, String>);

impl WebhookHeaders {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// `Name: <redacted>` pairs, safe to print.
    pub fn redacted(&self) -> Vec<String> {
        self.0.keys().map(|k| format!("{}: {}", k, REDACTED)).collect()
    }
}

impl fmt::Debug for WebhookHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|k| (k, REDACTED)))
            .finish()
    }
}

/// Headers TxWatch sets itself; a destination may not override them.
fn is_reserved_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "content-type" || name == "content-length" || name.starts_with("x-txwatch-")
}

/// RFC 9110 token: the characters allowed in a header name.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Header values may not contain control characters (CR/LF would allow header
/// injection); tabs are allowed.
fn is_header_value(value: &str) -> bool {
    value.chars().all(|c| c == '\t' || !c.is_control())
}

/// One place alerts are delivered to: a `[[contracts.webhooks]]` entry, or
/// the contract's `webhook_*` shorthand.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookDestination {
    /// http(s) URL the alert is POSTed to.
    pub url: String,
    /// Optional secret: sent as `X-TxWatch-Secret` and used for the
    /// `X-TxWatch-Signature` HMAC. Supports `${ENV_VAR}` interpolation.
    #[serde(default)]
    pub secret: Option<String>,
    /// Body shape: `txwatch` (default), `slack`, `discord` or `pagerduty`.
    #[serde(default)]
    pub format: WebhookFormat,
    /// Extra HTTP headers. Values support `${ENV_VAR}` interpolation.
    #[serde(default, skip_serializing_if = "WebhookHeaders::is_empty")]
    pub headers: WebhookHeaders,
    /// PagerDuty integration (routing) key; required for `format = "pagerduty"`.
    /// Supports `${ENV_VAR}` interpolation.
    #[serde(default)]
    pub routing_key: Option<String>,
}

impl fmt::Debug for WebhookDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookDestination")
            .field("url", &self.url)
            .field("secret", &self.secret.as_ref().map(|_| REDACTED))
            .field("format", &self.format)
            .field("headers", &self.headers)
            .field("routing_key", &self.routing_key.as_ref().map(|_| REDACTED))
            .finish()
    }
}

impl WebhookDestination {
    /// Validates a stand-alone destination (e.g. one built from CLI flags).
    pub fn validate(&self) -> Result<()> {
        ValidationErrors::into_result(self.problems(&|name| name.to_owned()))
    }

    /// Every problem with this destination. `field(name)` renders a field's
    /// config path, e.g. `webhook_url` or `webhooks[1].url`. Header values and
    /// secrets never appear in the messages.
    fn problems(&self, field: &dyn Fn(&str) -> String) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(problem) = check_http_url(&self.url) {
            problems.push(format!("{} {}", field("url"), problem));
        }
        for (name, value) in self.headers.iter() {
            if !is_header_name(name) {
                problems.push(format!(
                    "{} {:?} is not a valid HTTP header name",
                    field("headers"),
                    name
                ));
            } else if is_reserved_header(name) {
                problems.push(format!(
                    "{} {:?} is reserved: Content-Type, Content-Length and X-TxWatch-* \
                     are set by TxWatch",
                    field("headers"),
                    name
                ));
            }
            if !is_header_value(value) {
                problems.push(format!(
                    "{} value of {:?} contains control characters",
                    field("headers"),
                    name
                ));
            }
        }
        let has_routing_key = self
            .routing_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty());
        match (self.format, has_routing_key) {
            (WebhookFormat::Pagerduty, false) => problems.push(format!(
                "{} is required when format is \"pagerduty\"",
                field("routing_key")
            )),
            (format, true) if format != WebhookFormat::Pagerduty => problems.push(format!(
                "{} is only used when format is \"pagerduty\"",
                field("routing_key")
            )),
            _ => {}
        }
        problems
    }
}

// ── WatchedContract ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchedContract {
    pub label: String,
    pub contract_id: String,
    pub network: Network,
    pub rules: Vec<AlertRule>,
    /// Shorthand for a single destination; the `webhook_*` fields below
    /// describe it. Use `webhooks` for more than one destination. Optional when
    /// `webhooks` is non-empty.
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Optional secret sent as X-TxWatch-Secret header on every webhook POST.
    /// Supports `${ENV_VAR}` interpolation (e.g. `webhook_secret = "${MY_SECRET}"`).
    pub webhook_secret: Option<String>,
    /// Body shape for `webhook_url` (default `txwatch`).
    #[serde(default)]
    pub webhook_format: WebhookFormat,
    /// Extra HTTP headers for `webhook_url`, with `${ENV_VAR}` interpolation.
    #[serde(default, skip_serializing_if = "WebhookHeaders::is_empty")]
    pub webhook_headers: WebhookHeaders,
    /// PagerDuty routing key for `webhook_url` when `webhook_format = "pagerduty"`.
    #[serde(default)]
    pub webhook_routing_key: Option<String>,
    /// Additional destinations (`[[contracts.webhooks]]`). Every alert is
    /// delivered to each destination independently.
    #[serde(default)]
    pub webhooks: Vec<WebhookDestination>,
    /// Per-contract polling interval in seconds, overriding the top-level
    /// `poll_interval_seconds`. Same bounds (5–3600).
    #[serde(default)]
    pub poll_interval_seconds: Option<u64>,
    /// Override the Horizon base URL; never read from TOML — set programmatically in tests.
    #[serde(skip, default)]
    #[schemars(skip)]
    pub horizon_base_url_override: Option<String>,
}

/// Every problem found while validating a config. Each entry keeps the
/// `contract '<label>': <message>` format; `Display` prints one per line.
#[derive(Debug)]
pub struct ValidationErrors(pub Vec<String>);

impl ValidationErrors {
    fn into_result(errors: Vec<String>) -> Result<()> {
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors).into())
        }
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let [only] = self.0.as_slice() {
            return f.write_str(only);
        }
        write!(f, "{} configuration errors:", self.0.len())?;
        for error in &self.0 {
            write!(f, "\n  - {}", error)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

/// Checks that `value` is an http(s) URL with a host; returns a description
/// of the problem otherwise.
fn check_http_url(value: &str) -> Option<String> {
    match Url::parse(value) {
        Err(e) => Some(format!("'{}' is not a valid URL: {}", value, e)),
        Ok(url) if url.scheme() != "http" && url.scheme() != "https" => {
            Some(format!("'{}' must use http or https scheme", value))
        }
        Ok(url) if url.host().is_none() => Some(format!("'{}' has no host", value)),
        Ok(_) => None,
    }
}

impl WatchedContract {
    /// Every webhook destination: the `webhook_*` shorthand (when `webhook_url`
    /// is set) followed by the `[[contracts.webhooks]]` entries.
    pub fn destinations(&self) -> Vec<WebhookDestination> {
        let mut destinations = Vec::with_capacity(self.webhooks.len() + 1);
        if let Some(url) = &self.webhook_url {
            destinations.push(WebhookDestination {
                url: url.clone(),
                secret: self.webhook_secret.clone(),
                format: self.webhook_format,
                headers: self.webhook_headers.clone(),
                routing_key: self.webhook_routing_key.clone(),
            });
        }
        destinations.extend(self.webhooks.iter().cloned());
        destinations
    }

    /// The interval this contract is polled at: its own override, or `default`
    /// (the top-level `poll_interval_seconds`).
    pub fn effective_poll_interval(&self, default: u64) -> u64 {
        self.poll_interval_seconds.unwrap_or(default)
    }

    pub fn validate(&mut self) -> Result<()> {
        ValidationErrors::into_result(self.collect_errors())
    }

    /// Runs every contract check and returns all failures instead of stopping
    /// at the first one.
    fn collect_errors(&mut self) -> Vec<String> {
        let mut errors = Vec::new();

        self.label = self.label.trim().to_owned();
        if self.label.is_empty() {
            errors.push("a contract has an empty label".to_owned());
        }
        // Labels end up in log lines and CLI output; `{:?}` escapes the
        // offending characters so the error itself cannot inject them.
        if self.label.chars().any(char::is_control) {
            errors.push(format!(
                "contract label {:?} must not contain control characters",
                self.label
            ));
        }
        if self.label.chars().count() > MAX_LABEL_LEN {
            errors.push(format!(
                "contract label '{}…' is longer than {} characters",
                self.label.chars().take(32).collect::<String>(),
                MAX_LABEL_LEN
            ));
        }
        if let Some(interval) = self.poll_interval_seconds {
            if let Err(e) = validate_poll_interval(
                interval,
                &format!("contract '{}': poll_interval_seconds", self.label),
            ) {
                errors.push(e.to_string());
            }
        }

        // Stellar contract addresses start with 'C' and are 56 chars (base32)
        if self.contract_id.len() != 56 || !self.contract_id.starts_with('C') {
            errors.push(format!(
                "contract '{}': contract_id '{}' is not a valid Stellar contract address \
                 (must start with 'C' and be 56 characters)",
                self.label, self.contract_id
            ));
        }

        if self.webhook_url.is_none() {
            let stray: Vec<&str> = [
                ("webhook_secret", self.webhook_secret.is_some()),
                ("webhook_format", self.webhook_format != WebhookFormat::default()),
                ("webhook_headers", !self.webhook_headers.is_empty()),
                ("webhook_routing_key", self.webhook_routing_key.is_some()),
            ]
            .into_iter()
            .filter_map(|(name, set)| set.then_some(name))
            .collect();
            if !stray.is_empty() {
                errors.push(format!(
                    "contract '{}': {} set without webhook_url (put them in a \
                     [[contracts.webhooks]] entry instead)",
                    self.label,
                    stray.join(", ")
                ));
            }
        }
        let shorthand = usize::from(self.webhook_url.is_some());
        let destinations = self.destinations();
        if destinations.is_empty() {
            errors.push(format!(
                "contract '{}': no webhook destination; set webhook_url or add a \
                 [[contracts.webhooks]] entry",
                self.label
            ));
        }
        for (i, destination) in destinations.iter().enumerate() {
            let field = |name: &str| {
                if i < shorthand {
                    format!("webhook_{}", name)
                } else {
                    format!("webhooks[{}].{}", i - shorthand, name)
                }
            };
            for problem in destination.problems(&field) {
                errors.push(format!("contract '{}': {}", self.label, problem));
            }
        }

        if let Network::Custom(custom) = &mut self.network {
            // Links and request URLs are built as "<base>/...".
            custom.horizon_url = custom.horizon_url.trim_end_matches('/').to_owned();
            if let Some(problem) = check_http_url(&custom.horizon_url) {
                errors.push(format!(
                    "contract '{}': network horizon_url {}",
                    self.label, problem
                ));
            }
            if let Some(explorer_url) = &mut custom.explorer_url {
                *explorer_url = explorer_url.trim_end_matches('/').to_owned();
                if let Some(problem) = check_http_url(explorer_url) {
                    errors.push(format!(
                        "contract '{}': network explorer_url {}",
                        self.label, problem
                    ));
                }
            }
        }

        if self.rules.is_empty() {
            errors.push(format!(
                "contract '{}': at least one rule is required",
                self.label
            ));
        }

        let label = self.label.clone();
        for rule in &mut self.rules {
            if let Err(e) = rule.validate(&label) {
                errors.push(e.to_string());
            }
        }

        errors
    }
}

// ── AppConfig ─────────────────────────────────────────────────────────────────

/// Maximum number of watched contracts allowed in a single configuration.
/// Exceeding this limit would create too many concurrent Horizon polling tasks,
/// potentially exhausting memory or file descriptors.
pub const MAX_CONTRACTS: usize = 100;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Default polling interval in seconds for every contract (5–3600).
    /// Default: 10.
    #[serde(default = "default_poll_interval_seconds")]
    pub poll_interval_seconds: u64,
    pub contracts: Vec<WatchedContract>,
    /// Optional path to a JSON file used to persist the cursor map across restarts.
    /// When set, the poller will load cursors from this file on startup and write
    /// the updated cursor map after each poll cycle. If absent, cursors default
    /// to the Horizon keyword `now` and are not persisted.
    #[serde(default)]
    pub cursor_file: Option<String>,
    /// Maximum number of idle connections per host in the HTTP connection pool.
    /// Lower values reduce memory usage; higher values improve throughput for many contracts.
    /// Must be 1–100. Default: 10.
    #[serde(default = "default_http_pool_max_idle_per_host")]
    pub http_pool_max_idle_per_host: usize,
    /// TCP keepalive interval in seconds for idle HTTP connections.
    /// Helps detect stalled connections quickly; 0 disables keepalive.
    /// Must be <= 7200. Default: 30 seconds.
    #[serde(default = "default_http_tcp_keepalive_secs")]
    pub http_tcp_keepalive_secs: u64,
    /// Enable verbose output for HTTP connection pool debug information.
    /// Only useful for troubleshooting connection issues.
    /// Default: false.
    #[serde(default)]
    pub http_connection_verbose: Option<bool>,
}

fn default_poll_interval_seconds() -> u64 {
    DEFAULT_POLL_INTERVAL_SECONDS
}

fn default_http_pool_max_idle_per_host() -> usize {
    DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST
}

fn default_http_tcp_keepalive_secs() -> u64 {
    DEFAULT_HTTP_TCP_KEEPALIVE_SECS
}

fn deserialize_toml_with_field_context<T>(raw: &str, path: &Path) -> Result<T>
where
    T: DeserializeOwned,
{
    // serde_path_to_error::deserialize owns the Track that Deserializer::new
    // otherwise requires, so the field path survives into the error message.
    let deserializer = toml::Deserializer::new(raw);
    serde_path_to_error::deserialize(deserializer).map_err(|error| {
        let field_path = error.path().to_string();
        let inner = error.into_inner();
        if field_path.is_empty() {
            anyhow!("{} (in {})", inner, path.display())
        } else {
            anyhow!("{} (field: {} in {})", inner, field_path, path.display())
        }
    })
}

// ── Env-var interpolation ─────────────────────────────────────────────────────

fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Expands environment-variable references anywhere in `value`:
///
/// - `${VAR}` is replaced by the value of `VAR`; an unset variable is an error.
/// - `${VAR:-default}` uses `default` when `VAR` is unset or empty.
/// - `$${` is an escape for a literal `${`.
/// - Any other `$` is kept as is.
///
/// `lookup` resolves a variable name; errors never include resolved values.
fn interpolate(value: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar..];
        if let Some(tail) = after.strip_prefix("$${") {
            out.push_str("${");
            rest = tail;
        } else if let Some(tail) = after.strip_prefix("${") {
            let end = tail.find('}').context("unterminated '${'")?;
            let expr = &tail[..end];
            let (name, default) = match expr.split_once(":-") {
                Some((name, default)) => (name, Some(default)),
                None => (expr, None),
            };
            if name.is_empty() {
                bail!("empty variable name in '${{}}'");
            }
            if !is_env_var_name(name) {
                bail!(
                    "invalid variable name {:?} (use letters, digits and '_', \
                     not starting with a digit)",
                    name
                );
            }
            match (lookup(name), default) {
                (Some(resolved), Some(default)) if resolved.is_empty() => out.push_str(default),
                (Some(resolved), _) => out.push_str(&resolved),
                (None, Some(default)) => out.push_str(default),
                (None, None) => bail!("env var '{}' referenced in config is not set", name),
            }
            rest = &tail[end + 1..];
        } else {
            out.push('$');
            rest = &after[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// [`interpolate`] against the process environment.
fn resolve_env_interpolation(value: &str) -> Result<String> {
    interpolate(value, &|name| env::var(name).ok())
}

/// Interpolates `value` in place, naming `field` (never the value) in errors.
fn resolve_field(value: &mut String, field: &str) -> Result<()> {
    *value = resolve_env_interpolation(value).with_context(|| field.to_owned())?;
    Ok(())
}

/// Interpolates a destination's secret-bearing fields.
fn resolve_destination(
    secret: &mut Option<String>,
    headers: &mut WebhookHeaders,
    routing_key: &mut Option<String>,
    field: &dyn Fn(&str) -> String,
) -> Result<()> {
    if let Some(secret) = secret {
        resolve_field(secret, &field("secret"))?;
    }
    for (name, value) in headers.0.iter_mut() {
        resolve_field(value, &format!("{}.{}", field("headers"), name))?;
    }
    if let Some(key) = routing_key {
        resolve_field(key, &field("routing_key"))?;
    }
    Ok(())
}

impl AppConfig {
    /// Expands `${VAR}` references in webhook secrets, header values and
    /// PagerDuty routing keys.
    fn resolve_env_vars(&mut self) -> Result<()> {
        for (i, contract) in self.contracts.iter_mut().enumerate() {
            resolve_destination(
                &mut contract.webhook_secret,
                &mut contract.webhook_headers,
                &mut contract.webhook_routing_key,
                &|name| format!("contracts[{}].webhook_{}", i, name),
            )?;
            for (j, webhook) in contract.webhooks.iter_mut().enumerate() {
                resolve_destination(
                    &mut webhook.secret,
                    &mut webhook.headers,
                    &mut webhook.routing_key,
                    &|name| format!("contracts[{}].webhooks[{}].{}", i, j, name),
                )?;
            }
        }
        Ok(())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("cannot read config file '{}'", path.display()))?;
        Self::parse(&raw, path)
    }

    /// Parse and validate TOML exactly as [`AppConfig::from_file`] does;
    /// `source` only labels error messages.
    pub fn parse(raw: &str, source: &Path) -> Result<Self> {
        let mut cfg: AppConfig = deserialize_toml_with_field_context(raw, source)
            .with_context(|| format!("failed to parse config file '{}'", source.display()))?;
        cfg.resolve_env_vars()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validates the whole config and reports every error found, not just the first.
    pub fn validate(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        if let Err(e) = validate_poll_interval(self.poll_interval_seconds, "poll_interval_seconds")
        {
            errors.push(e.to_string());
        }
        if self.http_pool_max_idle_per_host == 0
            || self.http_pool_max_idle_per_host > MAX_HTTP_POOL_MAX_IDLE_PER_HOST
        {
            errors.push(format!(
                "http_pool_max_idle_per_host must be between 1 and {}",
                MAX_HTTP_POOL_MAX_IDLE_PER_HOST
            ));
        }
        if self.http_tcp_keepalive_secs > MAX_HTTP_TCP_KEEPALIVE_SECS {
            errors.push(format!(
                "http_tcp_keepalive_secs must be <= {} (0 disables keepalive)",
                MAX_HTTP_TCP_KEEPALIVE_SECS
            ));
        }
        if self.contracts.is_empty() {
            errors.push("at least one [[contracts]] entry is required".to_owned());
        }
        for contract in &mut self.contracts {
            errors.extend(contract.collect_errors());
        }
        // Labels are already trimmed by `WatchedContract::validate`; compare
        // case-insensitively so "Vault" and "vault" count as duplicates.
        let mut seen = std::collections::HashSet::new();
        let mut reported = std::collections::HashSet::new();
        for contract in &self.contracts {
            let key = contract.label.to_lowercase();
            if !seen.insert(key.clone()) && reported.insert(key) {
                errors.push(format!("duplicate contract label '{}'", contract.label));
            }
        }
        ValidationErrors::into_result(errors)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn valid_contract() -> WatchedContract {
        WatchedContract {
            label: "Test".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: Some("https://example.com/hook".into()),
            webhook_secret: None,
            poll_interval_seconds: None,
            horizon_base_url_override: None,
            webhook_format: Default::default(),
            webhook_headers: Default::default(),
            webhook_routing_key: None,
            webhooks: Vec::new(),
        }
    }

    #[test]
    fn valid_config_passes() {
        let mut c = valid_contract();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_short_contract_id() {
        let mut c = valid_contract();
        c.contract_id = "CSHORT".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_non_c_contract_id() {
        let mut c = valid_contract();
        c.contract_id = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_bad_webhook_url() {
        let mut c = valid_contract();
        c.webhook_url = Some("ftp://bad".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_no_host() {
        let mut c = valid_contract();
        c.webhook_url = Some("https://".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_spaces() {
        let mut c = valid_contract();
        c.webhook_url = Some("https://example .com/hook".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_that_is_not_a_url() {
        let mut c = valid_contract();
        c.webhook_url = Some("not-a-url-at-all".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_ftp_scheme() {
        let mut c = valid_contract();
        c.webhook_url = Some("ftp://files.example.com/hook".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn accepts_valid_http_webhook_url() {
        let mut c = valid_contract();
        c.webhook_url = Some("http://hooks.example.com/my-webhook".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn accepts_valid_https_webhook_url_with_path_and_query() {
        let mut c = valid_contract();
        c.webhook_url = Some("https://hooks.example.com/alerts?token=abc123".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_empty_rules() {
        let mut c = valid_contract();
        c.rules = vec![];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_zero_threshold() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::LargeTransfer { threshold_xlm: 0 }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_too_large_large_transfer_threshold() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::LargeTransfer {
            threshold_xlm: MAX_LARGE_TRANSFER_THRESHOLD_XLM + 1,
        }];
        let err = c.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("LargeTransfer threshold_xlm must be <="));
    }

    #[test]
    fn rejects_empty_function_name() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::FunctionCalled {
            function_name: "  ".into(),
        }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_empty_admin_function_names() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec![],
        }];
        assert!(c.validate().is_err());
    }

    /// Issue #18: blank entry in function_names should fail validation.
    #[test]
    fn rejects_blank_entry_in_admin_function_names() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec!["set_admin".into(), " ".into()],
        }];
        let err = c.validate().unwrap_err();
        assert!(
            err.to_string().contains("blank"),
            "expected 'blank' in error, got: {}",
            err
        );
    }

    /// Issue #18: single valid entry in function_names should pass validation.
    #[test]
    fn accepts_single_valid_admin_function_name() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec!["set_admin".into()],
        }];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn admin_function_names_normalised_to_lowercase() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec!["Set_Admin".into(), "UPGRADE".into()],
        }];
        c.validate().unwrap();
        if let AlertRule::AdminFunctionCalled { function_names } = &c.rules[0] {
            assert_eq!(function_names, &["set_admin", "upgrade"]);
        } else {
            panic!("expected AdminFunctionCalled");
        }
    }

    #[test]
    fn network_urls() {
        assert!(Network::Mainnet
            .horizon_base_url()
            .contains("horizon.stellar.org"));
        assert!(Network::Testnet.horizon_base_url().contains("testnet"));
        assert!(Network::Futurenet.horizon_base_url().contains("futurenet"));
    }

    #[test]
    fn network_display_names() {
        assert_eq!(Network::Mainnet.display_name(), "Stellar Mainnet");
        assert_eq!(Network::Testnet.display_name(), "Stellar Testnet");
        assert_eq!(Network::Futurenet.display_name(), "Stellar Futurenet");
    }

    #[test]
    fn network_explorer_urls() {
        assert!(Network::Mainnet
            .explorer_base_url()
            .unwrap()
            .contains("public"));
        assert!(Network::Testnet
            .explorer_base_url()
            .unwrap()
            .contains("testnet"));
    }

    // ── #99: custom / local network ──────────────────────────────────────────

    const CUSTOM_NETWORK_TOML: &str = r#"
        [[contracts]]
        label = "local"
        contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        network = { horizon_url = "http://localhost:8000/", passphrase = "Standalone Network ; February 2017" }
        webhook_url = "https://example.com/hook"
        [[contracts.rules]]
        type = "AnyTransaction"
    "#;

    fn parse_with_interval(contracts_toml: &str) -> AppConfig {
        toml::from_str(&format!("poll_interval_seconds = 10\n{}", contracts_toml)).unwrap()
    }

    #[test]
    fn custom_network_parses_and_validates() {
        let mut cfg = parse_with_interval(CUSTOM_NETWORK_TOML);
        cfg.validate().unwrap();
        let network = &cfg.contracts[0].network;
        assert_eq!(
            network,
            &Network::Custom(CustomNetwork {
                horizon_url: "http://localhost:8000".into(),
                explorer_url: None,
                passphrase: Some("Standalone Network ; February 2017".into()),
            })
        );
        assert_eq!(network.horizon_base_url(), "http://localhost:8000");
        assert_eq!(network.explorer_base_url(), None);
        assert_eq!(network.as_str(), "custom");
        assert_eq!(network.display_name(), "Custom Network");
    }

    #[test]
    fn custom_network_requires_horizon_url() {
        let raw = CUSTOM_NETWORK_TOML.replace(
            r#"{ horizon_url = "http://localhost:8000/", passphrase"#,
            r#"{ passphrase"#,
        );
        let err = toml::from_str::<AppConfig>(&format!("poll_interval_seconds = 10\n{}", raw))
            .unwrap_err();
        assert!(err.to_string().contains("horizon_url"), "got: {}", err);
    }

    #[test]
    fn custom_network_rejects_invalid_urls() {
        let raw = CUSTOM_NETWORK_TOML.replace(
            r#"horizon_url = "http://localhost:8000/""#,
            r#"horizon_url = "ftp://localhost", explorer_url = "not a url""#,
        );
        let err = parse_with_interval(&raw)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("contract 'local': network horizon_url"),
            "got: {}",
            err
        );
        assert!(
            err.contains("contract 'local': network explorer_url"),
            "got: {}",
            err
        );
    }

    #[test]
    fn unknown_network_name_is_still_rejected_with_variant_list() {
        let raw = CUSTOM_NETWORK_TOML.replace(
            r#"{ horizon_url = "http://localhost:8000/", passphrase = "Standalone Network ; February 2017" }"#,
            r#""main""#,
        );
        let err = toml::from_str::<AppConfig>(&format!("poll_interval_seconds = 10\n{}", raw))
            .unwrap_err();
        assert!(
            err.to_string().contains(
                "unknown variant `main`, expected one of `mainnet`, `testnet`, `futurenet`"
            ),
            "got: {}",
            err
        );
    }

    // ── #101: all validation errors reported together ────────────────────────

    #[test]
    fn validate_reports_all_errors_together() {
        let mut bad_id = valid_contract();
        bad_id.label = "A".into();
        bad_id.contract_id = "CSHORT".into();
        bad_id.webhook_url = Some("ftp://bad".into());
        let mut bad_rule = valid_contract();
        bad_rule.label = "B".into();
        bad_rule.rules = vec![AlertRule::LargeTransfer { threshold_xlm: 0 }];
        let mut cfg = AppConfig {
            poll_interval_seconds: 1,
            contracts: vec![bad_id, bad_rule, valid_contract(), valid_contract()],
            http_pool_max_idle_per_host: DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST,
            http_tcp_keepalive_secs: DEFAULT_HTTP_TCP_KEEPALIVE_SECS,
            http_connection_verbose: None,
            cursor_file: None,
        };
        let err = cfg.validate().unwrap_err();
        let errors = &err.downcast_ref::<ValidationErrors>().unwrap().0;
        assert_eq!(
            errors,
            &[
                "poll_interval_seconds must be >= 5".to_owned(),
                "contract 'A': contract_id 'CSHORT' is not a valid Stellar contract address \
                 (must start with 'C' and be 56 characters)"
                    .to_owned(),
                "contract 'A': webhook_url 'ftp://bad' must use http or https scheme".to_owned(),
                "contract 'B': LargeTransfer threshold_xlm must be > 0".to_owned(),
                "duplicate contract label 'Test'".to_owned(),
            ]
        );
        let text = err.to_string();
        assert!(
            text.starts_with("5 configuration errors:\n  - "),
            "got: {}",
            text
        );
    }

    #[test]
    fn rejects_duplicate_labels() {
        let c = valid_contract();
        let mut cfg = AppConfig {
            poll_interval_seconds: 10,
            contracts: vec![c.clone(), c],
            http_pool_max_idle_per_host: DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST,
            http_tcp_keepalive_secs: DEFAULT_HTTP_TCP_KEEPALIVE_SECS,
            http_connection_verbose: None,
            cursor_file: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate contract label"));
    }

    #[test]
    fn appconfig_validate_rejects_empty_contracts() {
        let mut cfg = AppConfig {
            poll_interval_seconds: 10,
            contracts: vec![],
            http_pool_max_idle_per_host: DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST,
            http_tcp_keepalive_secs: DEFAULT_HTTP_TCP_KEEPALIVE_SECS,
            http_connection_verbose: None,
            cursor_file: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("at least one"),
            "error should mention 'at least one', got: {}",
            err
        );
    }

    #[test]
    fn high_fee_threshold_xlm_normalises_to_stroops() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::HighFee {
            threshold_stroops: 0,
            threshold_xlm: Some(1),
        }];
        c.validate().unwrap();
        if let AlertRule::HighFee {
            threshold_stroops, ..
        } = &c.rules[0]
        {
            assert_eq!(
                *threshold_stroops, 10_000_000,
                "1 XLM should become 10_000_000 stroops"
            );
        } else {
            panic!("expected HighFee");
        }
    }

    #[test]
    fn high_fee_threshold_xlm_zero_is_rejected() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::HighFee {
            threshold_stroops: 0,
            threshold_xlm: Some(0),
        }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn high_fee_both_thresholds_is_rejected() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::HighFee {
            threshold_stroops: 100,
            threshold_xlm: Some(1),
        }];
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("not both"));
    }

    #[test]
    fn high_fee_neither_threshold_is_rejected() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::HighFee {
            threshold_stroops: 0,
            threshold_xlm: None,
        }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_poll_interval_too_low() {
        for val in 0u64..5 {
            let mut cfg = AppConfig {
                poll_interval_seconds: val,
                contracts: vec![valid_contract()],
                http_pool_max_idle_per_host: DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST,
                http_tcp_keepalive_secs: DEFAULT_HTTP_TCP_KEEPALIVE_SECS,
                http_connection_verbose: None,
                cursor_file: None,
            };
            let err = cfg.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("poll_interval_seconds must be >= 5"),
                "val={} should be rejected: {}",
                val,
                err
            );
        }
    }

    #[test]
    fn rejects_poll_interval_over_max() {
        let raw = r#"
            poll_interval_seconds = 9999
            [[contracts]]
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            [[contracts.rules]]
            type = "AnyTransaction"
        "#;
        let mut cfg: AppConfig = toml::from_str(raw).unwrap();
        assert!(cfg.validate().is_err());
    }

    fn config_with(contracts: Vec<WatchedContract>) -> AppConfig {
        AppConfig {
            poll_interval_seconds: DEFAULT_POLL_INTERVAL_SECONDS,
            contracts,
            http_pool_max_idle_per_host: DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST,
            http_tcp_keepalive_secs: DEFAULT_HTTP_TCP_KEEPALIVE_SECS,
            http_connection_verbose: None,
            cursor_file: None,
        }
    }

    const MINIMAL_TOML: &str = r#"
        [[contracts]]
        label = "x"
        contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        network = "testnet"
        webhook_url = "https://example.com/hook"
        [[contracts.rules]]
        type = "AnyTransaction"
    "#;

    // ── #97: poll interval default and per-contract override ─────────────────

    #[test]
    fn poll_interval_and_http_settings_default_when_omitted() {
        let mut cfg: AppConfig = toml::from_str(MINIMAL_TOML).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.poll_interval_seconds, 10);
        assert_eq!(cfg.http_pool_max_idle_per_host, 10);
        assert_eq!(cfg.http_tcp_keepalive_secs, 30);
        assert_eq!(cfg.contracts[0].poll_interval_seconds, None);
        assert_eq!(
            cfg.contracts[0].effective_poll_interval(cfg.poll_interval_seconds),
            10
        );
    }

    #[test]
    fn per_contract_poll_interval_overrides_global() {
        let raw = r#"
            poll_interval_seconds = 60
            [[contracts]]
            label = "fast"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            poll_interval_seconds = 5
            [[contracts.rules]]
            type = "AnyTransaction"
            [[contracts]]
            label = "slow"
            contract_id = "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            [[contracts.rules]]
            type = "AnyTransaction"
        "#;
        let mut cfg: AppConfig = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.contracts[0].effective_poll_interval(cfg.poll_interval_seconds),
            5
        );
        assert_eq!(
            cfg.contracts[1].effective_poll_interval(cfg.poll_interval_seconds),
            60
        );
    }

    #[test]
    fn rejects_per_contract_poll_interval_out_of_bounds() {
        for val in [0, 4, 3601] {
            let mut c = valid_contract();
            c.poll_interval_seconds = Some(val);
            let err = config_with(vec![c]).validate().unwrap_err().to_string();
            assert!(
                err.contains("contract 'Test': poll_interval_seconds must be"),
                "val={} should be rejected, got: {}",
                val,
                err
            );
        }
    }

    // ── #96: HTTP pool settings ──────────────────────────────────────────────

    #[test]
    fn rejects_zero_http_pool_max_idle_per_host() {
        let mut cfg = config_with(vec![valid_contract()]);
        cfg.http_pool_max_idle_per_host = 0;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("http_pool_max_idle_per_host"));
    }

    #[test]
    fn rejects_too_large_http_pool_max_idle_per_host() {
        let mut cfg = config_with(vec![valid_contract()]);
        cfg.http_pool_max_idle_per_host = MAX_HTTP_POOL_MAX_IDLE_PER_HOST + 1;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_too_large_http_tcp_keepalive_secs() {
        let mut cfg = config_with(vec![valid_contract()]);
        cfg.http_tcp_keepalive_secs = MAX_HTTP_TCP_KEEPALIVE_SECS + 1;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("http_tcp_keepalive_secs"));
    }

    #[test]
    fn accepts_zero_http_tcp_keepalive_secs() {
        let mut cfg = config_with(vec![valid_contract()]);
        cfg.http_tcp_keepalive_secs = 0;
        assert!(cfg.validate().is_ok());
    }

    // ── #95: contract labels ─────────────────────────────────────────────────

    #[test]
    fn rejects_label_with_newline() {
        let mut c = valid_contract();
        c.label = "Vault\nINFO forged log line".into();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("control characters"), "got: {}", err);
        assert!(
            !err.contains('\n'),
            "error must not echo the raw newline: {}",
            err
        );
    }

    #[test]
    fn rejects_label_with_ansi_escape() {
        let mut c = valid_contract();
        c.label = "\u{1b}[31mVault\u{1b}[0m".into();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("control characters"), "got: {}", err);
        assert!(
            !err.contains('\u{1b}'),
            "error must not echo the raw escape: {}",
            err
        );
    }

    #[test]
    fn rejects_label_longer_than_max() {
        let mut c = valid_contract();
        c.label = "a".repeat(MAX_LABEL_LEN + 1);
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("longer than 128 characters"), "got: {}", err);
    }

    #[test]
    fn accepts_label_of_max_length() {
        let mut c = valid_contract();
        c.label = "é".repeat(MAX_LABEL_LEN);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn label_is_trimmed() {
        let mut c = valid_contract();
        c.label = "  Vault \t".into();
        c.validate().unwrap();
        assert_eq!(c.label, "Vault");
    }

    #[test]
    fn rejects_duplicate_labels_differing_in_case_and_whitespace() {
        let mut a = valid_contract();
        a.label = "Vault".into();
        let mut b = valid_contract();
        b.label = "vault ".into();
        let err = config_with(vec![a, b]).validate().unwrap_err();
        assert!(err.to_string().contains("duplicate contract label"));
    }

    // ── #94: Soroban function names ──────────────────────────────────────────

    #[test]
    fn rejects_function_name_longer_than_32_chars() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::FunctionCalled {
            function_name: "a".repeat(33),
        }];
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("contract 'Test'"), "got: {}", err);
        assert!(err.contains("FunctionCalled"), "got: {}", err);
        assert!(
            err.contains("at most 32 characters from [a-zA-Z0-9_]"),
            "got: {}",
            err
        );
    }

    #[test]
    fn accepts_function_name_of_32_chars() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::FunctionCalled {
            function_name: "a".repeat(32),
        }];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_function_name_with_invalid_characters() {
        for name in ["with-draw", "with draw", "withdraw()", "wïthdraw"] {
            let mut c = valid_contract();
            c.rules = vec![AlertRule::FunctionCalled {
                function_name: name.into(),
            }];
            let err = c.validate().unwrap_err().to_string();
            assert!(
                err.contains("not a valid Soroban symbol"),
                "{}: {}",
                name,
                err
            );
        }
    }

    #[test]
    fn rejects_function_name_with_surrounding_whitespace() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::FunctionCalled {
            function_name: "withdraw ".into(),
        }];
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("\"withdraw \""), "got: {}", err);
    }

    #[test]
    fn rejects_invalid_admin_function_name() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec!["set_admin".into(), " upgrade".into()],
        }];
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("AdminFunctionCalled"), "got: {}", err);
        assert!(err.contains("not a valid Soroban symbol"), "got: {}", err);
    }

    #[test]
    fn from_file_returns_err_for_missing_file() {
        let nonexistent_path = std::path::Path::new("/tmp/txwatch_nonexistent_test_config.toml");
        let result = AppConfig::from_file(nonexistent_path);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("txwatch_nonexistent_test_config.toml"));
    }

    #[test]
    fn from_file_returns_err_for_wrong_type_field() {
        let path = std::env::temp_dir().join("txwatch_wrong_type_field_test_config.toml");
        let raw = r#"
            poll_interval_seconds = "ten"
            [[contracts]]
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            [[contracts.rules]]
            type = "AnyTransaction"
        "#;

        std::fs::write(&path, raw).unwrap();
        let result = AppConfig::from_file(&path);
        let _ = std::fs::remove_file(&path);

        assert!(result.is_err());
        // `{:#}` renders the whole anyhow chain; the field path lives on the
        // source error, not on the outer context.
        let error_msg = format!("{:#}", result.unwrap_err());
        assert!(error_msg.contains("failed to parse config file"));
        assert!(
            error_msg.contains("field: poll_interval_seconds"),
            "error should name the offending field, got: {}",
            error_msg
        );
    }

    // ── Webhook destinations, formats and headers ────────────────────────────

    /// Parses and validates `contract_body` as the only `[[contracts]]` entry
    /// (label "x", one AnyTransaction rule).
    fn parse_contract(contract_body: &str) -> Result<AppConfig> {
        let raw = format!(
            r#"
            [[contracts]]
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            {}
            [[contracts.rules]]
            type = "AnyTransaction"
            "#,
            contract_body
        );
        AppConfig::parse(&raw, Path::new("webhooks.toml"))
    }

    fn parse_err(contract_body: &str) -> String {
        format!("{:#}", parse_contract(contract_body).unwrap_err())
    }

    #[test]
    fn webhook_url_shorthand_is_one_txwatch_destination() {
        let cfg = parse_contract(r#"webhook_url = "https://example.com/hook""#).unwrap();
        let destinations = cfg.contracts[0].destinations();
        assert_eq!(destinations.len(), 1);
        assert_eq!(destinations[0].url, "https://example.com/hook");
        assert_eq!(destinations[0].format, WebhookFormat::Txwatch);
        assert!(destinations[0].headers.is_empty());
    }

    #[test]
    fn webhooks_array_combines_with_the_shorthand() {
        let cfg = parse_contract(
            r#"
            webhook_url = "https://internal.example.com/hook"
            [[contracts.webhooks]]
            url = "https://hooks.slack.com/services/T/B/X"
            format = "slack"
            [[contracts.webhooks]]
            url = "https://events.pagerduty.com/v2/enqueue"
            format = "pagerduty"
            routing_key = "R0UT1NG"
            "#,
        )
        .unwrap();
        let destinations = cfg.contracts[0].destinations();
        let summary: Vec<(&str, WebhookFormat)> = destinations
            .iter()
            .map(|d| (d.url.as_str(), d.format))
            .collect();
        assert_eq!(
            summary,
            [
                ("https://internal.example.com/hook", WebhookFormat::Txwatch),
                ("https://hooks.slack.com/services/T/B/X", WebhookFormat::Slack),
                ("https://events.pagerduty.com/v2/enqueue", WebhookFormat::Pagerduty),
            ]
        );
    }

    #[test]
    fn webhooks_array_alone_is_enough() {
        let cfg = parse_contract(
            r#"
            [[contracts.webhooks]]
            url = "https://discord.com/api/webhooks/1/abc"
            format = "discord"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.contracts[0].webhook_url, None);
        assert_eq!(cfg.contracts[0].destinations().len(), 1);
    }

    #[test]
    fn a_contract_needs_at_least_one_destination() {
        let err = parse_err("");
        assert!(err.contains("no webhook destination"), "got: {}", err);
    }

    #[test]
    fn shorthand_fields_without_webhook_url_are_rejected() {
        let err = parse_err(
            r#"
            webhook_format = "slack"
            [[contracts.webhooks]]
            url = "https://example.com/hook"
            "#,
        );
        assert!(err.contains("webhook_format set without webhook_url"), "got: {}", err);
    }

    #[test]
    fn unknown_format_and_unknown_destination_fields_are_rejected() {
        let err = parse_err(
            r#"webhook_url = "https://example.com/hook"
            webhook_format = "teams""#,
        );
        assert!(err.contains("unknown variant `teams`"), "got: {}", err);

        let err = parse_err(
            r#"
            [[contracts.webhooks]]
            url = "https://example.com/hook"
            secrett = "x"
            "#,
        );
        assert!(err.contains("unknown field `secrett`"), "got: {}", err);
    }

    #[test]
    fn destination_errors_name_the_field() {
        let err = parse_err(
            r#"
            [[contracts.webhooks]]
            url = "https://example.com/ok"
            [[contracts.webhooks]]
            url = "ftp://example.com/bad"
            "#,
        );
        assert!(
            err.contains("contract 'x': webhooks[1].url 'ftp://example.com/bad' must use http or https"),
            "got: {}",
            err
        );
    }

    #[test]
    fn pagerduty_requires_a_routing_key_and_others_reject_one() {
        let err = parse_err(
            r#"webhook_url = "https://events.pagerduty.com/v2/enqueue"
            webhook_format = "pagerduty""#,
        );
        assert!(
            err.contains("webhook_routing_key is required when format is \"pagerduty\""),
            "got: {}",
            err
        );

        let err = parse_err(
            r#"
            [[contracts.webhooks]]
            url = "https://example.com/hook"
            routing_key = "R0UT1NG"
            "#,
        );
        assert!(
            err.contains("webhooks[0].routing_key is only used when format is \"pagerduty\""),
            "got: {}",
            err
        );
    }

    #[test]
    fn custom_headers_are_accepted() {
        let cfg = parse_contract(
            r#"webhook_url = "https://example.com/hook"
            webhook_headers = { "Authorization" = "Bearer abc", "X-Api-Key" = "k" }"#,
        )
        .unwrap();
        let headers: Vec<(&str, &str)> = cfg.contracts[0].webhook_headers.iter().collect();
        assert_eq!(headers, [("Authorization", "Bearer abc"), ("X-Api-Key", "k")]);
    }

    #[test]
    fn reserved_headers_are_rejected() {
        for name in ["Content-Type", "content-length", "X-TxWatch-Secret", "x-txwatch-anything"] {
            let err = parse_err(&format!(
                r#"webhook_url = "https://example.com/hook"
                webhook_headers = {{ "{}" = "v" }}"#,
                name
            ));
            assert!(err.contains("is reserved"), "{}: {}", name, err);
            assert!(err.contains("webhook_headers"), "{}: {}", name, err);
        }
    }

    #[test]
    fn invalid_header_names_and_values_are_rejected_without_echoing_values() {
        let err = parse_err(
            r#"
            [[contracts.webhooks]]
            url = "https://example.com/hook"
            headers = { "Bad Header" = "v", "X-Ok" = "line1\r\nInjected: secret-value" }
            "#,
        );
        assert!(
            err.contains("webhooks[0].headers \"Bad Header\" is not a valid HTTP header name"),
            "got: {}",
            err
        );
        assert!(
            err.contains("webhooks[0].headers value of \"X-Ok\" contains control characters"),
            "got: {}",
            err
        );
        assert!(!err.contains("secret-value"), "header value leaked: {}", err);
    }

    #[test]
    fn debug_output_redacts_header_values_secrets_and_routing_keys() {
        let destination = WebhookDestination {
            url: "https://example.com/hook".into(),
            secret: Some("s3cret".into()),
            format: WebhookFormat::Pagerduty,
            headers: WebhookHeaders(BTreeMap::from([(
                "Authorization".to_owned(),
                "Bearer t0ken".to_owned(),
            )])),
            routing_key: Some("R0UT1NG".into()),
        };
        let debug = format!("{:?}", destination);
        for secret in ["s3cret", "t0ken", "R0UT1NG"] {
            assert!(!debug.contains(secret), "{} leaked: {}", secret, debug);
        }
        assert!(debug.contains("Authorization"), "{}", debug);
        assert_eq!(
            destination.headers.redacted(),
            ["Authorization: <redacted>"]
        );
    }

    // ── Env-var interpolation ────────────────────────────────────────────────

    fn lookup(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("t0ken".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn interpolation_supports_embedded_defaults_and_escapes() {
        let interp = |v: &str| interpolate(v, &lookup);
        assert_eq!(interp("${TOKEN}").unwrap(), "t0ken");
        assert_eq!(interp("Bearer ${TOKEN}").unwrap(), "Bearer t0ken");
        assert_eq!(interp("${MISSING:-fallback}").unwrap(), "fallback");
        assert_eq!(interp("${EMPTY:-fallback}").unwrap(), "fallback");
        assert_eq!(interp("$${TOKEN}").unwrap(), "${TOKEN}");
        assert_eq!(interp("cost: $5").unwrap(), "cost: $5");
        assert!(interp("${MISSING}").unwrap_err().to_string().contains("'MISSING'"));
        assert!(interp("${}").is_err());
        assert!(interp("${TOKEN").is_err());
        assert!(interp("${1X}").is_err());
    }

    /// Serialises tests that mutate the process environment.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn headers_secrets_and_routing_keys_are_interpolated() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        env::set_var("TXWATCH_TEST_BEARER", "t0ken");
        env::set_var("TXWATCH_TEST_PD_KEY", "R0UT1NG");
        let cfg = parse_contract(
            r#"webhook_url = "https://example.com/hook"
            webhook_headers = { "Authorization" = "Bearer ${TXWATCH_TEST_BEARER}" }
            [[contracts.webhooks]]
            url = "https://events.pagerduty.com/v2/enqueue"
            format = "pagerduty"
            routing_key = "${TXWATCH_TEST_PD_KEY}"
            secret = "prefix-${TXWATCH_TEST_BEARER}"
            "#,
        );
        env::remove_var("TXWATCH_TEST_BEARER");
        env::remove_var("TXWATCH_TEST_PD_KEY");
        let cfg = cfg.unwrap();
        let destinations = cfg.contracts[0].destinations();
        assert_eq!(
            destinations[0].headers.iter().collect::<Vec<_>>(),
            [("Authorization", "Bearer t0ken")]
        );
        assert_eq!(destinations[1].routing_key.as_deref(), Some("R0UT1NG"));
        assert_eq!(destinations[1].secret.as_deref(), Some("prefix-t0ken"));
    }

    #[test]
    fn missing_header_variable_names_the_header_not_the_value() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        env::remove_var("TXWATCH_TEST_UNSET");
        let err = parse_err(
            r#"webhook_url = "https://example.com/hook"
            webhook_headers = { "Authorization" = "Bearer ${TXWATCH_TEST_UNSET}" }"#,
        );
        assert!(
            err.contains("contracts[0].webhook_headers.Authorization"),
            "got: {}",
            err
        );
        assert!(err.contains("TXWATCH_TEST_UNSET"), "got: {}", err);
    }
}
