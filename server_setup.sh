#!/usr/bin/env bash
#
# One-time setup of a fresh Ubuntu server for the bidyut deployment.
#
# Usage (as root or with sudo, in an INTERACTIVE terminal):
#   sudo bash server-setup.sh 'ssh-ed25519 AAAA... you@laptop'
#
# The argument is your SSH PUBLIC key (the contents of ~/.ssh/id_ed25519.pub).
#
# What it does, in this order:
#   1. installs tools, creates a non-root "deploy" user and installs your key
#   2. enables the firewall (SSH, 80 and 443 only)
#   3. enables automatic security updates
#   4. installs Docker from Docker's official apt repository
#   5. asks you to confirm that a SECOND login works, and only then disables
#      SSH password login and root login
#
# Safe to re-run. Read it before you run it: it runs as root.

set -euo pipefail

DEPLOY_USER="${DEPLOY_USER:-deploy}"

log()  { printf '\n==> %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- preflight

[ "$(id -u)" -eq 0 ] || die "run as root (use sudo)"
[ -t 0 ] || die "run this in an interactive terminal (it asks questions)"
[ $# -eq 1 ] || die "usage: sudo bash $0 '<your ssh public key line>'"
PUBKEY="$1"

# shellcheck disable=SC1091
. /etc/os-release
[ "${ID:-}" = "ubuntu" ] || die "Ubuntu only (found: ${ID:-unknown})"
# VERSION_ID must be 22.04 or newer (older OpenSSH lacks the settings used below)
[ "$(printf '%s\n22.04\n' "${VERSION_ID:-0}" | sort -V | head -n1)" = "22.04" ] \
  || die "Ubuntu 22.04 or newer required (found: ${VERSION_ID:-unknown})"

case "$PUBKEY" in
  *PRIVATE*) die "that looks like a PRIVATE key. Never paste a private key anywhere." ;;
esac
command -v ssh-keygen >/dev/null || die "ssh-keygen not found"
tmp_key="$(mktemp)"
trap 'rm -f "$tmp_key"' EXIT
printf '%s\n' "$PUBKEY" > "$tmp_key"
[ "$(wc -l < "$tmp_key")" -eq 1 ] || die "the key must be a single line"
ssh-keygen -l -f "$tmp_key" >/dev/null 2>&1 || die "that is not a valid SSH public key"

# ----------------------------------------------- 1. packages, user, SSH key

log "Installing packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y ufw unattended-upgrades ca-certificates curl openssl

log "Creating user '$DEPLOY_USER'"
if ! id -u "$DEPLOY_USER" >/dev/null 2>&1; then
  adduser --disabled-password --gecos "" "$DEPLOY_USER"
fi
usermod -aG sudo "$DEPLOY_USER"

if [ "$(passwd -S "$DEPLOY_USER" | awk '{print $2}')" != "P" ]; then
  echo "Choose a password for '$DEPLOY_USER'."
  echo "It is only used for sudo. SSH password login will be disabled."
  passwd "$DEPLOY_USER"
  [ "$(passwd -S "$DEPLOY_USER" | awk '{print $2}')" = "P" ] || die "no password was set"
fi

log "Installing your SSH key for '$DEPLOY_USER'"
ssh_dir="/home/$DEPLOY_USER/.ssh"
install -d -m 700 -o "$DEPLOY_USER" -g "$DEPLOY_USER" "$ssh_dir"
touch "$ssh_dir/authorized_keys"
grep -qxF "$PUBKEY" "$ssh_dir/authorized_keys" || printf '%s\n' "$PUBKEY" >> "$ssh_dir/authorized_keys"
chown "$DEPLOY_USER:$DEPLOY_USER" "$ssh_dir/authorized_keys"
chmod 600 "$ssh_dir/authorized_keys"

# ------------------------------------------------------------ 2. firewall

log "Configuring the firewall"
SSH_PORT="$(sshd -T 2>/dev/null | awk '$1=="port"{print $2; exit}')"
[ -n "$SSH_PORT" ] || die "could not detect the SSH port"
# The SSH rule goes in BEFORE the firewall is switched on.
ufw default deny incoming
ufw default allow outgoing
ufw allow "${SSH_PORT}/tcp"
ufw allow 80/tcp
ufw allow 443/tcp
ufw --force enable
ufw status verbose
echo "(Ports published by Docker bypass ufw, so the compose file must publish only Caddy's 80 and 443.)"

# ------------------------------------------------- 3. automatic updates

log "Enabling automatic security updates"
cat > /etc/apt/apt.conf.d/20auto-upgrades <<'EOF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
EOF

# ------------------------------------------------------------ 4. Docker

if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  log "Docker is already installed: $(docker --version)"
else
  log "Installing Docker (official apt repository)"
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu ${UBUNTU_CODENAME:-$VERSION_CODENAME} stable" \
    > /etc/apt/sources.list.d/docker.list
  apt-get update -y
  apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
  systemctl enable --now docker
fi
docker --version
docker compose version
# '$DEPLOY_USER' is deliberately NOT added to the docker group: that group is root-equivalent.
# The deploy script uses sudo for docker instead.

# ---------------------------- 5. SSH hardening (only after you confirm)

cat <<EOF

--------------------------------------------------------------------
Before SSH password login and root login are switched off, confirm
that a NEW login works (this also proves the firewall rule is right).

Open a second terminal on your laptop and run:

    ssh ${DEPLOY_USER}@<this server's IP address>
    sudo whoami          <- it must print: root

Keep THIS session open until that works.
--------------------------------------------------------------------
EOF
read -r -p "Type YES when the second login worked (anything else skips the hardening): " answer
if [ "$answer" != "YES" ]; then
  warn "SSH hardening skipped. Password and root login are still enabled. Re-run this script when ready."
  exit 0
fi

grep -qE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config \
  || die "sshd_config does not include sshd_config.d; not touching SSH settings"

# Named 00-* on purpose: sshd uses the FIRST value it reads, so this file must
# sort before provider files such as 50-cloud-init.conf that may allow passwords.
hardening=/etc/ssh/sshd_config.d/00-hardening.conf
cat > "$hardening" <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
EOF

if ! sshd -t; then
  rm -f "$hardening"
  die "sshd rejected the new settings; the change was reverted"
fi
systemctl reload ssh 2>/dev/null || systemctl reload sshd

effective="$(sshd -T)"
for setting in 'passwordauthentication no' 'permitrootlogin no'; do
  printf '%s\n' "$effective" | grep -qx "$setting" \
    || die "'$setting' is not in effect; another config file overrides it. Check /etc/ssh/sshd_config.d/"
done

cat <<EOF

Done.

  - Password login and root login over SSH are now disabled.
  - Open ANOTHER terminal and confirm 'ssh ${DEPLOY_USER}@<ip>' still works
    before you close this one.
  - Next: copy the deploy files to the server and run deploy.sh.
EOF