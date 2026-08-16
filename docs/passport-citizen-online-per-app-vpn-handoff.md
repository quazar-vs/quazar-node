# Перенесено

Канон постановки переименован и уточнён (сервисы, не произвольные apps):

→ **[`passport-citizen-online-service-vpn-handoff.md`](./passport-citizen-online-service-vpn-handoff.md)**

**Канон:** SSH для приложения **полностью убрать** (не interim). Между app и сервером — **только VPN**: online → up, offline → down.

**Primary:** Shadowsocks-rust + ACL + тонкий VpnService/tun2socks (YouTube/Telegram, остальное direct).  
**Альтернатива:** WireGuard split. **SSH — нет.**

**Статус (~2026-08-16):** клиент Passport done (stub); **backend SS на VPS done** (`/api/vpn/ss/*` + ssserver); SSH tunnel в ядре удалён.  
**Следующий шаг:** client ss-local — [`passport-ss-local-replace-stub-handoff.md`](./passport-ss-local-replace-stub-handoff.md).