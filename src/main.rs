// === Hyperliquid Market Maker Bot (Refactored) ===
use ethers::signers::{LocalWallet, Signer};
use ethers::types::H160;
use hyperliquid_rust_sdk::ClientCancelRequest;
use hyperliquid_rust_sdk::ExchangeDataStatus;
use hyperliquid_rust_sdk::ExchangeDataStatuses;
use hyperliquid_rust_sdk::ExchangeResponse;
use hyperliquid_rust_sdk::ExchangeResponseStatus;
use hyperliquid_rust_sdk::{
    BaseUrl, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, InfoClient, Message,
    Subscription, UserStateResponse,
};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Write},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc::unbounded_channel};
use tokio::time::sleep;
use uuid::Uuid;
// === Parameters ===
const TWAP_WINDOW: usize = 120;
const TRADE_WINDOW: usize = 80;
const DEVIATION_THRESHOLD: f64 = 0.002;
const AGGRESSIVE_SPREAD_TICKS: f64 = 0.5;
const LEVERAGE: f64 = 20.0;
const POSITION_LIMIT: f64 = 5.0;
const COIN: &str = "BTC";
const BALANCE_REFRESH_SECS: u64 = 15;
const ORDER_MAX_AGE_MS: u128 = 30_000;
const TICK_SIZE: f64 = 1.0;
const SIZE_STEP: f64 = 0.001;
const PROFIT_DROP_THRESHOLD: f64 = 0.3;

// === Market data structures ===
#[derive(Debug, Clone)]
pub struct BookSample {
    pub timestamp_ms: u64,
    pub mid_price: f64,
    pub best_bid: f64,
    pub best_ask: f64,
    pub bid_volume: f64,
    pub ask_volume: f64,
}

#[derive(Debug, Clone)]
pub struct TradeSample {
    pub price: f64,
    pub size: f64,
    pub is_buy: bool,
    pub timestamp_ms: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Position {
    pub base: f64,
    pub quote: f64,
}

#[derive(Debug, Default, Clone)]
pub struct SignalState {
    pub book_history: VecDeque<BookSample>,
    pub trade_history: VecDeque<TradeSample>,
    pub trend_score: f64,
    pub twap: f64,
    pub sliding_signal: f64,
    pub normalized_slide: f64,
    pub fill_score: f64,
    pub twap_deviation: f64,
    pub mean_revert_signal: String,
    pub best_bid: f64,
    pub best_ask: f64,
    pub volatility: f64,
    pub aggressive_mode: bool,
    pub position: Position,
    pub mid_price: f64,
    pub usdc_balance: f64,
    pub pnl_history: VecDeque<f64>,
    pub profit_peak: f64,
    pub open_orders: HashMap<u64, Instant>, // order_id -> timestamp
    pub is_closing_position: bool,
    pub last_close_time: Option<Instant>,
    pub last_exit_pnl: f64,
    pub pnl_deteriorating: bool,
}
#[derive(Debug, Clone)]
pub struct SignalEngine {
    pub state: SignalState,
}
impl SignalEngine {
    pub fn new() -> Self {
        let mut state = SignalState::default();
        state.usdc_balance = 100.0;
        Self { state }
    }

    pub fn update_balance(&mut self, balance: f64) {
        self.state.usdc_balance = balance;
    }

