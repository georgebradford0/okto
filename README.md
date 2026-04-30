# claudulhu

An agentic coding assistant. Rulyeh manages a fleet of per-repo coding assistant containers and exposes the client-facing interface. Clients connect via an encrypted Noise tunnel.

## Architecture

| Component | Role |
|---|---|
| **Rulyeh** | Master node — manages child containers, exposes the Noise WebSocket interface |
| **Child** | One per repo — handles the agentic coding loop for a single Git repository |

Both roles use the same image: `ghcr.io/georgebradford0/rulyeh:latest`

---

## Prerequisites

### k3s

Install k3s on your server (single node is fine):

```sh
curl -sfL https://get.k3s.io | sh -
```

`kubectl` is included. To use it without `sudo`:

```sh
mkdir -p ~/.kube
sudo cp /etc/rancher/k3s/k3s.yaml ~/.kube/config
sudo chown $USER ~/.kube/config
```

To manage the cluster from another machine, copy `~/.kube/config` to that machine and replace `127.0.0.1` with your server's IP.

### k8s/ manifests

Clone this repo (or just download the `k8s/` directory) onto any machine with `kubectl` access:

```sh
git clone https://github.com/georgebradford0/claudulhu
cd claudulhu
```

---

## Setup

### 1. Create the namespace and RBAC

```sh
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/rbac.yaml
```

### 2. Create secrets

```sh
kubectl create secret generic claudulhu-secrets \
  --from-literal=anthropic-api-key="sk-ant-..." \
  --from-literal=gh-token="ghp_..." \
  -n claudulhu
```

### 3. Store the k3s join token

Only needed if you want rulyeh to provision remote EC2 worker nodes on demand. Skip if you're running everything on a single node.

```sh
kubectl create secret generic k3s-join-token \
  --from-literal=token="$(cat /var/lib/rancher/k3s/server/node-token)" \
  -n claudulhu
```

### 4. Deploy rulyeh

```sh
kubectl apply -f k8s/rulyeh.yaml
```

### 5. Get the QR code

```sh
kubectl logs -n claudulhu deploy/rulyeh
```

Scan the printed QR code with the mobile app. It establishes an encrypted Noise Protocol tunnel (NodePort 30090 by default) and routes all traffic through it.

---

## Creating a child container

Once connected in the app, ask rulyeh in chat:

> "Create a container for https://github.com/user/repo"

Rulyeh will create a Kubernetes Deployment, two PVCs (`/data` and `/workspace`), a ClusterIP Service for internal messaging, and a NodePort Service for your Noise connection — all automatically. NodePorts are assigned from the range **30100–30199**.

The child's QR code appears in its own pod logs once it starts:

```sh
kubectl logs -n claudulhu deploy/rulyeh-<repo-name>
```

### Remote EC2 children

To run a child on a fresh EC2 instance provisioned on demand, some extra setup is required first.

**Add AWS credentials to the secret:**

```sh
kubectl patch secret claudulhu-secrets -n claudulhu \
  --patch='{"stringData":{
    "aws-access-key-id":     "<id>",
    "aws-secret-access-key": "<secret>"
  }}'
```

**Add these env vars to the rulyeh Deployment in `k8s/rulyeh.yaml`:**

```yaml
- name: AWS_SECURITY_GROUP_ID
  value: "sg-xxxxxxxxxxxxxxxxx"   # inbound: TCP 30100–30199 + TCP 6443
- name: AWS_SUBNET_ID
  value: "subnet-xxxxxxxxxxxxxxxxx"
- name: K3S_CONTROL_PLANE_URL
  value: "https://<control-plane-ip>:6443"
- name: AWS_DEFAULT_REGION
  value: "us-east-1"
```

Then redeploy: `kubectl apply -f k8s/rulyeh.yaml`

**Then ask rulyeh:**

> "Create a container for https://github.com/user/repo on a t3.medium"

Rulyeh launches the EC2 instance, waits for it to join the cluster (~60s), then schedules the child pod on it. Total time is roughly 2–3 minutes. When you're done, asking rulyeh to terminate the container also terminates the EC2 instance.

---

## Environment variables

### Rulyeh

| Variable | Required | Description |
|---|---|---|
| `ANTHROPIC_API_KEY` | Yes | Anthropic API key |
| `GH_TOKEN` | Yes* | GitHub token — passed to every child |
| `PUBLIC_HOST` | No | Public IP/hostname for the QR code. Auto-detected if not set. |
| `NOISE_PORT` | No | Noise endpoint port (default: `9000`) |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | If remote | EC2 provisioning credentials |
| `AWS_DEFAULT_REGION` | If remote | EC2 region (default: `us-east-1`) |
| `AWS_SECURITY_GROUP_ID` | If remote | Security group for new EC2 instances |
| `AWS_SUBNET_ID` | If remote | Subnet for new EC2 instances |
| `K3S_CONTROL_PLANE_URL` | If remote | e.g. `https://<ip>:6443` — used in EC2 user-data for k3s agent join |

*`GH_TOKEN` is technically optional but required in practice for any GitHub repo work.

### Child containers

| Variable | Required | Description |
|---|---|---|
| `GIT_URL` | Yes | Repository to clone |
| `ANTHROPIC_API_KEY` | Yes | Anthropic API key |
| `GH_TOKEN` | No | GitHub token (required for private repos and PR creation) |
| `PUBLIC_HOST` | No | Public IP/hostname for the QR code. Auto-detected if not set. |
| `NOISE_PORT` | No | Noise endpoint port (default: `9000`) |
| `GIT_USER_NAME` / `GIT_USER_EMAIL` | No | Git commit author identity |
| `STARTUP_SCRIPT` | No | Shell script run before the server starts |
| `STARTUP_PROMPT` | No | Initial prompt sent to the agentic loop on startup |

---

## Git authentication

**HTTPS** (`https://github.com/user/repo`)
- Authenticated via `GH_TOKEN` — used for both clone and push

**SSH** (`git@github.com:user/repo.git`)
- Authenticated via SSH key mounted at `/root/.ssh/id_rsa`
- `GH_TOKEN` is ignored when using SSH URLs

---

## PR/MR creation

The agentic loop has a `create_pull_request` tool that opens a GitHub PR or GitLab MR after pushing a branch. Requires `GH_TOKEN` with `repo` (GitHub) or `api` (GitLab) scope. Not available when using SSH authentication.
