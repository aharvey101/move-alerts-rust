use anyhow::Result;
use futures::{SinkExt, StreamExt};
use reqwest;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio::{net::TcpStream, time};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, WebSocketStream};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    timeframes: Vec<String>,
}

#[derive(Debug)]
enum PercentThreshold {
    FiveMin = 1,
    FifteenMin = 3,
    ThirtyMin = 5,
    OneHour = 10,
}

impl PercentThreshold {
    fn from_timeframe(timeframe: &str) -> f64 {
        match timeframe {
            "5m" => PercentThreshold::FiveMin as i32 as f64,
            "15m" => PercentThreshold::FifteenMin as i32 as f64,
            "30m" => PercentThreshold::ThirtyMin as i32 as f64,
            "1h" => PercentThreshold::OneHour as i32 as f64,
            _ => panic!("Invalid timeframe"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TelegramMessage {
    chat_id: String,
    text: String,
    parse_mode: String,
}

async fn send_telegram_alert(message: &str) -> Result<()> {
    let token = env::var("TELEGRAM_TOKEN")?;
    let chat_id = env::var("TELEGRAM_CHAT_ID")?;
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);

    let client = reqwest::Client::new();
    let telegram_message = TelegramMessage {
        chat_id,
        text: message.to_string(),
        parse_mode: "HTML".to_string(),
    };

    println!(
        "SENDING TELEGRAM ALERT {}: {}",
        chrono::Utc::now().to_rfc3339(),
        message
    );

    client.post(&url).json(&telegram_message).send().await?;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct Symbol {
    symbol: String,
    quoteAsset: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct ExchangeInfo {
    symbols: Vec<Symbol>,
}

#[derive(Debug)]
pub struct BinanceWebSocketServer {
    processed_candles: Arc<Mutex<HashMap<String, f64>>>,
    is_initialized: Arc<Mutex<bool>>,
}

impl BinanceWebSocketServer {
    pub fn new() -> Self {
        BinanceWebSocketServer {
            processed_candles: Arc::new(Mutex::new(HashMap::new())),
            is_initialized: Arc::new(Mutex::new(false)),
        }
    }

    async fn fetch_usdt_pairs(&self) -> Result<Vec<String>> {
        let response = reqwest::get("https://fapi.binance.com/fapi/v1/exchangeInfo")
            .await?
            .json::<ExchangeInfo>()
            .await?;

        let pairs: Vec<String> = response
            .symbols
            .into_iter()
            .filter(|symbol| symbol.quoteAsset == "USDT" && symbol.status == "TRADING")
            .map(|symbol| symbol.symbol)
            .collect();

        if pairs.is_empty() {
            println!("No symbols found in response, retrying...");
        }

        Ok(pairs)
    }

    fn chunk_array<T: Clone>(array: &[T], size: usize) -> Vec<Vec<T>> {
        array.chunks(size).map(|chunk| chunk.to_vec()).collect()
    }

    async fn create_chunk_connection(
        &self,
        pairs: Vec<String>,
        index: usize,
        config: &ServerConfig,
    ) -> Result<()> {
        let streams: Vec<String> = pairs
            .iter()
            .flat_map(|pair| {
                config
                    .timeframes
                    .iter()
                    .map(move |timeframe| format!("{}@kline_{}", pair.to_lowercase(), timeframe))
            })
            .collect();

        let ws_url = format!(
            "wss://fstream.binance.com/stream?streams={}",
            streams.join("/")
        );

        let processed_candles = Arc::clone(&self.processed_candles);
        let is_initialized = Arc::clone(&self.is_initialized);

        loop {
            match connect_async(&ws_url).await {
                Ok((ws_stream, _)) => {
                    println!("WebSocket {} connected", index + 1);
                    *is_initialized.lock().await = true;
                    let _ =
                        send_telegram_alert(&format!("WebSocket {} connected", index + 1)).await;

                    self.handle_messages(ws_stream, index, processed_candles.clone())
                        .await?;
                }
                Err(e) => {
                    println!("Failed to connect to WebSocket {}: {}", index + 1, e);
                    time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn handle_messages(
        &self,
        mut ws_stream: WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        index: usize,
        processed_candles: Arc<Mutex<HashMap<String, f64>>>,
    ) -> Result<()> {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Err(e) = self.process_message(&text, &processed_candles).await {
                        println!("Error processing message: {}", e);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    println!("WebSocket {} error: {}", index + 1, e);
                    break;
                }
            }
        }
        Ok(())
    }

    async fn process_message(
        &self,
        message: &str,
        processed_candles: &Arc<Mutex<HashMap<String, f64>>>,
    ) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(message)?;
        println!("{}", value);
        if let Some(data) = value.get("data") {
            if data["e"].as_str() == Some("kline") {
                let kline = &data["k"];
                let symbol = data["s"].as_str().unwrap_or("");
                let open: f64 = kline["o"].as_str().unwrap_or("0").parse()?;
                let close: f64 = kline["c"].as_str().unwrap_or("0").parse()?;
                let candle_open_time = kline["t"].as_i64().unwrap_or(0);
                let timeframe = kline["i"].as_str().unwrap_or("");

                let percent_change = ((close - open) / open) * 100.0;
                let key = format!("{}-{}", candle_open_time, open);
                let threshold = PercentThreshold::from_timeframe(timeframe);

                let mut candles = processed_candles.lock().await;
                if candles.contains_key(&key) {
                    return Ok(());
                }
                if percent_change > threshold {
                    let _ = send_telegram_alert(&format!(
                        "🟢 {} crossed above {}% on {} timeframe ({:.2}%)",
                        symbol, threshold, timeframe, percent_change
                    ))
                    .await;
                    candles.insert(key.clone(), percent_change);
                }

                if percent_change < -threshold {
                    let _ = send_telegram_alert(&format!(
                        "🔴 {} crossed below -{}% on {} timeframe ({:.2}%)",
                        symbol, threshold, timeframe, percent_change
                    ))
                    .await;
                    candles.insert(key, percent_change);
                }
            }
        }
        Ok(())
    }

    pub async fn initialize(&self, config: ServerConfig) -> Result<()> {
        loop {
            match self.fetch_usdt_pairs().await {
                Ok(pairs) => {
                    let pair_chunks = Self::chunk_array(&pairs, 20);

                    let mut handles = vec![];
                    for (index, chunk) in pair_chunks.into_iter().enumerate() {
                        let config = config.clone();
                        let self_clone = self.clone();

                        let handle = tokio::spawn(async move {
                            if let Err(e) = self_clone
                                .create_chunk_connection(chunk, index, &config)
                                .await
                            {
                                println!("Error in chunk connection {}: {}", index, e);
                            }
                        });
                        handles.push(handle);
                    }

                    futures::future::join_all(handles).await;
                }
                Err(e) => {
                    println!("Failed to initialize Binance connections: {}", e);
                    *self.is_initialized.lock().await = false;
                    time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}

impl BinanceWebSocketServer {
    fn clone(&self) -> Self {
        BinanceWebSocketServer {
            processed_candles: Arc::clone(&self.processed_candles),
            is_initialized: Arc::clone(&self.is_initialized),
        }
    }
}

// Example usage
#[tokio::main]
async fn main() -> Result<()> {
    let config = ServerConfig {
        timeframes: vec![
            "5m".to_string(),
            "15m".to_string(),
            "30m".to_string(),
            "1h".to_string(),
        ],
    };

    let server = BinanceWebSocketServer::new();
    server.initialize(config).await?;
    Ok(())
}
