//! Watches this node's own `Node` object and keeps `/etc/cni/net.d/10-bridge.conflist` in sync
//! with `spec.podCIDRs` and the mesh MTU annotation - see `conflist.rs`'s module doc comment for
//! the rendering itself. A plain Kubernetes DaemonSet rather than something that runs before
//! Kubernetes: `podCIDR` only exists once a node has joined a running cluster
//! (kube-controller-manager's `--allocate-node-cidrs`), so a live API dependency is the natural
//! boundary, not a compromise.
//!
//! Watches rather than reads once: `Node.spec.podCIDRs` is documented as immutable once set, but
//! this is a long-lived pod, not a one-shot job, so watching costs nothing and doesn't depend on
//! that invariant holding forever (a manually edited/recreated Node could still change it).

mod conflist;
mod memstats;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::runtime::watcher;
use kube::{Api, ResourceExt};
use sliplib_kube::ApiTimeouts;
use sliplib_net::mtu::{MESH_MTU_ANNOTATION, MIN_IPV6_MTU};

const CONFLIST_PATH: &str = "/etc/cni/net.d/10-bridge.conflist";
const MEMORY_REPORT_PERIOD: Duration = Duration::from_secs(600);
/// Well inside the kubelet's default grace period of 30 s.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long after start a Node without the annotation is waited on before the conflist goes out
/// without an MTU: Talos sets annotations a moment after the Node registers, and a pod created in
/// between would keep the default MTU for its whole life.
const MESH_MTU_GRACE: Duration = Duration::from_secs(10);
/// A veth takes at most 65535 (`ETH_MAX_MTU`), and the bridge plugin fails every pod sandbox on
/// anything above.
const MESH_MTU_RANGE: std::ops::RangeInclusive<u32> = MIN_IPV6_MTU..=65535;
const API_TIMEOUTS: ApiTimeouts = ApiTimeouts {
    watch_secs: 60,
    read: Duration::from_secs(65),
};

#[global_allocator]
static ALLOCATOR: memstats::CountingAllocator = memstats::CountingAllocator;

#[tokio::main]
async fn main() -> Result<()> {
    sliplib_runtime::init_tracing();
    tracing::info!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
    // As PID 1 in its container the process has no default handler for SIGTERM: one that arrived
    // before `serve` installs the handler would be dropped, and the pod would go only when the
    // grace period runs out and the kubelet sends SIGKILL.
    sliplib_kube::run(
        "cni-config",
        SHUTDOWN_TIMEOUT,
        API_TIMEOUTS,
        |agent, shutdown| async move {
            tokio::spawn(memstats::report_every(MEMORY_REPORT_PERIOD));
            let nodes: Api<Node> = Api::all(agent.client);
            tokio::select! {
                result = keep_conflist_current(
                    nodes,
                    &agent.node,
                    &API_TIMEOUTS,
                    MESH_MTU_GRACE,
                    Path::new(CONFLIST_PATH),
                ) => result,
                () = shutdown.requested() => {
                    tracing::info!("SIGTERM received, exiting");
                    Ok(())
                }
            }
        },
    )
    .await
}

/// Watches this node's own `Node` object and rewrites the conflist whenever it differs from what
/// `spec.podCIDRs` and the mesh MTU annotation render to. Until `mtu_grace` has passed, a Node
/// without the annotation is waited on rather than written without an MTU. A conflist that cannot
/// be rendered or written ends the process rather than waiting for the next event, which a quiet
/// Node may not send for minutes: the kubelet's restart lists the Node again and retries.
async fn keep_conflist_current(
    nodes: Api<Node>,
    node_name: &str,
    timeouts: &ApiTimeouts,
    mtu_grace: Duration,
    path: &Path,
) -> Result<()> {
    let stream = sliplib_kube::watch_one(nodes, node_name, timeouts);
    let mut stream = std::pin::pin!(stream);
    let grace = tokio::time::sleep(mtu_grace);
    let mut grace = std::pin::pin!(grace);
    let mut grace_over = false;
    let mut waiting_for_mtu: Option<Vec<String>> = None;

    loop {
        let event = tokio::select! {
            event = stream.next() => event,
            () = &mut grace, if !grace_over => {
                grace_over = true;
                if let Some(pod_cidrs) = waiting_for_mtu.take() {
                    sync_conflist(path, &pod_cidrs, None)?;
                }
                continue;
            }
        };
        let Some(event) = event else {
            return Ok(());
        };
        memstats::WATCH_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let node = match event {
            Ok(watcher::Event::Apply(node) | watcher::Event::InitApply(node)) => node,
            Ok(_) => continue,
            Err(e) => {
                memstats::WATCH_ERRORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(error = %e, "watch stream error, retrying");
                continue;
            }
        };

        let Some(pod_cidrs) = node.spec.as_ref().and_then(|s| s.pod_cidrs.clone()) else {
            tracing::debug!(node = node.name_any(), "no spec.podCIDRs yet, waiting");
            continue;
        };
        let mtu = mesh_mtu(&node);
        if mtu.is_none() && !grace_over {
            tracing::debug!(node = node.name_any(), "no mesh MTU yet, waiting");
            waiting_for_mtu = Some(pod_cidrs);
            continue;
        }
        waiting_for_mtu = None;
        sync_conflist(path, &pod_cidrs, mtu)?;
    }
}

