use anyhow::Result;
use base64::{engine::general_purpose, Engine as _};
use ethers::abi::{encode, Token};
use ethers::prelude::*;
use ethers::utils::keccak256;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use sha2::Sha256;
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

fn ctf_exchange() -> String {
    std::env::var("CTF_EXCHANGE").unwrap_or_else(|_| "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E".to_string())
}
fn neg_risk_ctf_exchange() -> String {
    std::env::var("NEG_RISK_CTF_EXCHANGE").unwrap_or_else(|_| "0xC5d563A36AE78145C45a50134d48A1215220f80a".to_string())
}
static ORDER_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct Trader {
    client: Client,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    base_url: String,
    signer: LocalWallet,
    proxy_address: Address, // Polymarket profile/proxy wallet
}

#[derive(Deserialize, Debug)]
pub struct OrderResponse {
    #[serde(alias = "orderID")]
    pub order_id: Option<String>,
    pub status: Option<String>,
    #[serde(alias = "errorMsg")]
    pub error: Option<String>,
}

struct OrderData {
    salt: U256,
    maker: Address,
    signer_addr: Address,
    taker: Address,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    expiration: U256,
    nonce: U256,
    fee_rate_bps: U256,
    side: u8,
    signature_type: u8,
}

/// EIP-712 domain separator: keccak256(abi.encode(typeHash, nameHash, versionHash, chainId, verifyingContract))
fn compute_domain_separator(verifying_contract: Address) -> [u8; 32] {
    let domain_type_hash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let name_hash = keccak256(b"Polymarket CTF Exchange");
    let version_hash = keccak256(b"1");

    let encoded = encode(&[
        Token::FixedBytes(domain_type_hash.to_vec()),
        Token::FixedBytes(name_hash.to_vec()),
        Token::FixedBytes(version_hash.to_vec()),
        Token::Uint(U256::from(137)),
        Token::Address(verifying_contract),
    ]);

    keccak256(&encoded)
}

/// EIP-712 struct hash for Order
/// Type: Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,
///             uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,
///             uint256 feeRateBps,uint8 side,uint8 signatureType)
fn compute_order_struct_hash(order: &OrderData) -> [u8; 32] {
    let type_hash = keccak256(
        b"Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)"
    );

    let encoded = encode(&[
        Token::FixedBytes(type_hash.to_vec()),
        Token::Uint(order.salt),
        Token::Address(order.maker),
        Token::Address(order.signer_addr),
        Token::Address(order.taker),
        Token::Uint(order.token_id),
        Token::Uint(order.maker_amount),
        Token::Uint(order.taker_amount),
        Token::Uint(order.expiration),
        Token::Uint(order.nonce),
        Token::Uint(order.fee_rate_bps),
        Token::Uint(U256::from(order.side)),
        Token::Uint(U256::from(order.signature_type)),
    ]);

    keccak256(&encoded)
}

/// Sign an order using EIP-712: keccak256("\x19\x01" || domainSeparator || structHash)
fn sign_order(
    wallet: &LocalWallet,
    order: &OrderData,
    exchange_address: Address,
) -> Result<String> {
    let domain_separator = compute_domain_separator(exchange_address);
    let struct_hash = compute_order_struct_hash(order);

    let mut msg = vec![0x19u8, 0x01u8];
    msg.extend_from_slice(&domain_separator);
    msg.extend_from_slice(&struct_hash);
    let digest = keccak256(&msg);

    let signature = wallet.sign_hash(H256::from(digest))?;
    Ok(format!("0x{}", signature))
}

impl Trader {
    pub fn new() -> Result<Self> {
        let clean_env = |key: &str| -> Result<String> {
            let raw = env::var(key)?.trim().replace("\r", "");
            let stripped = raw.strip_prefix("\"").unwrap_or(&raw);
            let stripped = stripped.strip_suffix("\"").unwrap_or(stripped);
            Ok(stripped.to_string())
        };

        let api_key = clean_env("POLYMARKET_API_KEY")?;
        let api_secret = clean_env("POLYMARKET_SECRET")?;
        let api_passphrase = clean_env("POLYMARKET_PASSPHRASE")?;
        let private_key = clean_env("PRIVATE_KEY")?;
        let proxy_addr_str = clean_env("POLYMARKET_PROXY_ADDRESS")?;

        let signer = private_key.parse::<LocalWallet>()?.with_chain_id(137u64);
        let proxy_address: Address = proxy_addr_str.parse()?;

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| Client::new());