    pub fn process_l2_book(
        &mut self,
        ts: u64,
        bid_px: f64,
        ask_px: f64,
        bid_vol: f64,
        ask_vol: f64,
    ) {
        // update book history
        self.state.book_history.push_back(BookSample {
            timestamp_ms: ts,
            mid_price: (bid_px + ask_px) / 2.0,
            best_bid: bid_px,
            best_ask: ask_px,
            bid_volume: bid_vol,
            ask_volume: ask_vol,
        });
        if self.state.book_history.len() > TWAP_WINDOW {
            self.state.book_history.pop_front();
        }

        // compute signals using immutable refs
        let mid = self.state.book_history.back().unwrap().mid_price;
        self.state.best_bid = bid_px;
        self.state.best_ask = ask_px;
        self.state.mid_price = mid;
        self.state.trend_score = compute_momentum(&self.state.book_history);
        self.state.twap = compute_twap(&self.state.book_history);
        self.state.twap_deviation = compute_twap_deviation(mid, self.state.twap);
        self.state.mean_revert_signal = interpret_mean_reversion(self.state.twap_deviation);
        self.state.volatility = compute_volatility(&self.state.book_history);
        self.state.aggressive_mode = (ask_px - bid_px) <= 2.0 && self.state.volatility < 10.0;
        let (slide, norm) = compute_decay_weighted_slide(&self.state.trade_history, ts);
        self.state.sliding_signal = slide;
        self.state.normalized_slide = norm;

        // decide fill score
        let trend_strength = self.state.trend_score.tanh();
        let micro_pressure = self.state.normalized_slide;
        self.state.fill_score = if trend_strength.abs() > 0.1 {
            trend_strength.signum()
        } else if micro_pressure.abs() > 0.4 {
            micro_pressure.signum()
        } else {
            0.0
        };
    }

    pub fn process_trade(&mut self, price: f64, size: f64, is_buy: bool, ts: u64) {
        self.state.trade_history.push_back(TradeSample {
            price,
            size,
            is_buy,
            timestamp_ms: ts,
        });
        if self.state.trade_history.len() > TRADE_WINDOW {
            self.state.trade_history.pop_front();
        }
    }

    pub fn print(&self) {
        let s = &self.state;
        println!(
            "[Signal] Trend: {:.3} | TWAP: {:.2} | FillScore: {:.2} | Balance: {:.2} USDC",
            s.trend_score, s.twap, s.fill_score, s.usdc_balance
        );
        io::stdout().flush().unwrap();
    }
}

// === signal helpers ===
fn compute_momentum(hist: &VecDeque<BookSample>) -> f64 {
    if hist.len() < 2 {
        return 0.0;
    }
    hist.iter()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| w[0].mid_price - w[1].mid_price)
        .sum()
}

fn compute_twap(hist: &VecDeque<BookSample>) -> f64 {
    let n = hist.len().min(TWAP_WINDOW);
    if n == 0 {
        return 0.0;
    }
    hist.iter().rev().take(n).map(|b| b.mid_price).sum::<f64>() / (n as f64)
}

fn compute_decay_weighted_slide(trades: &VecDeque<TradeSample>, now: u64) -> (f64, f64) {
    let half = 8000.0;
    let (mut net, mut tot) = (0.0, 0.0);
    for t in trades {
        let age = (now as f64 - t.timestamp_ms as f64).max(0.0);
        let w = (-age.ln_1p() / half).exp();
        net += (if t.is_buy { 1.0 } else { -1.0 }) * t.size * w;
        tot += t.size * w;
    }
    if tot > 1e-6 {
        (net, net / tot)
    } else {
        (0.0, 0.0)
    }
}

fn compute_twap_deviation(p: f64, t: f64) -> f64 {
    if t.abs() < 1e-6 { 0.0 } else { (p - t) / t }
}

fn interpret_mean_reversion(d: f64) -> String {
    if d > DEVIATION_THRESHOLD {
        "Fade breakout".into()
    } else if d < -DEVIATION_THRESHOLD {
        "Scalp retracement".into()
    } else {
        "Neutral".into()
    }
}

fn compute_volatility(hist: &VecDeque<BookSample>) -> f64 {
    let n = hist.len();
    if n < 2 {
        return 0.0;
    }
    let m = hist.iter().map(|s| s.mid_price).sum::<f64>() / (n as f64);
    let v = hist.iter().map(|s| (s.mid_price - m).powi(2)).sum::<f64>() / (n as f64);
    v.sqrt()
}

// === Quote & Risk Management ===
pub struct QuoteProposal {
    pub side: String,
    pub price: f64,
    pub size: f64,
}