/// The MTU the node's mesh MTU annotation carries. A value that is not one is logged and read as
/// no annotation: refusing to write the conflist over it would stop every new pod on the node.
fn mesh_mtu(node: &Node) -> Option<u32> {
    let value = node.annotations().get(MESH_MTU_ANNOTATION)?;
    let mtu = value
        .parse()
        .ok()
        .filter(|mtu| MESH_MTU_RANGE.contains(mtu));
    if mtu.is_none() {
        tracing::error!(
            annotation = MESH_MTU_ANNOTATION,
            value,
            min = MESH_MTU_RANGE.start(),
            max = MESH_MTU_RANGE.end(),
            "annotation is not an MTU in range, ignored"
        );
    }
    mtu
}

/// Writes the conflist `pod_cidrs` and `mtu` render to unless the file already holds exactly that,
/// so a conflist removed or replaced since is restored on the next event.
fn sync_conflist(path: &Path, pod_cidrs: &[String], mtu: Option<u32>) -> Result<()> {
    let rendered = conflist::render_conflist(pod_cidrs, mtu)
        .with_context(|| format!("failed to render the CNI conflist for {pod_cidrs:?}"))?;
    // The CRI's loader skips the temporary file, so a kill mid-write leaves the previous file
    // whole.
    if sliplib_fs::write_if_changed(path, &rendered)
        .with_context(|| format!("failed to write the CNI conflist to {}", path.display()))?
    {
        tracing::info!(?pod_cidrs, ?mtu, path = %path.display(), "CNI conflist written");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sliplib_kube::testing::FakeApi;

    const NODE_NAME: &str = "node-a";
    const POD_CIDR: &str = "198.18.1.0/24";
    const TEST_TIMEOUTS: ApiTimeouts = ApiTimeouts {
        watch_secs: 1,
        read: Duration::from_secs(2),
    };
    const TEST_MTU_GRACE: Duration = Duration::from_secs(2);

    /// This node at `resource_version`, with its pod CIDR, carrying `mesh_mtu` as its annotation
    /// if given.
    fn node(resource_version: u32, mesh_mtu: Option<&str>) -> serde_json::Value {
        let annotations: Vec<_> = mesh_mtu
            .map(|mtu| (MESH_MTU_ANNOTATION, mtu))
            .into_iter()
            .collect();
        let mut node = sliplib_kube::testing::node(NODE_NAME, resource_version, &annotations);
        node["spec"] = serde_json::json!({"podCIDRs": [POD_CIDR]});
        node
    }

    /// `keep_conflist_current` running against a `FakeApi`, and the conflist it keeps.
    struct Watch {
        task: tokio::task::JoinHandle<Result<()>>,
        path: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Watch {
        async fn start(
            listed: serde_json::Value,
            changes: Vec<(Duration, serde_json::Value)>,
        ) -> Self {
            let api = FakeApi::start(0, listed, changes).await;
            let nodes: Api<Node> = Api::all(TEST_TIMEOUTS.client(api.config()).unwrap());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("10-bridge.conflist");
            let watch_path = path.clone();
            let task = tokio::spawn(async move {
                keep_conflist_current(
                    nodes,
                    NODE_NAME,
                    &TEST_TIMEOUTS,
                    TEST_MTU_GRACE,
                    &watch_path,
                )
                .await
            });
            Self {
                task,
                path,
                _dir: dir,
            }
        }

        /// The bridge plugin's entry of the first conflist written that satisfies `wanted`, or
        /// `None` if none is within `deadline`.
        async fn bridge_within(
            &self,
            deadline: Duration,
            wanted: impl Fn(&serde_json::Value) -> bool,
        ) -> Option<serde_json::Value> {
            tokio::time::timeout(deadline, async {
                loop {
                    if let Some(bridge) = self.bridge().filter(&wanted) {
                        return bridge;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .ok()
        }

        fn bridge(&self) -> Option<serde_json::Value> {
            let conflist = std::fs::read_to_string(&self.path).ok()?;
            let parsed: serde_json::Value = serde_json::from_str(&conflist).unwrap();
            Some(parsed["plugins"][0].clone())
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn any_conflist(_: &serde_json::Value) -> bool {
        true
    }

    fn with_mtu(mtu: u32) -> impl Fn(&serde_json::Value) -> bool {
        move |bridge| bridge["mtu"] == mtu
    }

    fn without_mtu(bridge: &serde_json::Value) -> bool {
        bridge.get("mtu").is_none()
    }

    fn annotated(mesh_mtu: Option<&str>) -> Node {
        serde_json::from_value(node(1, mesh_mtu)).unwrap()
    }

    #[test]
    fn accepts_the_mtu_range_a_veth_takes_from_ipv6_minimum_up() {
        assert_eq!(mesh_mtu(&annotated(Some("1280"))), Some(1280));
        assert_eq!(mesh_mtu(&annotated(Some("65535"))), Some(65535));
    }

    #[test]
    fn ignores_a_mesh_mtu_that_is_not_one() {
        for value in ["mtu", "1279", "65536"] {
            assert_eq!(
                mesh_mtu(&annotated(Some(value))),
                None,
                "{value:?} accepted"
            );
        }
    }

    #[test]
    fn a_conflist_whose_directory_is_missing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("10-bridge.conflist");
        assert!(sync_conflist(&path, &[POD_CIDR.to_string()], None).is_err());
    }

    #[tokio::test]
    async fn writes_the_conflist_with_the_mesh_mtu_from_the_initial_list() {
        let watch = Watch::start(node(1, Some("1432")), vec![]).await;
        let bridge = watch
            .bridge_within(TEST_MTU_GRACE / 2, any_conflist)
            .await
            .expect("conflist not written from the node in the initial list");
        assert_eq!(bridge["mtu"], 1432);
        assert_eq!(bridge["ipam"]["ranges"][0][0]["subnet"], POD_CIDR);
    }

    #[tokio::test]
    async fn waits_out_the_grace_period_for_the_annotation_then_writes_without_an_mtu() {
        let watch = Watch::start(node(1, None), vec![]).await;
        assert!(
            watch
                .bridge_within(TEST_MTU_GRACE / 2, any_conflist)
                .await
                .is_none(),
            "conflist written before the grace period ended"
        );
        assert!(
            watch
                .bridge_within(TEST_MTU_GRACE * 3, without_mtu)
                .await
                .is_some(),
            "conflist not written once the grace period ended"
        );
    }

    #[tokio::test]
    async fn an_annotation_arriving_within_the_grace_period_is_written_at_once() {
        let soon = TEST_MTU_GRACE / 4;
        let watch = Watch::start(node(1, None), vec![(soon, node(2, Some("1432")))]).await;
        let bridge = watch
            .bridge_within(TEST_MTU_GRACE, any_conflist)
            .await
            .expect("conflist not written when the annotation arrived");
        assert_eq!(bridge["mtu"], 1432);
    }

    #[tokio::test]
    async fn drops_the_mtu_when_the_annotation_is_removed_after_the_grace_period() {
        // Two steps, each inside the client's read timeout, which would cut a quiet watch.
        let step = TEST_MTU_GRACE * 3 / 4;
        let changes = vec![(step, node(2, Some("1432"))), (step, node(3, None))];
        let watch = Watch::start(node(1, Some("1432")), changes).await;
        assert!(
            watch
                .bridge_within(TEST_MTU_GRACE, with_mtu(1432))
                .await
                .is_some(),
            "conflist not written with the mesh MTU"
        );
        assert!(
            watch
                .bridge_within(Duration::from_secs(5), without_mtu)
                .await
                .is_some(),
            "mtu kept after the annotation was removed"
        );
    }
}
