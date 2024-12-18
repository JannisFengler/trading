#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

use ethers::signers::LocalWallet;
use hyperliquid_rust_sdk::{Input, MarketMaker};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;
use std::time::{Duration, Instant};
use log::info;

#[derive(Debug, Deserialize)]
struct Trade {
    e: String,  // Event type
    E: u64,     // Event time
    s: String,  // Symbol
    t: u64,     // Trade ID
    p: String,  // Price
    q: String,  // Quantity
    T: u64,     // Transaction time
    X: String,  // Type
    m: bool,    // Is buyer market maker
}

/// Connect to Binance and continuously update latest_binance_price.
async fn run_binance_feed(latest_binance_price: Arc<Mutex<f64>>) -> Result<(), Box<dyn std::error::Error>> {
    let url = Url::parse("wss://fstream.binance.com/ws/btcusdt@trade")?;
    let (ws_stream, _) = connect_async(url).await?;
    let (_, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        if let Ok(Message::Text(text)) = msg {
            if let Ok(trade) = serde_json::from_str::<Trade>(&text) {
                if let Ok(price) = trade.p.parse::<f64>() {
                    let mut binance_price = latest_binance_price.lock().unwrap();
                    *binance_price = price;
                }
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    env_logger::init();

    // Key was randomly generated for testing and shouldn't be used with any real funds
    let wallet: LocalWallet = ""
        .parse()
        .unwrap();

    // Create a shared variable for the Binance price
    let latest_binance_price = Arc::new(Mutex::new(0.0));

    // Spawn the Binance feed in the background
    {
        let binance_clone = latest_binance_price.clone();
        tokio::spawn(async move {
            if let Err(e) = run_binance_feed(binance_clone).await {
                eprintln!("Binance feed error: {:?}", e);
            }
        });
    }

    // Define our single trading configuration (just BTC)
    let input = Input {
        asset: "BTC".to_string(),
        target_liquidity: 0.0002,
        max_bps_diff: 20,
        half_spread: 5,
        max_absolute_position_size: 0.004,
        decimals: 0,
        wallet,
    };

    let wallet_mutex = Arc::new(AsyncMutex::new(input.wallet.clone()));
    let binance_price_clone = latest_binance_price.clone();

    let mut mm = MarketMaker::new(Input {
        wallet: wallet_mutex.lock().await.clone(),
        ..input
    })
    .await
    .expect("Failed to create MarketMaker");

    // Assign the binance price reference to the market maker
    mm.binance_price = binance_price_clone;

    mm.start().await;
}
