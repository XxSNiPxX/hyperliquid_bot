# Hyperliquid Market Making Bot

A high-performance market-making bot built for the Hyperliquid perpetual exchange, written in Rust. It features custom order placement logic, orderbook tracking, and profit-based exit strategies.

---

## 🚀 Features

- Place layered bids/asks with custom sizing and spreads.
- Cancel stale or filled orders.
- Track PnL and close positions at profit thresholds.
- Modular architecture for strategy and order logic.

---

## 📁 Project Structure

```plaintext
├── Cargo.toml              # Rust dependencies and project config
├── config.toml             # Secrets and wallet configuration (not committed)
├── src/
│   ├── main.rs             # Entry point
│   ├── trade_new.rs        # Core trading logic (order placement, exit strategies)
│   ├── config.rs           # Config loading from TOML
│   ├── utils.rs            # Helper functions (e.g., size computation, logger)
│   └── client.rs           # Hyperliquid API wrapper and error handling
