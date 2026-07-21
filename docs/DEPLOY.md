# Deployment — Google Cloud free tier + GitHub Actions

Target: a single `e2-micro` VM (Always Free) running the screener behind Caddy,
redeployed automatically on every push to `main`.

- VM: `us-east1-d`, Ubuntu 24.04, e2-micro (2 shared vCPU / 1 GB), 30 GB standard PD
- Public IP: reserve it as static (see §1)
- Image registry: `ghcr.io/martynov-lab/crypto_server` (free for this repo)

The build happens in GitHub Actions, never on the VM — 1 GB RAM is not enough to
link this workspace with `lto = "thin"`.

## 1. VM preflight (once)

Confirm the instance is actually free-tier shaped, since the console defaults are
not. In Compute Engine → your instance → Edit:

| Setting | Required for free tier |
| --- | --- |
| Region | `us-central1`, `us-west1` or `us-east1` |
| Machine type | `e2-micro` |
| Boot disk | Standard persistent disk, ≤ 30 GB |

Then:

- **Static IP** — VPC network → IP addresses → your external IP → *Reserve*.
  Ephemeral addresses change on stop/start and break DNS.
- **Firewall** — VPC network → Firewall. The instance needs the `http-server` and
  `https-server` tags (the "Allow HTTP/HTTPS traffic" checkboxes). Without them
  port 443 is unreachable from outside while everything looks fine from inside.

## 2. DNS

Register a free subdomain at <https://duckdns.org> (GitHub login), point it at
the VM's static IP. Verify before continuing — Caddy cannot issue a certificate
until the A record resolves:

```bash
dig +short arb-screener.duckdns.org   # must print the VM IP
```

## 3. Server setup (once, over browser SSH)

```bash
# Swap — 1 GB RAM without swap will OOM during image pulls.
sudo fallocate -l 4G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab

# Docker
curl -fsSL https://get.docker.com | sudo sh
sudo usermod -aG docker "$USER"   # log out / back in for this to take effect

# Deployment directory
sudo mkdir -p /opt/arb && sudo chown "$USER" /opt/arb
```

Copy `deploy/docker-compose.yml` and `deploy/Caddyfile` into `/opt/arb/`, then
create `/opt/arb/.env` from `deploy/.env.example`:

```bash
openssl rand -hex 32          # -> ARB_AUTH_TOKEN
chmod 600 /opt/arb/.env
```

`.env` holds the client token; it never leaves the VM and is not in git.

First launch:

```bash
cd /opt/arb && docker compose up -d && docker compose logs -f
```

`https://<domain>/healthz` should answer within a minute (Caddy needs a few
seconds for the ACME challenge on first start).

## 4. Deploy key for GitHub Actions

On the VM:

```bash
ssh-keygen -t ed25519 -f ~/.ssh/gh_deploy -N ''
cat ~/.ssh/gh_deploy.pub >> ~/.ssh/authorized_keys
cat ~/.ssh/gh_deploy       # private key — copy this, then see below
```

In the repo → Settings → Secrets and variables → Actions:

| Secret | Value |
| --- | --- |
| `DEPLOY_HOST` | VM static IP |
| `DEPLOY_USER` | your Linux user on the VM |
| `DEPLOY_SSH_KEY` | contents of `~/.ssh/gh_deploy` (the private key) |
| `DEPLOY_DOMAIN` | e.g. `arb-screener.duckdns.org` |

GCE rewrites `authorized_keys` from instance metadata for OS Login-managed
users. If the key stops working after a reboot, add the public key under
Compute Engine → Metadata → SSH keys instead.

Finally, make the package pullable: after the first successful build, open
<https://github.com/martynov-lab/crypto_server/pkgs/container/crypto_server> and
either keep it private and `docker login ghcr.io` on the VM with a
`read:packages` PAT, or set the package visibility to public.

## 5. Steady state

Push to `main` → Actions runs `clippy` + `cargo test`, builds the image, pushes
it to GHCR, SSHes into the VM for `docker compose pull && up -d`, then polls
`/healthz` until it answers. A failing test or an unhealthy instance fails the
run before/after the swap respectively.

Manual operations on the VM:

```bash
cd /opt/arb
docker compose logs -f arb          # live logs
docker compose restart arb          # restart without redeploying
ARB_IMAGE=ghcr.io/martynov-lab/crypto_server:<sha> docker compose up -d   # roll back
```

## 6. Cost watch

Always Free covers the VM and its disk permanently. Two things are not free:

- **External IPv4** — ~$3/month.
- **Egress** — 1 GB/month free from North America, then ~$0.12/GB. A continuously
  streaming WS client costs a few hundred MB per day, so expect a small monthly
  charge; `encode` in the Caddyfile compresses the JSON stream ~5×.

Both are covered by the trial credits until they expire. Keep the $1 budget alert
enabled, and note that resources are **deleted** when the trial ends unless the
billing account has been upgraded to pay-as-you-go.
