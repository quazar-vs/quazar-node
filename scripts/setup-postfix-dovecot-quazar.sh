#!/usr/bin/env bash
# =============================================================================
# setup-postfix-dovecot-quazar.sh
#
# Установка и базовая настройка Postfix + Dovecot (Maildir) для quazar.website
# на Ubuntu. Запускать от root.
#
# ВАЖНО:
#  - Перед включением UFW всегда открываем SSH (иначе можно потерять доступ).
#  - На этом VPS порты 25/465/587 уже могут быть заняты docker-mailserver
#    (quazar-mailserver-1). Скрипт остановит конфликтующие контейнеры, если
#    задать STOP_DOCKER_MAIL=1 (по умолчанию спрашивает / останавливает при --force).
#
# Пример:
#   sudo bash /root/setup-postfix-dovecot-quazar.sh
#   sudo STOP_DOCKER_MAIL=1 bash /root/setup-postfix-dovecot-quazar.sh
# =============================================================================
set -euo pipefail

DOMAIN="quazar.website"
HOSTNAME="mail.${DOMAIN}"
MAIL_USER="${MAIL_USER:-mailuser}"
MAIL_PASS="${MAIL_PASS:-}"   # если пусто — генерируется
STOP_DOCKER_MAIL="${STOP_DOCKER_MAIL:-}"
FORCE="${FORCE:-0}"

log() { echo "[mail] $*"; }
die() { echo "[mail] ОШИБКА: $*" >&2; exit 1; }

require_root() {
  [[ "${EUID}" -eq 0 ]] || die "запустите от root (sudo)"
}

backup_file() {
  local f="$1"
  [[ -f "$f" ]] && cp -a "$f" "${f}.bak.$(date +%Y%m%d-%H%M%S)"
}

ports_busy() {
  local p
  for p in 25 465 587 143 993; do
    if ss -tln | awk '{print $4}' | grep -qE "[:.]${p}\$"; then
      return 0
    fi
  done
  return 1
}

stop_docker_mail_if_needed() {
  if ! command -v docker >/dev/null 2>&1; then
    return 0
  fi
  local names
  names="$(docker ps --format '{{.Names}}' 2>/dev/null | grep -Ei 'mail|postfix|dovecot' || true)"
  if [[ -z "$names" ]]; then
    return 0
  fi
  if [[ "$STOP_DOCKER_MAIL" == "1" || "$FORCE" == "1" ]]; then
    log "Остановка Docker-почты (конфликт портов):"
    echo "$names" | while read -r n; do
      [[ -n "$n" ]] || continue
      log "  docker stop $n"
      docker stop "$n" >/dev/null || true
    done
    return 0
  fi
  log "ВНИМАНИЕ: заняты почтовые порты контейнерами:"
  echo "$names"
  log "Передайте STOP_DOCKER_MAIL=1, чтобы остановить их, либо освободите порты вручную."
  die "порты 25/465/587 уже заняты (docker-mailserver)"
}

require_root

# --- пароль тестового пользователя ---
if [[ -z "$MAIL_PASS" ]]; then
  if command -v openssl >/dev/null 2>&1; then
    MAIL_PASS="$(openssl rand -base64 18 | tr -d '/+=' | head -c 16)"
  else
    MAIL_PASS="$(head -c 16 /dev/urandom | base64 | tr -d '/+=' | head -c 16)"
  fi
fi

log "Домен=${DOMAIN}, hostname=${HOSTNAME}, пользователь=${MAIL_USER}"

stop_docker_mail_if_needed
if ports_busy; then
  # после остановки docker ещё раз
  sleep 1
  if ports_busy && [[ "$STOP_DOCKER_MAIL" != "1" && "$FORCE" != "1" ]]; then
    die "порты почты всё ещё заняты — проверьте: ss -tlnp | grep -E ':25|:587|:465|:143|:993'"
  fi
fi