pub struct QuoteLayerManager;
impl QuoteLayerManager {
    pub fn build_quotes(signal: &SignalState) -> Vec<QuoteProposal> {
        let spread = if signal.aggressive_mode {
            AGGRESSIVE_SPREAD_TICKS
        } else {
            2.0
        } * (1.0 + signal.volatility * 0.1).min(3.0);
        let half_spread = spread / 2.0;
        let fair_bid = round_to_tick(signal.mid_price - half_spread, TICK_SIZE);
        let fair_ask = round_to_tick(signal.mid_price + half_spread, TICK_SIZE);
        let base_size = (signal.usdc_balance * LEVERAGE) / signal.mid_price;

        let vol_adj = base_size * (1.0 / (1.0 + signal.volatility)).clamp(0.5, 2.0);
        // scale size up if stable positive pnl
        let pnl_avg: f64 = signal
            .pnl_history
            .iter()
            .rev()
            .take(10)
            .copied()
            .sum::<f64>()
            / (signal.pnl_history.len().min(10).max(1) as f64);

        let pnl_boost_factor = if pnl_avg > 0.2 && signal.volatility < 5.0 {
            1.2
        } else if pnl_avg > 0.1 {
            1.05
        } else {
            1.0
        };

        let vol_adj = vol_adj * pnl_boost_factor;

        let bias = signal.fill_score;
        let buy_size = round_size(vol_adj * (1.0 + bias), SIZE_STEP);
        let sell_size = round_size(vol_adj * (1.0 - bias), SIZE_STEP);

        vec![
            QuoteProposal {
                side: "Buy".into(),
                price: fair_bid,
                size: buy_size,
            },
            QuoteProposal {
                side: "Sell".into(),
                price: fair_ask,
                size: sell_size,
            },
        ]
    }
}

pub struct RiskManager {
    pub max_position: f64,
    pub exchange: Arc<ExchangeClient>,
    pub wallet: LocalWallet,
}

impl RiskManager {
    pub fn new(mp: f64, e: Arc<ExchangeClient>, w: LocalWallet) -> Self {
        Self {
            max_position: mp,
            exchange: e,
            wallet: w,
        }
    }

    pub async fn sync_state(&self, state: &mut SignalState) -> Option<UserStateResponse> {
        let client = InfoClient::new(None, Some(BaseUrl::Testnet)).await.ok()?;
        const ADDRESS: &str = "0x3daBb4cB399B36A1b35Bea776A9bAAB186BE21F5";
        fn address() -> H160 {
            ADDRESS.to_string().parse().unwrap()
        }
        let user = address();
        let resp = client.user_state(user).await.ok()?;
        println!("{:?}", resp);
        if let Some(p) = resp.asset_positions.iter().find(|p| p.type_string == COIN) {
            if state.position.base.abs() < 1e-4 {
                state.is_closing_position = false;
            }
            let entry_px = p
                .position
                .entry_px
                .as_ref()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let pos_sz = p.position.szi.parse::<f64>().unwrap_or(0.0);
            if pos_sz.abs() < 1e-4 && state.position.base.abs() >= 1e-4 {
                println!("Position closed — resetting last_exit_pnl");
                state.last_exit_pnl = 0.0;
            }

            let current_px = state.mid_price; // use book mid-price

            let pnl = if entry_px > 0.0 {
                (current_px - entry_px) / entry_px * (if pos_sz >= 0.0 { 1.0 } else { -1.0 })
            } else {
                0.0
            };

            state.position.base = p.position.szi.parse().unwrap_or(0.0);
            state.position.quote = 0.0;
            state.pnl_history.push_back(pnl);
            if state.pnl_history.len() > 100 {
                state.pnl_history.pop_front();
            }
            let pnl_len = state.pnl_history.len();
            if pnl_len >= 5 {
                let last = state.pnl_history.back().copied().unwrap_or(0.0);
                let avg_recent = state.pnl_history.iter().rev().take(5).copied().sum::<f64>() / 5.0;
                let pnl_deteriorating = avg_recent - last > 0.05; // e.g. >5% drop over 5 samples
                state.pnl_deteriorating = pnl_deteriorating;
            }

            state.profit_peak = state.profit_peak.max(pnl);
        }
        Some(resp)
    }

