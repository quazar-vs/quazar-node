FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive \
    MAIL_DOMAIN=quazar.website \
    MAIL_HOSTNAME=mail.quazar.website \
    POSTMASTER=no-reply@quazar.website

RUN apt-get update && apt-get install -y --no-install-recommends \
        postfix \
        dovecot-imapd \
        dovecot-pop3d \
        dovecot-lmtpd \
        ca-certificates \
        openssl \
        netcat-openbsd \
        procps \
        rsyslog \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /var/mail/vhosts /etc/dovecot/users /etc/postfix/maps /etc/ssl/mail \
    && groupadd -g 5000 vmail \
    && useradd -g vmail -u 5000 vmail -d /var/mail/vhosts -M \
    && chown -R vmail:vmail /var/mail/vhosts

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
COPY setup /usr/local/bin/setup
COPY conf/postfix-main.cf /etc/postfix/main.cf.template
COPY conf/10-master.conf /etc/dovecot/conf.d/10-master.conf
COPY conf/99-quazar.conf /etc/dovecot/conf.d/99-quazar.conf

RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/setup

EXPOSE 25 143 465 587 993

HEALTHCHECK --interval=20s --timeout=5s --retries=5 --start-period=40s \
  CMD nc -z 127.0.0.1 25 && nc -z 127.0.0.1 587 || exit 1

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
