#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use std::{env, fmt, fs, path::Path};
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

// ── WatchedContract ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchedContract {
    pub label: String,
    pub contract_id: String,
    pub network: Network,
    pub rules: Vec<AlertRule>,
    pub webhook_url: String,
    /// Optional secret sent as X-TxWatch-Secret header on every webhook POST.
    /// Supports `${ENV_VAR}` interpolation (e.g. `webhook_secret = "${MY_SECRET}"`).
    pub webhook_secret: Option<String>,
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

// ── Contract StrKey ───────────────────────────────────────────────────────────

/// Length of a contract StrKey: base32 of 1 version byte + 32 payload bytes +
/// 2 checksum bytes (35 bytes = 280 bits = 56 base32 characters, no padding).
const CONTRACT_STRKEY_LEN: usize = 56;

/// StrKey version byte for contract addresses (`2 << 3`), which encodes to a
/// leading 'C'.
const CONTRACT_STRKEY_VERSION: u8 = 2 << 3;

/// Why a string is not a valid Stellar contract StrKey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractIdError {
    /// Not exactly 56 characters long.
    Length(usize),
    /// Contains a character outside the base32 alphabet `A–Z2–7`
    /// (lowercase letters, `0`, `1`, `8` and `9` are the usual culprits).
    Alphabet { position: usize, found: char },
    /// Decodes, but the version byte is not the contract version ('C…').
    VersionByte(u8),
    /// Decodes, but the CRC16-XModem checksum does not match — usually a
    /// copy-paste or typing error.
    Checksum { expected: u16, found: u16 },
}

impl fmt::Display for ContractIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContractIdError::Length(len) => write!(
                f,
                "must be {} characters, got {}",
                CONTRACT_STRKEY_LEN, len
            ),
            ContractIdError::Alphabet { position, found } => write!(
                f,
                "invalid character {:?} at position {} (only A-Z and 2-7 are allowed)",
                found, position
            ),
            ContractIdError::VersionByte(byte) => write!(
                f,
                "wrong version byte 0x{:02x} (contract addresses start with 'C')",
                byte
            ),
            ContractIdError::Checksum { expected, found } => write!(
                f,
                "checksum mismatch (expected 0x{:04x}, found 0x{:04x}); \
                 the address is probably mistyped",
                expected, found
            ),
        }
    }
}

impl std::error::Error for ContractIdError {}

/// CRC16-XModem (poly 0x1021, init 0), as used by StrKey.
fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Decodes and checks a contract StrKey (`C…`): length, base32 alphabet,
/// version byte and CRC16-XModem checksum. Returns the 32-byte contract hash.
pub fn validate_contract_id(id: &str) -> std::result::Result<[u8; 32], ContractIdError> {
    let len = id.chars().count();
    if len != CONTRACT_STRKEY_LEN {
        return Err(ContractIdError::Length(len));
    }

    let mut bytes = [0u8; 35];
    let mut buffer: u32 = 0;
    let mut bits = 0;
    let mut out = 0;
    for (position, c) in id.chars().enumerate() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            '2'..='7' => c as u32 - '2' as u32 + 26,
            found => return Err(ContractIdError::Alphabet { position, found }),
        };
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes[out] = (buffer >> bits) as u8;
            out += 1;
        }
    }

    let (body, checksum) = bytes.split_at(33);
    if body[0] != CONTRACT_STRKEY_VERSION {
        return Err(ContractIdError::VersionByte(body[0]));
    }
    let expected = crc16_xmodem(body);
    let found = u16::from_le_bytes([checksum[0], checksum[1]]);
    if expected != found {
        return Err(ContractIdError::Checksum { expected, found });
    }

    let mut payload = [0u8; 32];
    payload.copy_from_slice(&body[1..]);
    Ok(payload)
}

impl WatchedContract {
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