    pub async fn cancel_order(&self, oid: u64) -> Result<(), Box<dyn std::error::Error>> {
        let cancel = ClientCancelRequest {
            asset: COIN.into(),
            oid,
        };
        let res = self.exchange.cancel(cancel, Some(&self.wallet)).await;
        match &res {
            Ok(response) => println!("Cancel success: {:?}", response),
            Err(e) => println!("Cancel failed: {:?}", e),
        }
        res.map(|_| ()).map_err(|e| e.into())
    }

    pub async fn close_position(&self, state: &mut SignalState) {
        state.is_closing_position = true;
        state.last_close_time = Some(Instant::now());

        let sz = state.position.base.abs();
        if sz < 1e-4 {
            println!("Already flat, skipping close_position");
            return;
        }
        let is_buy = state.position.base < 0.0;
        let px = if is_buy {
            state.best_ask + 2.0
        } else {
            state.best_bid - 2.0
        };
        let req = ClientOrderRequest {
            asset: COIN.into(),
            is_buy,
            reduce_only: false,
            limit_px: round_to_tick(px, TICK_SIZE),
            sz: round_size(sz, SIZE_STEP),
            cloid: Some(Uuid::new_v4()),
            order_type: ClientOrder::Limit(ClientLimit { tif: "Gtc".into() }),
        };
        println!(
            "Closing position: {} {} at {:.2}",
            if is_buy { "Buy" } else { "Sell" },
            sz,
            req.limit_px
        );
        let _ = self.exchange.order(req, Some(&self.wallet)).await;
    }

