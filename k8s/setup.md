# Cluster setup (one-time, operator)

## 1. Install k3s on the control-plane node

```sh
curl -sfL https://get.k3s.io | sh -
```

## 2. Create the `octo` namespace

```sh
kubectl apply -f k8s/namespace.yaml
```

## 3. Store the k3s join token as a K8s Secret

```sh
kubectl create secret generic k3s-join-token \
  --from-literal=token="$(cat /var/lib/rancher/k3s/server/node-token)" \
  -n octo
```

## 4. Store API keys as a K8s Secret

The Secret must be named `lair-secrets` and use UPPER_SNAKE_CASE keys; both
the lair Deployment and every child pod consume it via `envFrom`, so each
key becomes an env var of the same name verbatim. See
`k8s-ops/src/k8s.rs::upsert_secret` for the runtime mutation path lair uses.

```sh
kubectl create secret generic lair-secrets \
  --from-literal=ANTHROPIC_API_KEY="<key>" \
  --from-literal=GH_TOKEN="<token>" \
  -n octo
```

For remote EC2 provisioning also add:

```sh
kubectl patch secret lair-secrets -n octo \
  --patch='{"stringData":{"AWS_ACCESS_KEY_ID":"<id>","AWS_SECRET_ACCESS_KEY":"<secret>"}}'
```

## 5. Create AWS prerequisites (if using remote provisioning)

| Resource | Notes |
|---|---|
| **Security Group** | Inbound: TCP 30100–30199 (Noise NodePorts), TCP 6443 (k3s join). Store ID in `AWS_SECURITY_GROUP_ID` env var |
| **Subnet** | Store ID in `AWS_SUBNET_ID` env var |

Set `K3S_CONTROL_PLANE_URL` to `https://<control-plane-ip>:6443` in the lair Deployment.

## 6. Apply RBAC and deploy lair

```sh
kubectl apply -f k8s/rbac.yaml
kubectl apply -f k8s/lair.yaml
```

## Verify

```sh
kubectl get pods -n octo
kubectl logs -n octo deploy/lair
```
