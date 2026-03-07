use anyhow::Result;
use ethers::prelude::*;
use ethers::utils::format_units;
use std::env;
use std::sync::Arc;
use std::time::Instant;

const NATIVE_USDC: &str = "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359";
const BRIDGED_USDC: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

const DEFAULT_RPC_URLS: &[&str] = &[
    "https://polygon-rpc.com",
    "https://polygon.llamarpc.com",
    "https://polygon-bor-rpc.publicnode.com",
    "https://rpc.ankr.com/polygon",
    "https://polygon.drpc.org",
    "https://polygon.meowrpc.com",
    "https://1rpc.io/matic",
    "https://polygon-mainnet.public.blastapi.io",
];

abigen!(
    IERC20,
    r#"[
        function balanceOf(address account) external view returns (uint256)
    ]"#,
);

pub struct Governor {
    pub initial_balance: f64,
    pub current_balance: f64,
    pub initial_api_credit: f64,
    pub api_costs: f64,
    pub realized_profit: f64,
    pub start_time: Instant,
    pub wallet_address: Address,
    pub proxy_address: Address,
    rpc_urls: Vec<String>,
    last_working_idx: usize,
}

impl Governor {
    pub fn new(initial_balance: f64, wallet_address: Address, proxy_address: Address) -> Self {
        let initial_api_credit = env::var("INITIAL_API_CREDIT")
            .unwrap_or_else(|_| "0.0".to_string())
            .parse::<f64>()
            .unwrap_or(0.0);

        let mut rpc_urls: Vec<String> = Vec::new();

        // env override goes first
        if let Ok(env_url) = env::var("POLYGON_RPC_URL") {
            let trimmed = env_url.trim().to_string();
            if !trimmed.is_empty() {
                rpc_urls.push(trimmed);
            }
        }

        for url in DEFAULT_RPC_URLS {
            rpc_urls.push(url.to_string());
        }

        Self {
            initial_balance,
            current_balance: initial_balance,
            initial_api_credit,
            api_costs: 0.0,
            realized_profit: 0.0,
            start_time: Instant::now(),
            wallet_address,
            proxy_address,
            rpc_urls,
            last_working_idx: 0,
        }
    }

    /// Check the PROXY wallet's USDC balance (both native + bridged).
    /// Polymarket trades debit/credit the proxy, not the EOA.
    pub async fn fetch_real_balance(&mut self) -> Result<f64> {
        let native_addr: Address = NATIVE_USDC.parse()?;
        let bridged_addr: Address = BRIDGED_USDC.parse()?;

        let total = self.rpc_urls.len();
        for attempt in 0..total {
            let idx = (self.last_working_idx + attempt) % total;
            let rpc_url = &self.rpc_urls[idx];

            let provider = match Provider::<Http>::try_from(rpc_url.as_str()) {
                Ok(p) => Arc::new(p),
                Err(_) => continue,
            };

            // Check PROXY wallet (where Polymarket trades happen)
            let native_contract = IERC20::new(native_addr, provider.clone());
            let bridged_contract = IERC20::new(bridged_addr, provider);

            let native_call = native_contract.balance_of(self.proxy_address);
            let bridged_call = bridged_contract.balance_of(self.proxy_address);

            let (native_res, bridged_res) = tokio::join!(native_call.call(), bridged_call.call(),);

            match (native_res, bridged_res) {
                (Ok(native_bal), Ok(bridged_bal)) => {
                    let total_bal = native_bal + bridged_bal;
                    let balance_str = format_units(total_bal, 6)?;
                    let balance_f64 = balance_str.parse::<f64>().unwrap_or(0.0);

                    self.current_balance = balance_f64;
                    self.last_working_idx = idx;
                    return Ok(balance_f64);
                }
                _ => {
                    println!("⚠️ RPC failed: {} (trying next...)", rpc_url);
                    continue;
                }
            }
        }

        Err(anyhow::anyhow!("All {} RPC endpoints failed", total))
    }

    pub fn track_api_cost(&mut self, cost: f64) {
        self.api_costs += cost;
    }

    pub fn set_realized_profit(&mut self, realized_profit: f64) {
        self.realized_profit = realized_profit;
    }

    fn api_credit_from_profit(&self) -> f64 {
        self.realized_profit.max(0.0)
    }

    pub fn remaining_api_credit(&self) -> f64 {
        (self.initial_api_credit + self.api_credit_from_profit() - self.api_costs).max(0.0)
    }

    pub fn survival_stats(&self) -> String {
        let uptime = self.start_time.elapsed().as_secs();
        let profit_loss = self.current_balance - self.initial_balance;
        format!(
            "Wallet: `0x{:x}` | Bal: ${:.2} ({:+.2}) | API Left: ${:.4} | API Spent: ${:.4} | Realized: {:+.2} | Up: {}s",
            self.wallet_address,
            self.current_balance,
            profit_loss,
            self.remaining_api_credit(),
            self.api_costs,
            self.realized_profit,
            uptime
        )
    }
}
