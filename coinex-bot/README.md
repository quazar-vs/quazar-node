# CoinEx One-Sided Martingale Bot

Односторонний мартингейл-бот для CoinEx (**spot** или **futures**).

## Алгоритм

1. Открывает **одну** позицию в сторону `TRADE_SIDE` минимальным лотом = `exchange_min_amount × LOT_COEFF`.
2. Ставит один limit TP; soft SL хранится в памяти.
3. При закрытии:
   - **TP** → новый цикл снова с min lot (exchange_min × LOT_COEFF);
   - **SL** → полная докупка shortage до `initial × LOT_STEP_COEFF^(step)` + modify того же TP;
     максимум **MAX_SOFT_SL_ADDS** докупок (по умолчанию 6 → 7 уровней). Partial нет; нехватка USDT или глубина исчерпана → freeze TP до fill.
     Сканер берёт пару только если баланса хватает на полную лестницу (`min_lot × price × coeff^MAX_SOFT_SL_ADDS × 1.05`).
4. Вторую сторону **не открывает** (не хедж).
5. При старте: есть `botB_tp` → restore; нет → процедура открытия.
6. `AUTO_MARKET`: после TP сканер берёт shortlist по 24h обороту (USDT), отсекает широкий спред, затем
   12h×`SCANNER_BARS` (пара жива давно), 1h×N (не один дамп/памп), 5m×N (на медвежьих нижний фитиль > тела).

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
- `TP_SL_PERCENT` автоматически пересчитывается из комиссии выбранной пары: `tp_sl_percent = max(maker,taker) * 6` (комиссия в долях, результат в процентах). Значение из `.env` используется только как fallback.
- После soft SL оба уровня сдвигаются ещё на тот же % от **первой** сделки

## Запуск

```bash
cp .env.example .env
# MARKET=KOMAUSDT
# MARKET_TYPE=spot
# TRADE_SIDE=buy
# LOT_COEFF=1
# LOT_STEP_COEFF=2
# MAX_SOFT_SL_ADDS=6
# TP_SL_PERCENT=10

cargo run --release
```

`Ctrl+C` — graceful shutdown.

## Важно

- Реальные ордера.
- Нехватка баланса (USDT) на soft SL — freeze до fill TP; превышение max lot — freeze до TP.
