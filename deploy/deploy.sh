#!/usr/bin/env bash
#
# Deploys bidyut on the server.
# Lives next to docker-compose.prod.yml and Caddyfile (for example in ~/bidyut-deploy/).
# Run it as the deploy user; it uses sudo for docker.
#
# Usage:
#   ./deploy.sh init <domain>        once: create .env.prod with fresh random secrets
#   ./deploy.sh deploy <image-tag>   pull and start a version (40-char commit SHA or vX.Y.Z)
#   ./deploy.sh rollback             redeploy the previous version
#   ./deploy.sh status               show the containers
#   ./deploy.sh logs [service]       show recent logs

set -euo pipefail
umask 077
cd "$(dirname "${BASH_SOURCE[0]}")"

ENV_FILE=.env.prod
COMPOSE_FILE=docker-compose.prod.yml
PREV_FILE=.previous-image-tag
DOMAIN=""

log()  { printf '\n==> %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

usage() {
  sed -n '3,15p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 1
}

dc() { sudo docker compose --env-file "$ENV_FILE" -f "$COMPOSE_FILE" "$@"; }

valid_domain() { [[ "$1" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ && "$1" == *.* ]]; }
valid_tag()    { [[ "$1" =~ ^([0-9a-f]{40}|v[0-9]+\.[0-9]+\.[0-9]+)$ ]]; }

get_env() { { grep -E "^$1=" "$ENV_FILE" || true; } | head -n1 | cut -d= -f2-; }

set_tag() {
  local tmp
  tmp="$(mktemp .env.XXXXXX)"
  { grep -v '^IMAGE_TAG=' "$ENV_FILE" || true; } > "$tmp"
  printf 'IMAGE_TAG=%s\n' "$1" >> "$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$ENV_FILE"
}

# ------------------------------------------------------------------- init

cmd_init() {
  [ $# -eq 1 ] || usage
  valid_domain "$1" || die "'$1' is not a valid domain name"
  [ ! -e "$ENV_FILE" ] || die "$ENV_FILE already exists. Not overwriting: changing the database password later breaks an existing database."
  command -v openssl >/dev/null || die "openssl is not installed"

  {
    echo "DOMAIN=$1"
    echo "IMAGE_TAG=unset"
    echo "POSTGRES_PASSWORD=$(openssl rand -hex 32)"
    echo "REDIS_PASSWORD=$(openssl rand -hex 32)"
    echo "DEVICE_TOKEN_SECRET=$(openssl rand -hex 32)"
    echo "TELEGRAM_BOT_TOKEN="
  } > "$ENV_FILE"
  chmod 600 "$ENV_FILE"

  log "Created $ENV_FILE (mode 600) with new random secrets."
  echo "Store a copy of it in a password manager. Losing DEVICE_TOKEN_SECRET invalidates every issued device token."
  echo "Optional: put the PRODUCTION bot's token after TELEGRAM_BOT_TOKEN= in $ENV_FILE (blank = bot disabled)."
  echo "Next: ./deploy.sh deploy <full-commit-sha-from-the-GitHub-Packages-page>"
}

# -------------------------------------------------------------- preflight

preflight() {
  command -v docker >/dev/null || die "docker is not installed (run server-setup.sh first)"
  command -v curl >/dev/null   || die "curl is not installed"
  [ -f "$COMPOSE_FILE" ] || die "$COMPOSE_FILE not found next to this script"
  [ -f Caddyfile ]       || die "Caddyfile not found next to this script"
  [ -f "$ENV_FILE" ]     || die "$ENV_FILE not found. Run: ./deploy.sh init <domain>"

  local key
  for key in DOMAIN POSTGRES_PASSWORD REDIS_PASSWORD DEVICE_TOKEN_SECRET; do
    [ -n "$(get_env "$key")" ] || die "$key is empty in $ENV_FILE"
  done

  if [ "$(stat -c '%a' "$ENV_FILE")" != "600" ]; then
    warn "$ENV_FILE had loose permissions; setting mode 600"
    chmod 600 "$ENV_FILE"
  fi

  [ -n "$(get_env TELEGRAM_BOT_TOKEN)" ] \
    || warn "TELEGRAM_BOT_TOKEN is empty in $ENV_FILE: the Telegram bot will be disabled."

  DOMAIN="$(get_env DOMAIN)"
  valid_domain "$DOMAIN" || die "DOMAIN in $ENV_FILE is not a valid domain name"
  getent hosts "$DOMAIN" >/dev/null \
    || warn "$DOMAIN does not resolve yet. Caddy cannot get a certificate until its DNS A record points to this server."
}

# ----------------------------------------------------------------- checks

wait_healthy() {
  log "Waiting for the app to become healthy (up to 120 s)"
  local cid status i
  cid="$(dc ps -q app)"
  [ -n "$cid" ] || die "app container not found"
  for i in $(seq 1 60); do
    status="$(sudo docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$cid")"
    case "$status" in
      healthy)   echo "app is healthy"; return 0 ;;
      unhealthy) break ;;
    esac
    sleep 2
  done
  dc logs --tail 50 app || true
  die "the app did not become healthy. To go back: ./deploy.sh rollback"
}

