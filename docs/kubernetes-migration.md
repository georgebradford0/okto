# Kubernetes Migration Plan

## Overview

Migrate lair's container management from direct Docker CLI calls to Kubernetes, and add the ability to provision remote EC2 worker nodes that join the cluster on demand. Once a remote node joins, its child workload is a normal K8s Pod — discovery, pubkey exec, scaling, and messaging all work identically to local children.

---

## Assumptions

- Self-hosted **k3s** cluster (single control-plane, any number of workers)
- Single `octo` namespace for all workloads
- lair runs inside the cluster as a Pod with an in-cluster ServiceAccount
- AWS is the only supported cloud provider for now (others added later)
- No LoadBalancers — NodePort for all external Noise connections
- Pure Rust `kube` crate for all K8s API calls (no `kubectl` subprocess)
- AWS CLI (`aws`) subprocess for EC2 operations (simpler than `aws-sdk-rust` for now)

---

## Cluster setup (one-time, operator)

### 1. Install k3s on the control-plane node

```sh
curl -sfL https://get.k3s.io | sh -
```

### 2. Create the `octo` namespace

```sh
kubectl apply -f k8s/namespace.yaml
```

### 3. Store the k3s join token as a K8s Secret

```sh
kubectl create secret generic k3s-join-token \
  --from-literal=token="$(cat /var/lib/rancher/k3s/server/node-token)" \
  -n octo
```

This is how lair reads the join token at runtime — works whether lair is on the control-plane node or not.

### 4. Store API keys as a K8s Secret

```sh
kubectl create secret generic octo-secrets \
  --from-literal=anthropic-api-key="<key>" \
  --from-literal=gh-token="<token>" \
  -n octo
```

### 5. Create AWS prerequisites (one-time, manual)

| Resource | Notes |
|---|---|
| **Security Group** | Inbound: TCP 30100–30199 (Noise NodePorts), TCP 6443 (k3s agent join), TCP 22 (optional SSH). Store the SG ID in env var `AWS_SECURITY_GROUP_ID` |
| **Subnet** | The subnet to place new instances in. Store in `AWS_SUBNET_ID` |
| **IAM instance profile** | Optional. Useful for SSM access without SSH keys |

### 6. Apply RBAC and deploy lair

```sh
kubectl apply -f k8s/rbac.yaml
kubectl apply -f k8s/lair.yaml
```

---

## New environment variables for lair

| Variable | Required | Purpose |
|---|---|---|
| `AWS_ACCESS_KEY_ID` | if using remote | EC2 provisioning |
| `AWS_SECRET_ACCESS_KEY` | if using remote | EC2 provisioning |
| `AWS_DEFAULT_REGION` | if using remote | Region for EC2 instances (default: `us-east-1`) |
| `AWS_SECURITY_GROUP_ID` | if using remote | SG applied to new instances |
| `AWS_SUBNET_ID` | if using remote | Subnet to place new instances in |
| `K3S_CONTROL_PLANE_URL` | if using remote | e.g. `https://<node-ip>:6443` — embedded in user-data for agent join |

---

## Kubernetes manifests (`k8s/` directory)

### Static manifests (committed to repo)

| File | Contents |
|---|---|
| `k8s/namespace.yaml` | `octo` Namespace |
| `k8s/rbac.yaml` | ServiceAccount `lair` + Role + RoleBinding |
| `k8s/secret.example.yaml` | Non-secret template showing expected keys (no real values) |
| `k8s/lair.yaml` | lair Deployment + PVC (`/data`) + ClusterIP Service (port 8000) + NodePort Service (port 9000) |
| `k8s/setup.md` | Operator guide (mirrors this document, concise step-by-step) |

### Dynamic resources (created by lair at runtime per child)

For a child named `lair-myrepo`:

| Resource | Name | Notes |
|---|---|---|
| PVC | `lair-myrepo-data` | `ReadWriteOnce`, mounted at `/data` |
| PVC | `lair-myrepo-workspace` | `ReadWriteOnce`, mounted at `/workspace` |
| Deployment | `lair-myrepo` | 1 replica, labels `octo.managed=1` + `octo.git_url=<url>` |
| Service (ClusterIP) | `lair-myrepo` | Port 8000, internal HTTP for `message_child` and `LAIR_URL` |
| Service (NodePort) | `lair-myrepo-noise` | Port 9000 → NodePort 301xx, external Noise connection |

