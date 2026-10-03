# Сеть узлов Квазара: trust, seeds, reachability, QUIC mesh

*Без привилегированной «столицы». Bootstrap — любой online seed из GitHub.*

## Три плоскости

| Плоскость | Где истина | Что хранит |
|-----------|------------|------------|
| **Trust** | таблица `nodes` + `PeerListUpdate` | `node_id`, pubkey, статус — *кто свой* |
| **Seeds** | [`network/seeds.json`](../network/seeds.json) (GitHub) | подмножество с **входящим** endpoint — *куда позвонить первый раз* |
| **Reachability** | `node_endpoints` | эфемерный tip (`https://…` и/или `quic://host:port`), `last_seen` |

URL **не** личность узла. Смена IP ≠ перерегистрация.

## Транспорт

1. **QUIC** (UDP, порт `QUAZAR_QUIC_PORT`, по умолчанию `8443`) — основной dial между узлами.
2. **Relay** — если прямой dial не вышел: любой reachable trusted peer доставляет payload (1 hop; `POST /p2p/relay` как HTTP-fallback при блокировке UDP).
3. **HTTP gossip** (`/events/gossip`, `/chat/gossip`) — последний запасной путь.

Auth на QUIC: первый кадр `Hello { node_id, token }` с общим `QUAZAR_NODE_SECRET` (как Bearer на HTTP). TLS-сертификат эфемерный self-signed; доверие — токену, не CA.

### Env

| Переменная | Смысл |
|------------|--------|
| `QUAZAR_QUIC_PORT` | UDP listen (default `8443`) |
| `QUAZAR_QUIC_ADVERTISE` | host для tip `quic://host:port` (иначе из `QUAZAR_PUBLIC_URL` / `QUAZAR_NODE_URL`) |
| `QUAZAR_PUBLIC_URL` | публичный HTTPS tip для announce |
| `QUAZAR_SEEDS_URL` / `QUAZAR_SEEDS_PATH` | bootstrap seeds |

На VPS в compose открыт `8443/udp`. За NAT без UDP узел всё равно шлёт outbound и принимает через relay.

## Запуск нового узла

1. Качает `seeds.json` (`QUAZAR_SEEDS_URL` → GitHub raw → локальный `network/seeds.json`).
2. Регистрирует tips в `nodes` + `node_endpoints` (HTTP + `quic://` если есть).
3. Outbound: `GET /p2p/trusted` у первого живого seed → догоняет trust-список.
4. Слушает QUIC; `background_sync` тянет `/blocks` и ключи по HTTP tip.
5. Announce раз в минуту: HTTPS + `quic://` tips на пиров.
6. Gossip: QUIC → relay → HTTP.

## API

| Метод | Путь | Кто |
|-------|------|-----|
| POST | `/p2p/announce` | node secret / Aiya |
| GET | `/p2p/tip/:node_id` | node secret / гражданин с ключом |
| GET | `/p2p/trusted` | node secret / гражданин с ключом |
| POST | `/p2p/relay` | node secret — HTTP relay fallback |

## Обновление seeds на GitHub

В `seeds.json` — только узлы со **стабильным** входящим доступом (HTTPS и/или UDP).  
Домашний NAT — в trust через реестр, в seeds — не обязан.