    pub async fn evaluate(&self, state: &mut SignalState, quotes: &[QuoteProposal]) {
        if state.is_closing_position {
            println!("In the middle of position close, skipping quote placement");
            return;
        }
        // After updating state with pnl_pct

        // === Sync Position State ===
        if let Some(user_state) = self.sync_state(state).await {
            println!("Current positions:");
            for p in user_state.asset_positions.iter() {
                println!(
                    "  {:?}: base={}, entry_px={:?}, unrealized_pnl={:?}",
                    p.type_string, p.position.szi, p.position.entry_px, p.position.unrealized_pnl
                );
            }
        }

        // === Check PnL Drop (Risk Management) ===
        let pnl = *state.pnl_history.back().unwrap_or(&0.0);
        let drop = state.profit_peak - pnl;
        if pnl > 0.0 && drop > PROFIT_DROP_THRESHOLD {
            println!(
                "Profit drop detected: peak {:.2} -> current {:.2}",
                state.profit_peak, pnl
            );
            self.close_position(state).await;
            return;
        }
        if state.last_exit_pnl < 0.0 && pnl >= state.last_exit_pnl.abs() {
            println!(
                "Recovered from previous loss {:.4}%, exiting at {:.4}%",
                state.last_exit_pnl, pnl
            );
            self.close_position(state).await;
            return;
        }
        if state.position.base > 0.0 && state.fill_score < -0.3 && state.pnl_deteriorating {
            println!(
                "Aggressive exit: long side fill_score={} & pnl drop",
                state.fill_score
            );
            self.close_position(state).await;
            return;
        }
        if state.position.base < 0.0 && state.fill_score > 0.3 && state.pnl_deteriorating {
            println!(
                "Aggressive exit: short side fill_score={} & pnl drop",
                state.fill_score
            );
            self.close_position(state).await;
            return;
        }

        // === Adverse Fill Score Based Exit ===
        if state.position.base > 0.0 && state.fill_score < 0.0 {
            println!("Closing adverse long: fill_score {}", state.fill_score);
            self.close_position(state).await;
            return;
        }

        if state.position.base < 0.0 && state.fill_score > 0.0 {
            println!("Closing adverse short: fill_score {}", state.fill_score);
            self.close_position(state).await;
            return;
        }

        // === Cancel Stale Orders ===
        const ORDER_MAX_AGE_MS: u128 = 30_000;
        let now = Instant::now();
        let mut stale_orders = vec![];

        for (cloid, timestamp) in state.open_orders.iter() {
            if now.duration_since(*timestamp).as_millis() > ORDER_MAX_AGE_MS {
                stale_orders.push(*cloid);
            }
        }

        for cloid in stale_orders {
            println!("Canceling stale order: {:?}", cloid);

            let k = self.cancel_order(cloid).await;
            println!("{:?}", k);
            state.open_orders.remove(&cloid);
        }

        // === Place Fresh Quotes ===
        let mid = state.mid_price;
        let levels = 5;

        for i in 1..=levels {
            let offset = i as f64 * TICK_SIZE;

            // --- Place Bid ---
            let bid_px = round_to_tick(mid - offset, TICK_SIZE);
            let bid_cloid = Uuid::new_v4();
            let bid_req = ClientOrderRequest {
                asset: COIN.into(),
                is_buy: true,
                reduce_only: false,
                limit_px: bid_px,
                sz: SIZE_STEP,
                cloid: Some(bid_cloid),
                order_type: ClientOrder::Limit(ClientLimit { tif: "Gtc".into() }),
            };

            println!("Layered Bid {} @ {}", bid_req.sz, bid_req.limit_px);
            let response = self.exchange.order(bid_req, Some(&self.wallet)).await;

            // Match the ExchangeResponseStatus to extract ExchangeResponse
            let exchange_response_status = match response {
                Ok(status) => status,
                Err(e) => {
                    println!("HTTP/SDK error: {:?}", e);
                    return; // or return Err(e) if you’re in a Result-returning fn
                }
            };

            let exchange_response = match exchange_response_status {
                ExchangeResponseStatus::Ok(data) => data,
                ExchangeResponseStatus::Err(e) => {
                    println!("Exchange returned error: {:?}", e);
                    return; // or return Err(e) if inside a Result-returning function
                }
            };

            // Unwrap the optional data
            let data_statuses = exchange_response
                .data
                .expect("No data in exchange response");

            // Iterate over statuses and extract `oid`
            for status in data_statuses.statuses {
                match status {
                    ExchangeDataStatus::Filled(order) => {
                        println!("Order filled: {:?}", order);
                        let oid = order.oid;
                        state.open_orders.insert(oid, Instant::now());

                        println!("Extracted oid: {}", oid);
                    }
                    ExchangeDataStatus::Resting(order) => {
                        println!("Order resting: {:?}", order);
                        let oid = order.oid;
                        state.open_orders.insert(oid, Instant::now());

                        println!("Extracted oid: {}", oid);
                    }
                    other => {
                        panic!("Unexpected order status: {:?}", other);
                    }
                }
            }

            // --- Place Ask ---
            let ask_px = round_to_tick(mid + offset, TICK_SIZE);
            let ask_cloid = Uuid::new_v4();
            let ask_req = ClientOrderRequest {
                asset: COIN.into(),
                is_buy: false,
                reduce_only: false,
                limit_px: ask_px,
                sz: SIZE_STEP,
                cloid: Some(ask_cloid),
                order_type: ClientOrder::Limit(ClientLimit { tif: "Gtc".into() }),
            };
            println!("Layered Ask {} @ {}", ask_req.sz, ask_req.limit_px);
            let ask_res = self.exchange.order(ask_req, Some(&self.wallet)).await;

            // Correct: unwrap the outer Result
            let exchange_response_status = ask_res.expect("Order request failed");

            let exchange_response = match exchange_response_status {
                ExchangeResponseStatus::Ok(data) => data,
                ExchangeResponseStatus::Err(e) => {
                    println!("Exchange returned error: {e}");
                    return; // Or `return Err(e);` if in a Result-returning fn
                }
            };

            // Unwrap the optional data
            let data_statuses = exchange_response
                .data
                .expect("No data in exchange response");

            // Iterate over statuses and extract `oid`
            for status in data_statuses.statuses {
                match status {
                    ExchangeDataStatus::Filled(order) => {
                        println!("Order filled: {:?}", order);
                        let oid = order.oid;
                        state.open_orders.insert(oid, Instant::now());
                        println!("Extracted oid: {}", oid);
                    }
                    ExchangeDataStatus::Resting(order) => {
                        println!("Order resting: {:?}", order);
                        let oid = order.oid;
                        state.open_orders.insert(oid, Instant::now());
                        println!("Extracted oid: {}", oid);
                    }
                    other => {
                        panic!("Unexpected order status: {:?}", other);
                    }
                }
            }
        }
    }
}