For remote children, the Deployment also gets a `nodeSelector` pinning it to the provisioned EC2 node.

---

## RBAC — required verbs

ServiceAccount `lair` in namespace `octo`:

| Resource | Verbs |
|---|---|
| `deployments` | get, list, watch, create, patch, update, delete |
| `pods` | get, list, watch |
| `pods/exec` | create |
| `services` | get, list, create, delete |
| `persistentvolumeclaims` | get, list, create, delete |
| `nodes` | get, list, watch, patch, delete |
| `secrets` | get (for `k3s-join-token`) |

`nodes` is cluster-scoped, so the Role must be a `ClusterRole` with a `ClusterRoleBinding` scoped to the ServiceAccount. The namespace-scoped resources use a regular `Role` + `RoleBinding`.

---

## Port strategy

NodePort range: **30100–30199** (100 children max, same limit as the old Docker 9100–9199 range).

lair assigns NodePorts by listing existing `*-noise` Services in the namespace and finding the lowest free port in that range. The NodePort is stored in the Service spec and readable at any time.

All child Noise traffic arrives at the K8s node's public IP on the assigned NodePort. Since children are always co-located on cluster nodes (not truly remote in terms of connectivity), this works the same as the old Docker port-publish model.

---

## Remote EC2 node provisioning flow

Triggered when `create_container` is called with `remote: true`.

### Steps

1. **Read join token** — lair reads the `k3s-join-token` Secret via the K8s API.

2. **Select AMI** — lair calls:
   ```sh
   aws ec2 describe-images \
     --owners 099720109477 \
     --filters "Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*" \
               "Name=state,Values=available" \
     --query "sort_by(Images, &CreationDate)[-1].ImageId" \
     --output text
   ```
   This always selects the latest Ubuntu 24.04 LTS AMI for the configured region.

3. **Launch instance** — lair calls `aws ec2 run-instances` with:
   - `--image-id <ami>`
   - `--instance-type <user-specified>`
   - `--security-group-ids <AWS_SECURITY_GROUP_ID>`
   - `--subnet-id <AWS_SUBNET_ID>`
   - `--associate-public-ip-address`
   - `--tag-specifications` including `octo.managed=1`, `octo.child-name=<name>`
   - `--user-data <script>` (see below)

4. **User-data script** embedded at launch time:
   ```sh
   #!/bin/bash
   set -e
   curl -sfL https://get.k3s.io | \
     K3S_URL=<K3S_CONTROL_PLANE_URL> \
     K3S_TOKEN=<join-token> \
     sh -
   ```
   The join token and control-plane URL are interpolated by lair before sending. The instance joins the cluster as a worker node within ~60s.

5. **Poll for public IP** — lair polls `aws ec2 describe-instances --instance-ids <id>` every 5s until state is `running` and a public IP is assigned (typically 20–40s).

6. **Wait for node Ready** — lair watches `Api::<Node>` until a node appears with label `octo.child-name=<name>` (set via k3s node label, added to user-data: `K3S_NODE_LABEL="octo.child-name=<name>"`) and its `Ready` condition is `True`. Timeout: 3 minutes.

7. **Label the node** — lair patches the Node with:
   - `octo.ec2-instance-id=<instance-id>`
   - `octo.child-name=<child-name>`

8. **Create child resources** — same as local flow (PVCs, Deployment, Services), but the Deployment spec includes:
   ```yaml
   nodeSelector:
     octo.child-name: <child-name>
   ```

9. **Return to user** — child name, NodePort, EC2 public IP.

---

## `create_container` tool — updated parameters

| Parameter | Type | Required | Notes |
|---|---|---|---|
| `git_url` | string | yes | |
| `name` | string | no | Defaults to `lair-<repo-slug>` |
| `noise_port` | int | no | NodePort 30100–30199, auto-assigned if omitted |
| `remote` | bool | no | Default `false`. If `true`, provisions an EC2 node first |
| `instance_type` | string | if remote | e.g. `t3.medium`, `c5.xlarge` — user specifies in chat |
| `startup_script` | string | no | Shell script run before server starts |
| `startup_prompt` | string | no | Initial prompt sent to child agent on startup |

---

## Lifecycle operations

### Start (resume) a stopped child
Scale the Deployment to 1 replica:
```rust
deployments.patch(name, &PatchParams::default(),
    &Patch::MergePatch(json!({"spec": {"replicas": 1}})))
```
For remote children, the EC2 instance is already running (it was never stopped). The Pod simply reschedules onto the same node.