# =============================================================================
# 1) Установка пакетов
# =============================================================================
log "Установка postfix, dovecot-imapd, dovecot-pop3d..."
export DEBIAN_FRONTEND=noninteractive
# неинтерактивный выбор Internet Site
debconf-set-selections <<EOF
postfix postfix/main_mailer_type select Internet Site
postfix postfix/mailname string ${DOMAIN}
EOF
apt-get update -qq
apt-get install -y postfix dovecot-imapd dovecot-pop3d mailutils

# hostname машины (желательно)
if ! grep -q "$HOSTNAME" /etc/hosts 2>/dev/null; then
  echo "127.0.1.1 ${HOSTNAME} ${DOMAIN}" >> /etc/hosts || true
fi
hostnamectl set-hostname "$HOSTNAME" 2>/dev/null || true

# =============================================================================
# 2) Postfix
# =============================================================================
log "Настройка Postfix..."
backup_file /etc/postfix/main.cf

postconf -e "myhostname = ${HOSTNAME}"
postconf -e "myorigin = ${DOMAIN}"
postconf -e "mydestination = ${DOMAIN}, localhost.localdomain, localhost"
postconf -e "inet_interfaces = all"
postconf -e "inet_protocols = ipv4"
postconf -e "home_mailbox = Maildir/"
postconf -e "mailbox_command ="
# submission / smtps позже можно усилить SASL; базовая конфигурация
postconf -e "smtpd_banner = \$myhostname ESMTP"
postconf -e "biff = no"
postconf -e "append_dot_mydomain = no"

# master.cf: включить submission (587) и smtps (465) если есть шаблоны
if [[ -f /etc/postfix/master.cf ]]; then
  backup_file /etc/postfix/master.cf
  # раскомментировать submission/smtps при наличии стандартных строк Debian
  sed -i -E 's/^#[[:space:]]*submission[[:space:]]+inet/submission inet/' /etc/postfix/master.cf || true
  sed -i -E 's/^#[[:space:]]*smtps[[:space:]]+inet/smtps inet/' /etc/postfix/master.cf || true
fi

# =============================================================================
# 3) Dovecot — Maildir в /home/user/Maildir
# =============================================================================
log "Настройка Dovecot (Maildir)..."
backup_file /etc/dovecot/conf.d/10-mail.conf
backup_file /etc/dovecot/conf.d/10-auth.conf
backup_file /etc/dovecot/conf.d/10-master.conf

# mail_location
if [[ -f /etc/dovecot/conf.d/10-mail.conf ]]; then
  sed -i -E 's|^#?mail_location[[:space:]]*=.*|mail_location = maildir:~/Maildir|' \
    /etc/dovecot/conf.d/10-mail.conf
  if ! grep -q '^mail_location' /etc/dovecot/conf.d/10-mail.conf; then
    echo 'mail_location = maildir:~/Maildir' >> /etc/dovecot/conf.d/10-mail.conf
  fi
fi

# auth: системные пользователи (passwd)
if [[ -f /etc/dovecot/conf.d/10-auth.conf ]]; then
  sed -i -E 's|^#?disable_plaintext_auth[[:space:]]*=.*|disable_plaintext_auth = no|' \
    /etc/dovecot/conf.d/10-auth.conf || true
  # для теста без TLS на 143 — только на закрытом VPS; в проде включите TLS
  sed -i -E 's|^#?auth_mechanisms[[:space:]]*=.*|auth_mechanisms = plain login|' \
    /etc/dovecot/conf.d/10-auth.conf || true
fi

# протоколы
if [[ -f /etc/dovecot/dovecot.conf ]]; then
  backup_file /etc/dovecot/dovecot.conf
  if grep -q '^protocols' /etc/dovecot/dovecot.conf; then
    sed -i -E 's|^protocols[[:space:]]*=.*|protocols = imap pop3|' /etc/dovecot/dovecot.conf
  else
    echo 'protocols = imap pop3' >> /etc/dovecot/dovecot.conf
  fi
fi

