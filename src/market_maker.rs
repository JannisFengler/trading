#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

use ethers::{
    signers::{LocalWallet, Signer},
    types::H160,
};
use gxhash::{HashMap, HashMapExt};
use log::{error, info};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::unbounded_channel;

use crate::{
    BaseUrl, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, ExchangeDataStatus,
    ExchangeResponseStatus, InfoClient, Message, Subscription, EPSILON,
};

// Parameters for z-score calculation
const WINDOW_SIZE: usize = 100; // rolling window size
const Z_THRESHOLD: f64 = 2.0;   // z-score threshold
const TRADE_SIZE: f64 = 0.001;  // size of each trade

pub struct Input {
    pub asset: String,
    pub target_liquidity: f64,
    pub half_spread: u16,
    pub max_bps_diff: u16,
    pub max_absolute_position_size: f64,
    pub decimals: u32,
    pub wallet: LocalWallet,
}

pub struct MarketMaker {
    pub asset: String,
    pub info_client: InfoClient,
    pub exchange_client: ExchangeClient,
    pub user_address: H160,
    // Shared reference to Binance price
    pub binance_price: Arc<Mutex<f64>>,

    // Rolling buffer of differences
    diffs: VecDeque<f64>,
    pub latest_mid_price: f64,
}

impl MarketMaker {
    /// # Errors
    ///
    /// Returns `Err` if the exchange or info clients can't be created.
    pub async fn new(input: Input) -> Result<Self, Box<dyn std::error::Error>> {
        let user_address = input.wallet.address();

        let info_client = InfoClient::new(None, Some(BaseUrl::Mainnet)).await?;
        let exchange_client =
            ExchangeClient::new(None, input.wallet, Some(BaseUrl::Mainnet), None, None).await?;

        Ok(Self {
            asset: input.asset,
            info_client,
            exchange_client,
            user_address,
            binance_price: Arc::new(Mutex::new(0.0)),
            diffs: VecDeque::with_capacity(WINDOW_SIZE),
            latest_mid_price: -1.0,
        })
    }

    pub async fn start(&mut self) {
        let (sender, mut receiver) = unbounded_channel();

        // Subscribe to UserEvents (fills)
        if let Err(e) = self
            .info_client
            .subscribe(
                Subscription::UserEvents {
                    user: self.user_address,
                },
                sender.clone(),
            )
            .await
        {
            error!("Error subscribing to UserEvents: {:?}", e);
            return;
        }

        // Subscribe to AllMids from Hyperliquid to get latest mid prices
        if let Err(e) = self.info_client.subscribe(Subscription::AllMids, sender).await {
            error!("Error subscribing to AllMids: {:?}", e);
            return;
        }

        // Main event loop
        while let Some(message) = receiver.recv().await {
            self.process_message(message).await;
        }
    }

    async fn process_message(&mut self, message: Message) {
        match message {
            Message::AllMids(all_mids) => {
                if let Some(mid_str) = all_mids.data.mids.get(&self.asset) {
                    if let Ok(mid) = mid_str.parse::<f64>() {
                        self.latest_mid_price = mid;
                        self.on_price_update().await;
                    } else {
                        error!("Invalid mid price format for asset {}: {:?}", self.asset, mid_str);
                    }
                } else {
                    error!("Could not get mid for asset {}: {:?}", self.asset, all_mids);
                }
            }
            Message::User(user_events) => {
                // Handle fills if needed. Currently, we do not store positions or PnL here, 
                // but you could log fills or track PnL.
                for fill in user_events.data.fills {
                    if fill.coin == self.asset {
                        let amount: f64 = fill.sz.parse().unwrap_or(0.0);
                        info!("Fill event: side={}, amount={}", fill.side, amount);
                    }
                }
            }
            _ => {
                // Other messages are ignored for now
            }
        }
    }

