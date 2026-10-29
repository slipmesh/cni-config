# cni-config

Watches this node's own `Node.spec.podCIDRs` and writes a `bridge`/`host-local`/`loopback` CNI
conflist for it (`/etc/cni/net.d/10-bridge.conflist`). `bridge`, `host-local` and `loopback` are
already present in `/opt/cni/bin` on every Talos node - this file is the only piece that's
actually missing, and writing it needs a live Kubernetes API (`podCIDR` only exists once a node
has joined a running cluster), so this is a plain Kubernetes DaemonSet, not a Talos extension.

The result is the smallest pod network that works: a bridge per node, addresses handed out from
that node's own podCIDR, and no overlay of its own.

## What it does not do

Cross-node pod-to-pod routing is a separate concern and is not handled here - this repo only ever
writes one file, and never touches routing. Something has to carry each node's podCIDR to the
other nodes; in slipmesh that's the `router` extension
([talos-extensions](https://github.com/slipmesh/talos-extensions)), which announces the local
podCIDR over iBGP by watching the kernel routing table for routes falling inside the configured
pod subnet - a prefix, not an interface name. So it neither knows nor cares that the route came
from this bridge, and swapping this DaemonSet for a different CNI doesn't touch the routing side,
as long as the replacement leaves a route to the node's podCIDR in the kernel table.

A cluster does need *a* CNI, though - dropping this one without putting something in its place
leaves pods with no networking at all.

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
Talos only applies these at bootstrap and never removes what it has already applied, so on an
existing cluster `kubectl apply -f <same URL>` does the same job, and a GitOps tool can own the
manifest instead.

Each release's `daemonset.yaml` asset pins the image to that same release tag, so a given URL
always deploys exactly one, immutable version.

## Memory report

Every ten minutes the binary logs a `memory` line at debug level: the live heap as counted at the
allocator, `RssAnon` and `RssFile` from `/proc/self/status`, and the Node watch events and errors
seen since start. A heap that grows is memory the code keeps; a flat heap under a growing
`RssAnon` is memory the allocator keeps. It is off by default; to turn it on without the rest of
the debug output, set the container's `RUST_LOG` to `info,cni_config::memstats=debug`.

## Release

Run the Release PR workflow (Actions, then Release PR). It opens `chore: release vX.Y.Z`, which
sets `version` in `Cargo.toml` and the image tag in `deploy/daemonset.yaml` to the next version the
commits since the last tag call for, and regenerates `CHANGELOG.md` with git-cliff: the binary
reports that version in its first log line, and a tag that moves alone leaves it misnaming itself.

Merging that PR releases it. Only that push builds: it builds the binary, pushes a multi-arch image
as `ghcr.io/slipmesh/cni-config:sha-<commit>`, then tags the commit `vX.Y.Z` and retags that image
as `:vX.Y.Z` and, unless it is a pre-release, `:latest`. A build that fails leaves no tag. The run
then rewrites `deploy/daemonset.yaml`'s image reference to the tag and creates a GitHub Release with
that manifest attached as a downloadable asset - that release-asset URL is what
`cluster.network.cni.urls` points at.

Every other push and pull request runs only the checks, and `cargo audit` also runs weekly. The
release build waits for every check of its own push, `cargo audit` included; an advisory with no fix
yet is ignored in `.cargo/audit.toml`, with the reason next to it.

## Local development

```sh
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

CI runs clippy and the tests against the musl target the image ships, on each arch's own runner:
`cargo test --target <arch>-unknown-linux-musl` locally needs the `musl-gcc` wrapper (Debian/Ubuntu
`musl-tools`) for aws-lc-sys's C part.

No live cluster needed for any of the above - `conflist.rs`'s rendering is pure. Testing the
watcher (`main.rs`) end-to-end needs a real Kubernetes API server (`kube::Client::try_default()`
uses in-cluster config; point `KUBECONFIG` at a real or kind/k3d cluster to run it standalone).

## License

MIT or Apache-2.0, at your option - see [`LICENSE-MIT`](./LICENSE-MIT) and
[`LICENSE-APACHE`](./LICENSE-APACHE).