        if let Some(problem) = check_http_url(&self.webhook_url) {
            errors.push(format!(
                "contract '{}': webhook_url {}",
                self.label, problem
            ));
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

/// Default maximum number of watched contracts in a single configuration.
/// Every contract is polled by its own task, so an unbounded list could
/// exhaust memory, file descriptors or the Horizon rate limit. Raise it with
/// the top-level `max_contracts` setting.
pub const MAX_CONTRACTS: usize = 100;

/// Upper bound for the `max_contracts` override, for operators running their
/// own Horizon instance.
pub const MAX_CONTRACTS_CEILING: usize = 10_000;

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
    /// Maximum number of `[[contracts]]` entries (1–10000). Default: 100.
    /// Raise it only when the Horizon instance (typically your own) can take
    /// the extra polling load.
    #[serde(default)]
    pub max_contracts: Option<usize>,
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

/// Resolves a `${VAR_NAME}` reference to the corresponding environment variable.
/// Values that don't match the `${...}` pattern are returned unchanged.
fn resolve_env_interpolation(value: &str) -> Result<String> {
    match value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        Some(var_name) => env::var(var_name)
            .with_context(|| format!("env var '{}' referenced in config is not set", var_name)),
        None => Ok(value.to_owned()),
    }
}

impl AppConfig {
    fn resolve_env_vars(&mut self) -> Result<()> {
        for contract in &mut self.contracts {
            if let Some(secret) = &contract.webhook_secret {
                contract.webhook_secret = Some(resolve_env_interpolation(secret)?);
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

    /// The contract limit in force: `max_contracts`, else [`MAX_CONTRACTS`].
    pub fn effective_max_contracts(&self) -> usize {
        self.max_contracts.unwrap_or(MAX_CONTRACTS)
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
        match self.max_contracts {
            Some(max) if max == 0 || max > MAX_CONTRACTS_CEILING => errors.push(format!(
                "max_contracts must be between 1 and {}",
                MAX_CONTRACTS_CEILING
            )),
            _ => {
                let max = self.effective_max_contracts();
                if self.contracts.len() > max {
                    errors.push(format!(
                        "{} contracts configured, more than the limit of {}; each contract is \
                         polled by its own task. Split the config across several TxWatch \
                         instances, or raise max_contracts (up to {}) if your Horizon can \
                         handle the load",
                        self.contracts.len(),
                        max,
                        MAX_CONTRACTS_CEILING
                    ));
                }
            }
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
            webhook_url: "https://example.com/hook".into(),
            webhook_secret: None,
            poll_interval_seconds: None,
            horizon_base_url_override: None,
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
        c.webhook_url = "ftp://bad".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_no_host() {
        let mut c = valid_contract();
        c.webhook_url = "https://".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_spaces() {
        let mut c = valid_contract();
        c.webhook_url = "https://example .com/hook".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_that_is_not_a_url() {
        let mut c = valid_contract();
        c.webhook_url = "not-a-url-at-all".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_ftp_scheme() {
        let mut c = valid_contract();
        c.webhook_url = "ftp://files.example.com/hook".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn accepts_valid_http_webhook_url() {
        let mut c = valid_contract();
        c.webhook_url = "http://hooks.example.com/my-webhook".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn accepts_valid_https_webhook_url_with_path_and_query() {
        let mut c = valid_contract();
        c.webhook_url = "https://hooks.example.com/alerts?token=abc123".into();
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
        bad_id.webhook_url = "ftp://bad".into();
        let mut bad_rule = valid_contract();
        bad_rule.label = "B".into();
        bad_rule.rules = vec![AlertRule::LargeTransfer { threshold_xlm: 0 }];
        let mut cfg = AppConfig {
            poll_interval_seconds: 1,
            contracts: vec![bad_id, bad_rule, valid_contract(), valid_contract()],
            http_pool_max_idle_per_host: DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST,
            http_tcp_keepalive_secs: DEFAULT_HTTP_TCP_KEEPALIVE_SECS,
            http_connection_verbose: None,
            max_contracts: None,
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
            max_contracts: None,
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
            max_contracts: None,
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
                max_contracts: None,
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
            max_contracts: None,
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

    // ── Contract limit ───────────────────────────────────────────────────────

    /// `n` contracts with unique labels.
    fn contracts(n: usize) -> Vec<WatchedContract> {
        (0..n)
            .map(|i| {
                let mut c = valid_contract();
                c.label = format!("c{}", i);
                c
            })
            .collect()
    }

    #[test]
    fn accepts_exactly_max_contracts() {
        let mut cfg = config_with(contracts(MAX_CONTRACTS));
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_more_than_max_contracts() {
        let mut cfg = config_with(contracts(MAX_CONTRACTS + 1));
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("101 contracts configured, more than the limit of 100"),
            "got: {}",
            err
        );
        assert!(err.contains("max_contracts"), "got: {}", err);
    }

    #[test]
    fn max_contracts_override_raises_and_lowers_the_limit() {
        let mut cfg = config_with(contracts(150));
        cfg.max_contracts = Some(150);
        cfg.validate().unwrap();

        let mut cfg = config_with(contracts(3));
        cfg.max_contracts = Some(2);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("more than the limit of 2"), "got: {}", err);
    }

    #[test]
    fn max_contracts_override_must_be_in_range() {
        for max in [0, MAX_CONTRACTS_CEILING + 1] {
            let mut cfg = config_with(contracts(1));
            cfg.max_contracts = Some(max);
            let err = cfg.validate().unwrap_err().to_string();
            assert!(
                err.contains("max_contracts must be between 1 and 10000"),
                "max={}: {}",
                max,
                err
            );
        }
    }

    #[test]
    fn max_contracts_parses_from_toml() {
        let mut cfg: AppConfig =
            toml::from_str(&format!("max_contracts = 500\n{}", MINIMAL_TOML)).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.effective_max_contracts(), 500);
        let default: AppConfig = toml::from_str(MINIMAL_TOML).unwrap();
        assert_eq!(default.effective_max_contracts(), MAX_CONTRACTS);
    }

    // ── Contract StrKey ──────────────────────────────────────────────────────

    #[test]
    fn validate_contract_id_accepts_real_contract_ids() {
        // Native XLM Stellar Asset Contract on testnet and mainnet.
        for id in [
            "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC",
            "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA",
        ] {
            assert!(validate_contract_id(id).is_ok(), "{}", id);
        }
    }

    #[test]
    fn validate_contract_id_reports_each_failure_distinctly() {
        let valid = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
        assert_eq!(
            validate_contract_id(&valid[..55]),
            Err(ContractIdError::Length(55))
        );
        assert_eq!(
            validate_contract_id("CTEST000000000000000000000000000000000000000000000000000"),
            Err(ContractIdError::Alphabet { position: 5, found: '0' })
        );
        assert_eq!(
            validate_contract_id("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"),
            Err(ContractIdError::VersionByte(6 << 3))
        );
        let mistyped = valid.replacen("LZ", "LY", 1);
        assert!(matches!(
            validate_contract_id(&mistyped),
            Err(ContractIdError::Checksum { .. })
        ));
    }
}
