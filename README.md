# Hyperliquid Market Making Bot

This is a basic market-making bot built for the [Hyperliquid](https://hyperliquid.xyz) exchange. It places layered bid and ask limit orders based on the current orderbook, listens to fills, and optionally exits positions based on PnL or size.

---

## Features

- Layered bid/ask strategy
- Position tracking (size, entry price)
- PnL tracking and position exit logic
- Fee estimation and volume tracking
- Cancel and replace logic for stale or filled orders

---

## Project Structure

```bash
project/
├── Cargo.toml
└── src/
    └── main.rs
```

Everything is currently implemented in a single file: `src/main.rs`.

---

## Setup

### Prerequisites

- Rust (recommended: latest stable version)
- A Hyperliquid account
- Your private key and wallet address

### Installation

```bash
git clone https://github.com/XxSNiPxX/hyperliquid_bot.git
cd hyperliquid_bot
cargo build --release
```

---

## Configuration

At the moment, private key and wallet address are hardcoded directly inside `main.rs`.

You should move them to a separate config file (like `config.toml`) or use environment variables in the future for better security.

---

## Usage

```bash
cargo run --release
```

The bot will continuously:

1. Fetch the orderbook
2. Place layered bids and asks around the mid price
3. Monitor fills
4. Exit positions when PnL or position thresholds are met

---

## Disclaimer

This bot is experimental. Use at your own risk. Trading involves risk, and you can lose money.

---

## License

MIT
