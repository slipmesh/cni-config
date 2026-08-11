//! Watches this node's own `Node` object and keeps `/etc/cni/net.d/10-bridge.conflist` in sync
//! with `spec.podCIDRs` - see `conflist.rs`'s module doc comment for the rendering itself. A plain
//! Kubernetes DaemonSet, not a Talos extension: `podCIDR` only exists once a node has already
//! joined a running cluster (kube-controller-manager's own `--allocate-node-cidrs` allocation), so
//! unlike `talos-extensions`' `router`/`awg`/`nftables` (which must work *before* Kubernetes is up)
//! a live API dependency here is the natural boundary, not a compromise.
//!
//! Watches rather than reads once: `Node.spec.podCIDRs` is documented as immutable once set, but
//! this is a long-lived pod, not a one-shot job, so watching costs nothing and doesn't depend on
//! that invariant holding forever (a manually edited/recreated Node could still change it).

mod conflist;

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client, ResourceExt};

const CONFLIST_PATH: &str = "/etc/cni/net.d/10-bridge.conflist";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .without_time()
        .with_ansi(false)
        .init();

    let node_name =
        std::env::var("NODE_NAME").context("NODE_NAME env var must be set (downward API)")?;
    tracing::info!(node_name, "starting cni-config");

    let client = Client::try_default()
        .await
        .context("failed to build in-cluster Kubernetes client")?;
    let nodes: Api<Node> = Api::all(client);

    let config = watcher::Config::default().fields(&format!("metadata.name={node_name}"));
    let stream = watcher(nodes, config).default_backoff();
    let mut stream = std::pin::pin!(stream);

    let mut last_written: Option<Vec<String>> = None;
    while let Some(event) = stream.next().await {
        let node = match event {
            Ok(watcher::Event::Apply(node)) => node,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "watch stream error, retrying");
                continue;
            }
        };

        let Some(pod_cidrs) = node.spec.as_ref().and_then(|s| s.pod_cidrs.clone()) else {
            tracing::debug!(node = node.name_any(), "no spec.podCIDRs yet, waiting");
            continue;
        };
        if last_written.as_ref() == Some(&pod_cidrs) {
            continue;
        }

        match conflist::render_conflist(&pod_cidrs) {
            Ok(rendered) => match std::fs::write(CONFLIST_PATH, &rendered) {
                Ok(()) => {
                    tracing::info!(?pod_cidrs, path = CONFLIST_PATH, "CNI conflist written");
                    last_written = Some(pod_cidrs);
                }
                Err(e) => {
                    tracing::error!(error = %e, path = CONFLIST_PATH, "failed to write CNI conflist");
                }
            },
            Err(e) => tracing::error!(error = %e, ?pod_cidrs, "failed to render CNI conflist"),
        }
    }

    Ok(())
}