### Stop a child (suspend, keep resources)
Scale the Deployment to 0 replicas. The EC2 instance (if remote) keeps running. PVCs are preserved. The child can be resumed cheaply.

### Terminate a child (full teardown)
1. Scale Deployment to 0
2. Delete Deployment, Services, PVCs
3. If remote:
   - Call `aws ec2 terminate-instances --instance-ids <id>` (reads instance ID from node label)
   - Delete the Node object from K8s: `Api::<Node>::delete(node_name)`

A `terminate_container` tool is added alongside the existing `create_container` tool. The `/start` HTTP endpoint maps to "scale to 1".

---

## Child discovery changes

`list_managed_deployments()` replaces `fetch_managed_containers()`:

- `Api::<Deployment>::namespaced(client, "octo").list(ListParams::default().labels("octo.managed=1"))`
- `git_url` from Deployment label `octo.git_url`
- `NOISE_PORT` from container env in pod template spec
- Status mapped from `deployment.status`: `available_replicas > 0` → `"running"`, `replicas == 0` → `"stopped"`, otherwise `"pending"`
- NodePort read from the associated `*-noise` Service
- For remote children: public IP read from the associated Node's `status.addresses` (type `ExternalIP`)

`ContainerInfo` gains two optional fields:
```rust
remote: bool,
instance_id: Option<String>,  // EC2 instance ID if remote
```
These are included in the existing `container_list` wire frame at no protocol cost (additive JSON fields, mobile ignores unknown fields).

---

## Pubkey fetch

`fetch_pubkey_via_exec()` replaces `docker exec`:

1. Find the running Pod for the Deployment: `Api::<Pod>::list` with label selector `app=<child-name>`, filter for phase `Running`
2. `pods.exec(pod_name, ["octo-server", "--print-pubkey"], &AttachParams::default().stdout(true))`
3. Cache result in `/data/pubkey_registry.json` as before

---

## Rust code structure

```
lair/src/
  main.rs     — AppState gains kube::Client; Docker tool executor replaced with k8s/aws calls
  k8s.rs      — all kube API operations (list, exec, create resources, scale, delete, watch node)
  aws.rs      — EC2 operations via aws CLI subprocess (run_instances, describe_instances,
                  describe_images, terminate_instances)
```

### New dependencies in `lair/Cargo.toml`

```toml
kube = { version = "0.99", features = ["runtime", "derive"] }
k8s-openapi = { version = "0.24", features = ["v1_32"] }
```

No new dependency for AWS — `aws` CLI is already in the image and credentials are already passed via environment variables.

---

## Dockerfile changes

In `lair/Dockerfile` runtime stage:
- **Remove** the `docker-ce-cli` apt package and the Docker apt repo setup
- **Verify** `awscli` is installed (add `awscli` to the apt install block if not already present)
- No `kubectl` binary needed

In `lair/docker-entrypoint.sh`:
- **Remove** `docker network create octo-net` line
- **Remove** `docker network connect octo-net "$(hostname)"` line
- Everything else stays the same

---

## What does not change

| Component | Status |
|---|---|
| `core/` library | Untouched |
| `server/` binary and entrypoint | Untouched |
| `server/docker-entrypoint.sh` | Untouched |
| Noise transport and handshake | Untouched |
| WebSocket protocol and all message types | Untouched |
| `ContainerInfo` wire format | Additive only (`remote`, `instance_id` fields) |
| Pubkey registry cache (`/data/pubkey_registry.json`) | Untouched |
| Mobile app | Untouched |
| `docker-compose.yml` (still useful for local dev without K8s) | Untouched |

---

## Implementation order

1. Add `kube` + `k8s-openapi` to `lair/Cargo.toml`
2. Write `lair/src/k8s.rs` — local child lifecycle (list, exec, create, scale, delete)
3. Replace Docker tool executor in `main.rs` with `k8s.rs` calls
4. Write K8s manifests (`k8s/`)
5. Update `lair/Dockerfile` (remove docker-ce-cli, add awscli)
6. Update `lair/docker-entrypoint.sh` (remove network setup)
7. Write `lair/src/aws.rs` — EC2 provisioning
8. Extend `create_container` tool with `remote`/`instance_type` parameters
9. Add `terminate_container` tool
10. Update `CLAUDE.md` and `docs/lair-architecture.md`