        Ok(Self {
            client,
            api_key,
            api_secret,
            api_passphrase,
            base_url: "https://clob.polymarket.com".to_string(),
            signer,
            proxy_address,
        })
    }

    fn generate_auth_signature(
        &self,
        timestamp: u64,
        method: &str,
        request_path: &str,
        body: &str,
    ) -> Result<String> {
        let mut message = format!("{}{}{}", timestamp, method, request_path);
        if !body.is_empty() {
            message.push_str(body);
        }

        let mut secret = self.api_secret.clone();
        if secret.len() % 4 != 0 {
            secret += &"=".repeat(4 - (secret.len() % 4));
        }

        let secret_bytes = general_purpose::URL_SAFE.decode(&secret)?;
        let mut mac = HmacSha256::new_from_slice(&secret_bytes)?;
        mac.update(message.as_bytes());
        let result = mac.finalize();
        Ok(general_purpose::URL_SAFE.encode(result.into_bytes()))
    }

    pub async fn place_market_order(
        &self,
        token_id_str: &str,
        side_str: &str,
        price: f64,
        amount_usdc: f64,
        neg_risk: bool,
    ) -> Result<OrderResponse> {
        let endpoint = "/order";
        let url = format!("{}{}", self.base_url, endpoint);

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let timestamp = now.as_secs();
        let salt_seq = ORDER_SEQ.fetch_add(1, Ordering::Relaxed) % 1000;
        let salt = (now.as_micros() as u64)
            .saturating_mul(1000)
            .saturating_add(salt_seq);

        let token_id = U256::from_dec_str(token_id_str)?;
        let side: u8 = if side_str.to_uppercase() == "BUY" {
            0
        } else {
            1
        };
        let side_string = if side == 0 { "BUY" } else { "SELL" };

        // FOK market order: allow 5% slippage
        let fill_price = if side == 0 {
            (price * 1.05).min(0.99)
        } else {
            (price * 0.95).max(0.01)
        };

        // Polymarket precision: amounts must be truncated to whole numbers (no extra decimals)
        // Both amounts are in 6-decimal token units but must be round numbers
        let (maker_amount, taker_amount) = if side == 0 {
            // BUY: makerAmount = USDC to pay, takerAmount = tokens to receive
            let ma_raw = (amount_usdc * 1_000_000.0) as u128;
            // maker: 2 decimal places = 10000 granularity
            let ma = (ma_raw / 10000) * 10000;
            let ta_raw = (amount_usdc / fill_price * 1_000_000.0) as u128;
            // taker: 4 decimal places = 100 granularity
            let ta = (ta_raw / 100) * 100;
            (U256::from(ma), U256::from(ta))
        } else {
            // SELL: makerAmount = tokens to send, takerAmount = USDC to receive
            let token_amount = amount_usdc / fill_price;
            let ma_raw = (token_amount * 1_000_000.0) as u128;
            // maker: 2 decimal accuracy = 10000 granularity
            let ma = (ma_raw / 10000) * 10000;
            let ta_raw = (token_amount * fill_price * 1_000_000.0) as u128;
            // taker: 4 decimal accuracy = 100 granularity
            let ta = (ta_raw / 100) * 100;
            (U256::from(ma), U256::from(ta))
        };

        let exchange_address: Address = if neg_risk {
            neg_risk_ctf_exchange().parse()?
        } else {
            ctf_exchange().parse()?
        };

        let signer_addr_cs = ethers::utils::to_checksum(&self.signer.address(), None);
        let funder_addr_cs = ethers::utils::to_checksum(&self.proxy_address, None);

        // signatureType=2 (browser wallet), maker=funder(proxy), signer=EOA
        let order_data = OrderData {
            salt: U256::from(salt),
            maker: self.proxy_address,          // funder/proxy wallet
            signer_addr: self.signer.address(), // EOA signer
            taker: Address::zero(),
            token_id,
            maker_amount,
            taker_amount,
            expiration: U256::zero(),
            nonce: U256::zero(),
            fee_rate_bps: U256::zero(),
            side,
            signature_type: 2, // browser wallet proxy
        };

        let sig_str = sign_order(&self.signer, &order_data, exchange_address)?;

        let body_json = serde_json::json!({
            "order": {
                "salt": salt,
                "maker": funder_addr_cs,
                "signer": signer_addr_cs,
                "taker": "0x0000000000000000000000000000000000000000",
                "tokenId": token_id_str,
                "makerAmount": maker_amount.to_string(),
                "takerAmount": taker_amount.to_string(),
                "expiration": "0",
                "nonce": "0",
                "feeRateBps": "0",
                "side": side_string,
                "signatureType": 2,
                "signature": sig_str,
            },
            "owner": self.api_key,
            "orderType": "FOK"
        });

        let body_str = body_json.to_string();
        let auth_sig = self.generate_auth_signature(timestamp, "POST", endpoint, &body_str)?;

        println!("DEBUG Order: {} {} @ {:.4} (fill_price={:.4}) | amount=${:.2} | maker={} taker={} | neg_risk={}",
            side_string, token_id_str, price, fill_price, amount_usdc, maker_amount, taker_amount, neg_risk);

        // Retry transient HTTP failures with exponential backoff
        let max_retries = 2u32;
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let backoff_ms = 500 * 2u64.pow(attempt - 1);
                println!(
                    "  ⏳ Order retry {}/{} after {}ms...",
                    attempt, max_retries, backoff_ms
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }

            let response = match self
                .client
                .post(&url)
                .header("POLY_ADDRESS", &signer_addr_cs)
                .header("POLY_SIGNATURE", &auth_sig)
                .header("POLY_TIMESTAMP", timestamp.to_string())
                .header("POLY_API_KEY", &self.api_key)
                .header("POLY_PASSPHRASE", &self.api_passphrase)
                .header("Content-Type", "application/json")
                .body(body_str.clone())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let kind = if e.is_timeout() {
                        "timeout"
                    } else if e.is_connect() {
                        "connection"
                    } else {
                        "network"
                    };
                    println!(
                        "  ⚠️ Order {} error (attempt {}/{}): {} | {} {} @ {:.4} ${:.2}",
                        kind, attempt + 1, max_retries + 1, e,
                        side_string, token_id_str, price, amount_usdc
                    );
                    last_err = Some(anyhow::anyhow!("Order {} error: {}", kind, e));
                    continue;
                }
            };

            let status = response.status();

            // Retry on server errors (5xx) or rate limiting (429)
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let text = response.text().await.unwrap_or_default();
                let truncated = &text[..text.len().min(200)];
                println!(
                    "  ⚠️ Order API transient error (attempt {}/{}): HTTP {} | {} {} @ {:.4} ${:.2} | {}",
                    attempt + 1, max_retries + 1, status,
                    side_string, token_id_str, price, amount_usdc, truncated
                );
                last_err = Some(anyhow::anyhow!("Order API Error ({}): {}", status, text));
                continue;
            }

            let text = match response.text().await {
                Ok(t) => t,
                Err(e) => {
                    println!(
                        "  ⚠️ Order response body read failed: {} | {} {} @ {:.4} ${:.2}",
                        e, side_string, token_id_str, price, amount_usdc
                    );
                    last_err = Some(anyhow::anyhow!("Order response body error: {}", e));
                    continue;
                }
            };

            if status.is_success() {
                println!("Order Response: {}", text);
                match serde_json::from_str::<OrderResponse>(&text) {
                    Ok(resp) => return Ok(resp),
                    Err(e) => {
                        println!(
                            "  ⚠️ Order response JSON parse failed: {} | body: {}",
                            e, &text[..text.len().min(300)]
                        );
                        return Ok(OrderResponse {
                            order_id: None,
                            status: Some("PARSE_ERROR".to_string()),
                            error: Some(format!("Response parse error: {} | raw: {}", e, &text[..text.len().min(200)])),
                        });
                    }
                }
            } else {
                // Non-retryable client error - return immediately with detailed context
                println!(
                    "  ❌ Order rejected: HTTP {} | {} {} @ {:.4} ${:.2} neg_risk={} | {}",
                    status, side_string, token_id_str, price, amount_usdc, neg_risk,
                    &text[..text.len().min(300)]
                );
                return Ok(OrderResponse {
                    order_id: None,
                    status: Some("FAILED".to_string()),
                    error: Some(format!("HTTP {}: {}", status, text)),
                });
            }
        }

        // All retries exhausted - log final failure summary
        let err_detail = last_err.as_ref().map(|e| e.to_string()).unwrap_or_default();
        println!(
            "  ❌ Order FAILED after {} retries: {} {} @ {:.4} ${:.2} neg_risk={} | {}",
            max_retries + 1, side_string, token_id_str, price, amount_usdc, neg_risk, err_detail
        );

        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("Order failed after {} retries", max_retries)
        }))
    }
}