    async fn on_price_update(&mut self) {
        let hl_price = self.latest_mid_price;
        let binance_price = {
            let p = self.binance_price.lock().unwrap();
            *p
        };

        if binance_price.abs() < EPSILON {
            return; // can't compute relative diff if binance price is zero
        }

        let diff = (hl_price - binance_price) / binance_price;

        // Update rolling window
        if self.diffs.len() == WINDOW_SIZE {
            self.diffs.pop_front();
        }
        self.diffs.push_back(diff);

        if self.diffs.len() < WINDOW_SIZE {
            // Wait until we have a full window
            return;
        }

        // Compute mean and stddev
        let mean = self.mean();
        let stddev = self.stddev(mean);
        if stddev < EPSILON {
            return;
        }

        let z = (diff - mean) / stddev;
        if z > Z_THRESHOLD {
            // SELL Hyperliquid
            self.execute_immediate_trade(false, TRADE_SIZE).await;
        } else if z < -Z_THRESHOLD {
            // BUY Hyperliquid
            self.execute_immediate_trade(true, TRADE_SIZE).await;
        } else {
            // No trade
        }
    }

    fn mean(&self) -> f64 {
        let sum: f64 = self.diffs.iter().sum();
        sum / (self.diffs.len() as f64)
    }

    fn stddev(&self, mean: f64) -> f64 {
        let variance: f64 = self
            .diffs
            .iter()
            .map(|&x| {
                let d = x - mean;
                d * d
            })
            .sum::<f64>()
            / (self.diffs.len() as f64 - 1.0);
        variance.sqrt()
    }

    /// Execute a quick trade to capture the arbitrage opportunity.
    async fn execute_immediate_trade(&mut self, is_buy: bool, size: f64) {
        // We send a marketable limit order by offsetting from the mid price.
        // For a quick execution, pick an offset to cross the spread:
        let offset = if is_buy { 100.0 } else { -100.0 };
        let order_price = (self.latest_mid_price + offset).round();

        let (amount_filled, _) = self.place_order(self.asset.clone(), size, order_price, is_buy).await;
        if amount_filled > EPSILON {
            info!(
                "Executed immediate {} of {} at ~{:.2}",
                if is_buy { "buy" } else { "sell" },
                size,
                order_price
            );
        } else {
            error!("Failed to execute immediate trade, no fill received.");
        }
    }

    async fn place_order(
        &mut self,
        asset: String,
        amount: f64,
        price: f64,
        is_buy: bool,
    ) -> (f64, u64) {
        let order = self
            .exchange_client
            .order(
                ClientOrderRequest {
                    asset,
                    is_buy,
                    reduce_only: false,
                    limit_px: price,
                    sz: amount,
                    cloid: None,
                    order_type: ClientOrder::Limit(ClientLimit {
                        tif: "Ioc".to_string(), // Use Immediate-Or-Cancel to ensure quick fill
                    }),
                },
                None,
            )
            .await;

        match order {
            Ok(resp) => match resp {
                ExchangeResponseStatus::Ok(order_resp) => {
                    if let Some(order) = order_resp.data {
                        if !order.statuses.is_empty() {
                            match order.statuses[0].clone() {
                                ExchangeDataStatus::Filled(o) => {
                                    return (amount, o.oid);
                                }
                                ExchangeDataStatus::Resting(_o) => {
                                    // If it ended up resting, no immediate fill:
                                    return (0.0, 0);
                                }
                                ExchangeDataStatus::Error(e) => {
                                    error!("Order error: {e}");
                                }
                                _ => {}
                            }
                        }
                    } else {
                        error!("Exchange response data is empty when placing order");
                    }
                }
                ExchangeResponseStatus::Err(e) => {
                    error!("Error with placing order: {e}");
                    if e.contains("Insufficient margin") {
                        error!("Not enough margin to place order, skipping trade.");
                    }
                }
            },
            Err(e) => error!("Error with placing order: {e}"),
        }

        (0.0, 0)
    }
}
