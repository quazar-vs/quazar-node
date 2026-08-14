# CoinEx One-Sided Martingale Bot

Односторонний мартингейл-бот для CoinEx (**spot** или **futures**).

## Алгоритм

1. Открывает **одну** позицию в сторону `TRADE_SIDE` минимальным лотом = `exchange_min_amount × LOT_COEFF`.
2. Ставит один limit TP; soft SL хранится в памяти.
3. При закрытии:
   - **TP** → новый цикл снова с min lot (exchange_min × LOT_COEFF);
   - **SL** → докупка shortage до `current × LOT_STEP_COEFF` + modify того же TP (не выше `MAX_LOT` / `MAX_LOT_SPOT`).
4. Вторую сторону **не открывает** (не хедж).
5. При старте: есть `botB_tp` → restore; нет → процедура открытия.

### Spot
- Вход: market только на shortage (если KOMA уже хватает на min lot — только limit TP)
- TP: один limit на противоположной стороне
- SL: soft (в памяти); при срабатывании — докупка + amend TP

### Futures
- Вход: market
- TP/SL: через position take-profit / stop-loss API

### TP / SL (один процент от первой сделки)

- `BUY`: `TP = open × (1 + TP_SL_PERCENT/100)`, `SL = open × (1 − TP_SL_PERCENT/100)`
- `SELL`: зеркально
- После soft SL оба уровня сдвигаются ещё на тот же % от **первой** сделки

## Запуск

```bash
cp .env.example .env
# MARKET=KOMAUSDT
# MARKET_TYPE=spot
# TRADE_SIDE=buy
# LOT_COEFF=1
# LOT_STEP_COEFF=2
# MAX_LOT_SPOT=6400
# TP_SL_PERCENT=10

cargo run --release
```

`Ctrl+C` — graceful shutdown.

## Важно

- Реальные ордера.
- Нехватка баланса (USDT) на soft SL — freeze до fill TP; превышение max lot — freeze до TP.
