# cni-config

Watches this node's own `Node.spec.podCIDRs` and writes a `bridge`/`host-local`/`loopback` CNI
conflist for it (`/etc/cni/net.d/10-bridge.conflist`). `bridge`, `host-local`, and `loopback` are
already present in `/opt/cni/bin` on every Talos node - this is the only piece that's actually
missing, and it needs a live Kubernetes API to do its job (`podCIDR` only exists once a node has
already joined a running cluster), so it's a plain Kubernetes DaemonSet, not a Talos extension.

Cross-node pod-to-pod routing (announcing each node's pod-bridge subnet over the mesh's iBGP full
mesh) is a separate concern, handled by `talos-extensions`' `router` extension - see that repo's
`router/templates/bird.conf` for the `direct_cni` protocol block. This repo only ever writes one
file; it does not touch routing.

## Deploy

Via Talos's own `cluster.network.cni` machine config (the same mechanism used for Cilium/Calico):

```yaml
cluster:
  network:
    cni:
      name: custom
      urls:
        - https://github.com/slipmesh/cni-config/releases/download/vX.Y.Z/daemonset.yaml
```

Applied once at bootstrap via `kubectl apply -f` under the hood - same as any other manifest URL.

## Release

Push a `vX.Y.Z` tag. CI (`.github/workflows/release.yml`) cross-compiles for amd64+arm64, pushes a
multi-arch image to `ghcr.io/slipmesh/cni-config:vX.Y.Z` (and `:latest`), and creates a GitHub
Release for the same tag with `deploy/daemonset.yaml` attached as a downloadable asset - that
release-asset URL is what `cluster.network.cni.urls` points at.

## Local development

```sh
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

No live cluster needed for any of the above - `conflist.rs`'s rendering is pure. Testing the
watcher (`main.rs`) end-to-end needs a real Kubernetes API server (`kube::Client::try_default()`
uses in-cluster config; point `KUBECONFIG` at a real or kind/k3d cluster to run it standalone).