# =============================================================================
# 4) UFW — почтовые порты (+ SSH, чтобы не отрезать доступ)
# =============================================================================
log "Открытие портов в UFW (сначала SSH)..."
if command -v ufw >/dev/null 2>&1; then
  ufw allow OpenSSH comment 'SSH' >/dev/null 2>&1 || ufw allow 22/tcp >/dev/null
  ufw allow 80/tcp comment 'HTTP' >/dev/null 2>&1 || true
  ufw allow 443/tcp comment 'HTTPS' >/dev/null 2>&1 || true
  for p in 25 143 993 465 587; do
    ufw allow "${p}/tcp" comment "mail ${p}" >/dev/null 2>&1 || true
  done
  ufw --force enable >/dev/null 2>&1 || true
  ufw status numbered | sed -n '1,30p' || true
else
  log "ufw не найден — правила iptables для 25,143,993,465,587"
  for p in 25 143 993 465 587; do
    iptables -C INPUT -p tcp --dport "$p" -j ACCEPT 2>/dev/null || \
      iptables -I INPUT -p tcp --dport "$p" -j ACCEPT
  done
fi

# =============================================================================
# 5) Тестовый пользователь mailuser
# =============================================================================
log "Создание пользователя ${MAIL_USER}..."
if id "$MAIL_USER" >/dev/null 2>&1; then
  log "пользователь уже существует — обновляем пароль"
else
  useradd -m -s /bin/bash "$MAIL_USER"
fi
echo "${MAIL_USER}:${MAIL_PASS}" | chpasswd
# Maildir
mkdir -p "/home/${MAIL_USER}/Maildir"/{cur,new,tmp}
chown -R "${MAIL_USER}:${MAIL_USER}" "/home/${MAIL_USER}/Maildir"
chmod -R 700 "/home/${MAIL_USER}/Maildir"

# локальный алиас / доставка на user@domain через системного user
# (Postfix при home_mailbox доставляет локальным пользователям)
if ! grep -q "^${MAIL_USER}@" /etc/aliases 2>/dev/null; then
  :
fi
newaliases 2>/dev/null || true

# сохранить учётные данные локально (права только root)
umask 077
{
  echo "# Generated $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "DOMAIN=${DOMAIN}"
  echo "HOSTNAME=${HOSTNAME}"
  echo "IMAP_HOST=${HOSTNAME}"
  echo "USER=${MAIL_USER}"
  echo "PASSWORD=${MAIL_PASS}"
  echo "EMAIL=${MAIL_USER}@${DOMAIN}"
} > /root/mailuser-credentials.txt
chmod 600 /root/mailuser-credentials.txt

# =============================================================================
# 6) Перезапуск служб
# =============================================================================
log "Перезапуск postfix и dovecot..."
systemctl enable postfix dovecot >/dev/null 2>&1 || true
systemctl restart postfix
systemctl restart dovecot

sleep 1
systemctl is-active postfix >/dev/null || die "postfix не active"
systemctl is-active dovecot >/dev/null || die "dovecot не active"

log "Статус:"
systemctl --no-pager --full status postfix | head -12 || true
echo "---"
systemctl --no-pager --full status dovecot | head -12 || true

log "Готово."
cat <<EOF

================================================================================
  Postfix + Dovecot для ${DOMAIN}
================================================================================
  Hostname:     ${HOSTNAME}
  Тестовый ящик: ${MAIL_USER}@${DOMAIN}
  Пароль:       сохранён в /root/mailuser-credentials.txt (не светите в чат)

  Порты UFW: 25, 143, 993, 465, 587 (+ SSH)

  Проверка локально:
    echo 'test' | mail -s 'hello' ${MAIL_USER}@localhost
    # или
    echo 'test' | mail -s 'hello' ${MAIL_USER}
    ls /home/${MAIL_USER}/Maildir/new/

  IMAP (с клиента): сервер ${HOSTNAME} или ${DOMAIN}, порт 143/993,
  логин ${MAIL_USER} или ${MAIL_USER}@${DOMAIN} — зависит от auth.

  DNS для приёма почты (у регистратора / BIND):
    MX  @  ->  ${HOSTNAME}.
    A   mail ->  <IP сервера>
    SPF / DKIM / DMARC — настройте отдельно для внешней доставки.

  Если снова поднимете docker-mailserver — будет конфликт портов.
================================================================================
EOF