smoke_test() {
  log "Checking https://$DOMAIN/readyz (a first deploy needs DNS and a certificate, so this retries)"
  local i body
  for i in $(seq 1 12); do
    if body="$(curl -fsS --max-time 10 "https://$DOMAIN/readyz" 2>/dev/null)" \
       && printf '%s' "$body" | grep -q '"database":true'; then
      echo "readyz: $body"
      printf '%s' "$body" | grep -q '"redis":true' \
        || warn "Redis is not reachable: rate limiting is failing open"
      return 0
    fi
    sleep 5
  done
  dc logs --tail 30 caddy || true
  die "https://$DOMAIN/readyz did not return a healthy answer. Check the DNS A record, ports 80/443 and the caddy logs above."
}

check_ports() {
  log "Checking that only Caddy publishes ports"
  local svc cid ports bad=0
  for svc in app postgres redis; do
    cid="$(dc ps -q "$svc")"
    if [ -z "$cid" ]; then
      warn "$svc container not found"
      bad=1
      continue
    fi
    ports="$(sudo docker port "$cid" 2>/dev/null || true)"
    if [ -n "$ports" ]; then
      warn "$svc publishes ports: $ports"
      bad=1
    fi
  done
  [ "$bad" -eq 0 ] || die "unexpected published ports (see above)"
  echo "ok: app, postgres and redis publish nothing"
}

# ----------------------------------------------------------------- deploy

deploy_tag() {
  local tag="$1" current
  valid_tag "$tag" || die "'$tag' is not a 40-character commit SHA or a vX.Y.Z tag"
  preflight

  current="$(get_env IMAGE_TAG)"
  if [ -n "$current" ] && [ "$current" != "unset" ] && [ "$current" != "$tag" ]; then
    printf '%s\n' "$current" > "$PREV_FILE"
  fi
  set_tag "$tag"

  dc config -q || die "the compose file does not validate"

  log "Pulling images"
  if ! dc pull; then
    warn "If the image is private, log in first:  sudo docker login ghcr.io -u <github-user>"
    warn "(use a token with ONLY the read:packages scope)"
    die "pull failed"
  fi

  log "Starting the stack"
  if ! dc up -d --remove-orphans; then
    dc logs --tail 50 app || true
    die "start failed. To go back: ./deploy.sh rollback"
  fi

  wait_healthy
  smoke_test
  check_ports

  log "Deployed $tag"
  echo "Still to check by hand, from your LAPTOP (all four must fail or time out):"
  echo "  nc -zv -w 5 <server-ip> 5432; nc -zv -w 5 <server-ip> 6379; nc -zv -w 5 <server-ip> 3000; nc -zv -w 5 <server-ip> 9090"
}

cmd_rollback() {
  [ -f "$PREV_FILE" ] || die "no previous version recorded"
  local prev
  prev="$(cat "$PREV_FILE")"
  valid_tag "$prev" || die "the recorded previous tag is invalid"
  warn "Rolling back to $prev. Migrations only move forward: this is safe only if the older code works with the current database schema."
  deploy_tag "$prev"
}

# ------------------------------------------------------------------- main

case "${1:-}" in
  init)     shift; cmd_init "$@" ;;
  deploy)   shift; [ $# -eq 1 ] || usage; deploy_tag "$1" ;;
  rollback) cmd_rollback ;;
  status)   preflight; dc ps ;;
  logs)     shift; preflight; dc logs --tail 100 "$@" ;;
  *)        usage ;;
esac
