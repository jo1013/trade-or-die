use anyhow::Result;
use ethers::abi::{encode, Token};
use ethers::prelude::*;
use ethers::utils::keccak256;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

fn bridged_usdc_addr() -> String {
    std::env::var("BRIDGED_USDC").unwrap_or_else(|_| "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174".to_string())
}
fn native_usdc_addr() -> String {
    std::env::var("NATIVE_USDC").unwrap_or_else(|_| "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359".to_string())
}
fn uniswap_router_addr() -> String {
    std::env::var("UNISWAP_ROUTER").unwrap_or_else(|_| "0xE592427A0AEce92De3Edee1F18E0157C05861564".to_string())
}
fn redotpay_addr() -> String {
    std::env::var("REDOTPAY_ADDRESS").expect("REDOTPAY_ADDRESS must be set in .env")
}

abigen!(
    GnosisSafe,
    r#"[
        function nonce() external view returns (uint256)
        function getTransactionHash(address to, uint256 value, bytes data, uint8 operation, uint256 safeTxGas, uint256 baseGas, uint256 gasPrice, address gasToken, address refundReceiver, uint256 _nonce) external view returns (bytes32)
        function execTransaction(address to, uint256 value, bytes data, uint8 operation, uint256 safeTxGas, uint256 baseGas, uint256 gasPrice, address gasToken, address refundReceiver, bytes signatures) external payable returns (bool)
    ]"#
);

pub struct Recharger {
    signer: LocalWallet,
    proxy_address: Address,
    rpc_url: String,
    pub recharge_threshold: f64,
    pub recharge_amount: f64,
    pub cooldown_secs: u64,
    pub last_attempt: Option<std::time::Instant>,
}

