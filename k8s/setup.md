# Cluster setup (one-time, operator)

## 1. Install k3s on the control-plane node

```sh
curl -sfL https://get.k3s.io | sh -
```

## 2. Create the `claudulhu` namespace

```sh
kubectl apply -f k8s/namespace.yaml
```

## 3. Store the k3s join token as a K8s Secret

```sh
kubectl create secret generic k3s-join-token \
  --from-literal=token="$(cat /var/lib/rancher/k3s/server/node-token)" \
  -n claudulhu
```

## 4. Store API keys as a K8s Secret

```sh
kubectl create secret generic claudulhu-secrets \
  --from-literal=anthropic-api-key="<key>" \
  --from-literal=gh-token="<token>" \
  -n claudulhu
```

For remote EC2 provisioning also add:

```sh
kubectl patch secret claudulhu-secrets -n claudulhu \
  --patch='{"stringData":{"aws-access-key-id":"<id>","aws-secret-access-key":"<secret>"}}'
```

## 5. Create AWS prerequisites (if using remote provisioning)

| Resource | Notes |
|---|---|
| **Security Group** | Inbound: TCP 30100–30199 (Noise NodePorts), TCP 6443 (k3s join). Store ID in `AWS_SECURITY_GROUP_ID` env var |
| **Subnet** | Store ID in `AWS_SUBNET_ID` env var |

Set `K3S_CONTROL_PLANE_URL` to `https://<control-plane-ip>:6443` in the rulyeh Deployment.

## 6. Apply RBAC and deploy rulyeh

```sh
kubectl apply -f k8s/rbac.yaml
kubectl apply -f k8s/rulyeh.yaml
```

## Verify

```sh
kubectl get pods -n claudulhu
kubectl logs -n claudulhu deploy/rulyeh
```