// === Router & Main ===
pub struct MessageRouter {
    signal: Arc<Mutex<SignalEngine>>,
    risk_mgr: Arc<RiskManager>,
    info_client: Arc<InfoClient>,
}

impl MessageRouter {
    pub fn new(s: Arc<Mutex<SignalEngine>>, r: Arc<RiskManager>, info: Arc<InfoClient>) -> Self {
        Self {
            signal: s,
            risk_mgr: r,
            info_client: info,
        }
    }

    pub async fn handle(&self, msg: Message) {
        match msg {
            Message::L2Book(b) => {
                let bids = &b.data.levels[0];
                let asks = &b.data.levels[1];
                if bids.is_empty() || asks.is_empty() {
                    return;
                }
                let bid_px = bids[0].px.parse().unwrap_or(0.0);
                let ask_px = asks[0].px.parse().unwrap_or(0.0);
                let bid_vol = bids.iter().map(|x| x.sz.parse().unwrap_or(0.0)).sum();
                let ask_vol = asks.iter().map(|x| x.sz.parse().unwrap_or(0.0)).sum();

                let mut eng = self.signal.lock().await;
                eng.process_l2_book(b.data.time, bid_px, ask_px, bid_vol, ask_vol);
                eng.print();

                if self.risk_mgr.sync_state(&mut eng.state).await.is_none() {
                    return;
                }

                // Cancel stale orders if any, then quote
                let quotes = QuoteLayerManager::build_quotes(&eng.state);
                self.risk_mgr.evaluate(&mut eng.state, &quotes).await;
            }
            Message::Trades(t) => {
                let mut eng = self.signal.lock().await;
                for e in t.data {
                    let price = e.px.parse().unwrap_or(0.0);
                    let size = e.sz.parse().unwrap_or(0.0);
                    eng.process_trade(price, size, e.side == "B", e.time);
                }
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let wallet = get_testnet_wallet();

    let exchange = Arc::new(
        ExchangeClient::new(None, wallet.clone(), Some(BaseUrl::Testnet), None, None).await?,
    );
    let mut info = InfoClient::new(None, Some(BaseUrl::Testnet)).await?;

    let (tx, mut rx) = unbounded_channel();
    info.subscribe(Subscription::L2Book { coin: COIN.into() }, tx.clone())
        .await?;
    info.subscribe(Subscription::Trades { coin: COIN.into() }, tx.clone())
        .await?;
    let info = Arc::new(info);

    let signal = Arc::new(Mutex::new(SignalEngine::new()));
    let risk = Arc::new(RiskManager::new(
        POSITION_LIMIT,
        exchange.clone(),
        wallet.clone(),
    ));
    let router = MessageRouter::new(signal.clone(), risk.clone(), info.clone());

    // balance refresher
    tokio::spawn({
        let sig = signal.clone();
        let inf = info.clone();
        let wal = wallet.clone();
        async move {
            loop {
                if let Ok(resp) = inf.user_state(wal.address()).await {
                    if let Ok(bal) = resp.cross_margin_summary.account_value.parse::<f64>() {
                        sig.lock().await.update_balance(bal);
                    }
                }
                sleep(Duration::from_secs(BALANCE_REFRESH_SECS)).await;
            }
        }
    });

    while let Some(msg) = rx.recv().await {
        router.handle(msg).await;
    }

    Ok(())
}

fn round_to_tick(price: f64, tick: f64) -> f64 {
    (price / tick).round() * tick
}

fn round_size(sz: f64, step: f64) -> f64 {
    (sz / step).floor() * step
}

fn get_testnet_wallet() -> LocalWallet {
    "0x77aee190b31390b9e1835cb295fc1d041c968d5c692774e5a37e5a75d75688b0"
        .parse()
        .unwrap()
}