impl Recharger {
    pub fn new(proxy_address: Address) -> Result<Self> {
        let pk_raw = std::env::var("PRIVATE_KEY")?;
        let pk = pk_raw.trim().trim_matches('"').replace('\r', "");
        let signer: LocalWallet = pk.parse::<LocalWallet>()?.with_chain_id(137u64);

        let rpc_url = std::env::var("POLYGON_RPC_URL")
            .unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".to_string());

        let recharge_threshold = std::env::var("API_RECHARGE_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5.0);
        let recharge_amount = std::env::var("RECHARGE_AMOUNT_USDC")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15.5);
        let cooldown_secs = std::env::var("RECHARGE_COOLDOWN_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(86400u64);

        Ok(Self {
            signer,
            proxy_address,
            rpc_url,
            recharge_threshold,
            recharge_amount,
            cooldown_secs,
            last_attempt: None,
        })
    }

    /// Check if API recharge is needed
    pub fn needs_recharge(&self, api_remaining: f64, balance: f64) -> bool {
        if api_remaining >= self.recharge_threshold {
            return false;
        }
        if let Some(last) = self.last_attempt {
            if last.elapsed().as_secs() < self.cooldown_secs {
                return false;
            }
        }
        // Keep at least $5 for trading after withdrawal
        balance >= self.recharge_amount + 5.0
    }

    fn make_client(&self) -> Result<Arc<SignerMiddleware<Provider<Http>, LocalWallet>>> {
        let provider = Provider::<Http>::try_from(self.rpc_url.as_str())?;
        let client = SignerMiddleware::new(provider, self.signer.clone());
        Ok(Arc::new(client))
    }

    /// Send a transaction from EOA to a contract with encoded calldata
    async fn send_eoa_tx(
        &self,
        client: &Arc<SignerMiddleware<Provider<Http>, LocalWallet>>,
        to: Address,
        data: Bytes,
        gas: u64,
        label: &str,
    ) -> Result<TransactionReceipt> {
        let gas_price = client.get_gas_price().await?;
        let tx = TransactionRequest::new()
            .to(to)
            .data(data)
            .gas(gas)
            .gas_price(gas_price * 130 / 100);

        let pending = client.send_transaction(tx, None).await?;
        let tx_hash = pending.tx_hash();
        info!(step = label, tx = format_args!("0x{:x}", tx_hash), "TX submitted");

        let receipt = pending
            .await?
            .ok_or_else(|| anyhow::anyhow!("No receipt for {}", label))?;

        if receipt.status == Some(U64::from(1)) {
            info!(step = label, "TX succeeded");
            Ok(receipt)
        } else {
            Err(anyhow::anyhow!("{} reverted", label))
        }
    }

    /// Step 1: Withdraw USDC.e from proxy to EOA via GnosisSafe
    async fn withdraw_to_eoa(
        &self,
        client: &Arc<SignerMiddleware<Provider<Http>, LocalWallet>>,
        amount_raw: U256,
    ) -> Result<String> {
        let usdc_addr: Address = bridged_usdc_addr().parse()?;
        let eoa = self.signer.address();

        let selector = &keccak256(b"transfer(address,uint256)")[..4];
        let args = encode(&[Token::Address(eoa), Token::Uint(amount_raw)]);
        let mut transfer_data = Vec::with_capacity(4 + args.len());
        transfer_data.extend_from_slice(selector);
        transfer_data.extend_from_slice(&args);
        let transfer_bytes: Bytes = transfer_data.into();

        let safe = GnosisSafe::new(self.proxy_address, client.clone());
        let safe_nonce = safe.nonce().call().await?;
        let zero = Address::zero();

        let safe_tx_hash: [u8; 32] = safe
            .get_transaction_hash(
                usdc_addr,
                U256::zero(),
                transfer_bytes.clone(),
                0u8,
                U256::zero(),
                U256::zero(),
                U256::zero(),
                zero,
                zero,
                safe_nonce,
            )
            .call()
            .await?;

        let signature = self.signer.sign_hash(H256::from(safe_tx_hash))?;
        let sig_bytes: Bytes = signature.to_vec().into();

        let exec_call = safe.exec_transaction(
            usdc_addr,
            U256::zero(),
            transfer_bytes,
            0u8,
            U256::zero(),
            U256::zero(),
            U256::zero(),
            zero,
            zero,
            sig_bytes,
        );
        let exec_with_gas = exec_call.gas(200_000u64);
        let pending = exec_with_gas.send().await?;
        let tx_hash = pending.tx_hash();
        let receipt = pending
            .await?
            .ok_or_else(|| anyhow::anyhow!("No receipt"))?;

        if receipt.status == Some(U64::from(1)) {
            let hash_str = format!("0x{:x}", tx_hash);
            info!(tx = %hash_str, "Proxy→EOA withdrawal succeeded");
            Ok(hash_str)
        } else {
            Err(anyhow::anyhow!("GnosisSafe execTransaction reverted"))
        }
    }

    /// Step 2: Approve Uniswap Router for USDC.e
    async fn approve_router(
        &self,
        client: &Arc<SignerMiddleware<Provider<Http>, LocalWallet>>,
        amount_raw: U256,
    ) -> Result<()> {
        let usdc_addr: Address = bridged_usdc_addr().parse()?;
        let router_addr: Address = uniswap_router_addr().parse()?;

        let selector = &keccak256(b"approve(address,uint256)")[..4];
        let args = encode(&[Token::Address(router_addr), Token::Uint(amount_raw)]);
        let mut data = Vec::with_capacity(4 + args.len());
        data.extend_from_slice(selector);
        data.extend_from_slice(&args);

        self.send_eoa_tx(client, usdc_addr, data.into(), 100_000, "Approve")
            .await?;
        Ok(())
    }

    /// Step 3: Swap USDC.e → Native USDC via Uniswap V3
    async fn swap_to_native_usdc(
        &self,
        client: &Arc<SignerMiddleware<Provider<Http>, LocalWallet>>,
        amount_raw: U256,
    ) -> Result<()> {
        let bridged: Address = bridged_usdc_addr().parse()?;
        let native: Address = native_usdc_addr().parse()?;
        let router: Address = uniswap_router_addr().parse()?;
        let eoa = self.signer.address();
        let min_out = amount_raw * 99 / 100;
        let deadline = U256::from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_secs()
                + 600,
        );

        let selector = &keccak256(
            b"exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))",
        )[..4];

        for fee in [100u32, 500u32] {
            let args = encode(&[Token::Tuple(vec![
                Token::Address(bridged),
                Token::Address(native),
                Token::Uint(U256::from(fee)),
                Token::Address(eoa),
                Token::Uint(deadline),
                Token::Uint(amount_raw),
                Token::Uint(min_out),
                Token::Uint(U256::zero()),
            ])]);
            let mut data = Vec::with_capacity(4 + args.len());
            data.extend_from_slice(selector);
            data.extend_from_slice(&args);

            let label = format!("Swap(fee={})", fee);
            match self
                .send_eoa_tx(client, router, data.into(), 300_000, &label)
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    warn!(fee, err = %e, "Swap fee tier failed");
                    if fee == 500 {
                        return Err(anyhow::anyhow!("All swap fee tiers failed"));
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
        unreachable!()
    }

    /// Step 4: Send all Native USDC in EOA to RedotPay
    async fn send_to_redotpay(
        &self,
        client: &Arc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    ) -> Result<(String, f64)> {
        let native_usdc: Address = native_usdc_addr().parse()?;
        let redotpay: Address = redotpay_addr().parse()?;
        let eoa = self.signer.address();

        // Check Native USDC balance in EOA
        let bal_selector = &keccak256(b"balanceOf(address)")[..4];
        let bal_args = encode(&[Token::Address(eoa)]);
        let mut bal_data = Vec::with_capacity(4 + bal_args.len());
        bal_data.extend_from_slice(bal_selector);
        bal_data.extend_from_slice(&bal_args);

        let bal_call = TransactionRequest::new()
            .to(native_usdc)
            .data(Bytes::from(bal_data));
        let result = client.call(&bal_call.into(), None).await?;
        let balance = U256::from_big_endian(&result);
        let balance_f64 = balance.as_u128() as f64 / 1_000_000.0;

        if balance.is_zero() {
            return Err(anyhow::anyhow!("No Native USDC after swap"));
        }

        // Transfer to RedotPay
        let selector = &keccak256(b"transfer(address,uint256)")[..4];
        let args = encode(&[Token::Address(redotpay), Token::Uint(balance)]);
        let mut data = Vec::with_capacity(4 + args.len());
        data.extend_from_slice(selector);
        data.extend_from_slice(&args);

        let receipt = self
            .send_eoa_tx(client, native_usdc, data.into(), 100_000, "→RedotPay")
            .await?;
        let tx_hash = format!("0x{:x}", receipt.transaction_hash);

        Ok((tx_hash, balance_f64))
    }

    /// Full pipeline: Proxy → EOA → Swap → RedotPay
    pub async fn full_recharge_pipeline(&mut self) -> Result<(String, f64)> {
        let client = self.make_client()?;
        // Add $0.50 extra to cover swap fees, so RedotPay gets >= recharge_amount
        let withdraw_raw =
            U256::from(((self.recharge_amount + 0.5) * 1_000_000.0) as u128);

        info!(amount = format_args!("${:.2}", self.recharge_amount), "Recharger: Starting pipeline (+$0.50 buffer)");

        // Step 1
        info!("[1/4] Withdraw USDC.e from Proxy → EOA");
        self.withdraw_to_eoa(&client, withdraw_raw).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Step 2
        info!("[2/4] Approve Uniswap Router");
        self.approve_router(&client, withdraw_raw).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Step 3
        info!("[3/4] Swap USDC.e → Native USDC");
        self.swap_to_native_usdc(&client, withdraw_raw).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Step 4
        info!("[4/4] Send Native USDC → RedotPay");
        let (tx_hash, amount_sent) = self.send_to_redotpay(&client).await?;

        self.last_attempt = Some(std::time::Instant::now());
        info!(amount = format_args!("${:.2}", amount_sent), tx = %tx_hash, "Recharger: pipeline complete");
        Ok((tx_hash, amount_sent))
    }
}
