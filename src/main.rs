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
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client, ResourceExt};

const CONFLIST_PATH: &str = "/etc/cni/net.d/10-bridge.conflist";
const MEMORY_REPORT_PERIOD: Duration = Duration::from_secs(600);
/// Well inside the kubelet's default grace period of 30 s.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// The one MTU of every mesh interface on this node, which cross-node pod traffic transits.
const MESH_MTU_ANNOTATION: &str = "slipmesh.net/mesh_mtu";
/// How long after start a Node without the annotation is waited on before the conflist goes out
/// without an MTU: Talos sets annotations a moment after the Node registers, and a pod created in
/// between would keep the default MTU for its whole life.
const MESH_MTU_GRACE: Duration = Duration::from_secs(10);
/// IPv6 needs at least 1280 on every link; a veth takes at most 65535 (`ETH_MAX_MTU`), and the
/// bridge plugin fails every pod sandbox on anything above.
const MESH_MTU_RANGE: std::ops::RangeInclusive<u32> = 1280..=65535;
const API_TIMEOUTS: ApiTimeouts = ApiTimeouts {
    watch_secs: 60,
    read: Duration::from_secs(65),
};

#[global_allocator]
static ALLOCATOR: memstats::CountingAllocator = memstats::CountingAllocator;

#[tokio::main]
async fn main() -> Result<()> {
    sliplib_runtime::init_tracing();
    // As PID 1 in its container the process has no default handler for SIGTERM: one that arrived
    // before `serve` installs the handler would be dropped, and the pod would go only when the
    // grace period runs out and the kubelet sends SIGKILL.
    sliplib_runtime::serve("cni-config", SHUTDOWN_TIMEOUT, async |shutdown| {
        tokio::select! {
            result = watch() => result,
            () = shutdown.requested() => {
                tracing::info!("SIGTERM received, exiting");
                Ok(())
            }
        }
    })
    .await
}

async fn watch() -> Result<()> {
    tracing::info!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
    let node_name =
        std::env::var("NODE_NAME").context("NODE_NAME env var must be set (downward API)")?;
    tracing::info!(node_name, "starting cni-config");
    tokio::spawn(memstats::report_every(MEMORY_REPORT_PERIOD));

    let config = kube::Config::infer()
        .await
        .context("failed to load the in-cluster Kubernetes config")?;
    let nodes: Api<Node> = Api::all(API_TIMEOUTS.client(config)?);
    keep_conflist_current(
        nodes,
        &node_name,
        &API_TIMEOUTS,
        MESH_MTU_GRACE,
        Path::new(CONFLIST_PATH),
    )
    .await
}

/// kube sets no response timeout and bounds only an open watch stream, so a list or watch request
/// the API server accepted but never answers would hold the watcher forever. `read` has to outlast
/// `watch_secs`, the server-side lifetime of a watch, or every quiet watch would be cut as dead.
struct ApiTimeouts {
    watch_secs: u32,
    read: Duration,
}

impl ApiTimeouts {
    fn client(&self, mut config: kube::Config) -> Result<Client> {
        config.read_timeout = Some(self.read);
        Client::try_from(config).context("failed to build the Kubernetes client")
    }
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
    let config = watcher::Config::default()
        .fields(&format!("metadata.name={node_name}"))
        .timeout(timeouts.watch_secs);
    let stream = watcher(nodes, config).default_backoff();
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
            "annotation is not an MTU from 1280 to 65535, ignored"
        );
    }
    mtu
}

/// Writes the conflist `pod_cidrs` and `mtu` render to unless the file already holds exactly that,
/// so a conflist removed or replaced since is restored on the next event.
fn sync_conflist(path: &Path, pod_cidrs: &[String], mtu: Option<u32>) -> Result<()> {
    let rendered = conflist::render_conflist(pod_cidrs, mtu)
        .with_context(|| format!("failed to render the CNI conflist for {pod_cidrs:?}"))?;
    if std::fs::read_to_string(path).is_ok_and(|current| current == rendered) {
        return Ok(());
    }
    write_conflist(path, &rendered)
        .with_context(|| format!("failed to write {}", path.display()))?;
    tracing::info!(?pod_cidrs, ?mtu, path = %path.display(), "CNI conflist written");
    Ok(())
}

/// Replaces the conflist through a temporary file in the same directory, so a kill mid-write
/// leaves the previous file whole rather than a truncated one the CRI would fail to parse. The
/// temporary file is named `.10-bridge.conflist.<random>`, which the CRI's loader skips.
fn write_conflist(path: &Path, rendered: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(rendered.as_bytes())?;
    file.commit()
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const NODE_NAME: &str = "node-a";
    const POD_CIDR: &str = "198.18.1.0/24";
    const TEST_TIMEOUTS: ApiTimeouts = ApiTimeouts {
        watch_secs: 1,
        read: Duration::from_secs(2),
    };
    const TEST_MTU_GRACE: Duration = Duration::from_secs(2);

    /// This node at `resource_version`, carrying `mesh_mtu` as its annotation if given.
    fn node(resource_version: u32, mesh_mtu: Option<&str>) -> serde_json::Value {
        let mut metadata = serde_json::json!({
            "name": NODE_NAME,
            "resourceVersion": resource_version.to_string(),
        });
        if let Some(mtu) = mesh_mtu {
            metadata["annotations"] = serde_json::json!({ (MESH_MTU_ANNOTATION): mtu });
        }
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": metadata,
            "spec": {"podCIDRs": [POD_CIDR]},
        })
    }

    /// A Kubernetes API stand-in. It answers the watcher's list with `listed`, sends `changes` as
    /// `MODIFIED` events on the first watch, each after its delay, and holds every watch open after
    /// that. The first `unanswered` connections are accepted and read but never answered, the way
    /// an API server going down mid-request leaves them.
    async fn serve_api(
        listener: TcpListener,
        mut unanswered: usize,
        listed: serde_json::Value,
        mut changes: Vec<(Duration, serde_json::Value)>,
    ) {
        let mut held = Vec::new();
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let Some(request) = read_request(&mut conn).await else {
                continue;
            };
            if unanswered > 0 {
                unanswered -= 1;
                held.push(conn);
                continue;
            }
            if request.contains("watch=true") {
                conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n")
                    .await
                    .unwrap();
                for (delay, change) in changes.drain(..) {
                    tokio::time::sleep(delay).await;
                    let event = serde_json::json!({"type": "MODIFIED", "object": change})
                        .to_string()
                        + "\n";
                    let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                    conn.write_all(chunk.as_bytes()).await.unwrap();
                }
                held.push(conn);
                continue;
            }
            let body = serde_json::json!({
                "apiVersion": "v1",
                "kind": "NodeList",
                "metadata": {"resourceVersion": listed["metadata"]["resourceVersion"]},
                "items": [listed],
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(response.as_bytes()).await.unwrap();
        }
    }

    /// `None` if the client closes the connection before a whole request arrives.
    async fn read_request(conn: &mut TcpStream) -> Option<String> {
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = conn.read(&mut buf).await.ok()?;
            if n == 0 {
                return None;
            }
            request.extend_from_slice(&buf[..n]);
        }
        Some(String::from_utf8(request).unwrap())
    }

    /// `keep_conflist_current` running against `serve_api`, and the conflist it keeps.
    struct Watch {
        task: tokio::task::JoinHandle<Result<()>>,
        path: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Watch {
        async fn start(
            unanswered: usize,
            listed: serde_json::Value,
            changes: Vec<(Duration, serde_json::Value)>,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(serve_api(listener, unanswered, listed, changes));

            let config = kube::Config::new(url.parse().unwrap());
            let nodes: Api<Node> = Api::all(TEST_TIMEOUTS.client(config).unwrap());
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
    fn reads_the_mesh_mtu_annotation() {
        assert_eq!(mesh_mtu(&annotated(Some("1432"))), Some(1432));
    }

    #[test]
    fn a_node_without_the_annotation_has_no_mesh_mtu() {
        assert_eq!(mesh_mtu(&annotated(None)), None);
    }

    #[test]
    fn accepts_the_mtu_range_a_veth_takes_from_ipv6_minimum_up() {
        assert_eq!(mesh_mtu(&annotated(Some("1280"))), Some(1280));
        assert_eq!(mesh_mtu(&annotated(Some("65535"))), Some(65535));
    }

    #[test]
    fn ignores_a_mesh_mtu_that_is_not_one() {
        for value in ["", "mtu", "1432.0", "-1432", "1279", "65536"] {
            assert_eq!(
                mesh_mtu(&annotated(Some(value))),
                None,
                "{value:?} accepted"
            );
        }
    }

    #[test]
    fn restores_a_conflist_removed_since_it_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("10-bridge.conflist");
        sync_conflist(&path, &[POD_CIDR.to_string()], None).unwrap();
        std::fs::remove_file(&path).unwrap();
        sync_conflist(&path, &[POD_CIDR.to_string()], None).unwrap();
        assert!(path.exists(), "conflist not restored");
    }

    #[test]
    fn a_failed_write_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("10-bridge.conflist");
        assert!(sync_conflist(&path, &[POD_CIDR.to_string()], None).is_err());
    }

    #[tokio::test]
    async fn writes_the_conflist_with_the_mesh_mtu_from_the_initial_list() {
        let watch = Watch::start(0, node(1, Some("1432")), vec![]).await;
        let bridge = watch
            .bridge_within(TEST_MTU_GRACE / 2, any_conflist)
            .await
            .expect("conflist not written from the node in the initial list");
        assert_eq!(bridge["mtu"], 1432);
        assert_eq!(bridge["ipam"]["ranges"][0][0]["subnet"], POD_CIDR);
    }

    #[tokio::test]
    async fn recovers_from_a_request_the_api_never_answers() {
        let watch = Watch::start(1, node(1, Some("1432")), vec![]).await;
        assert!(
            watch
                .bridge_within(Duration::from_secs(10), with_mtu(1432))
                .await
                .is_some(),
            "conflist not written once the API answers"
        );
    }

    #[tokio::test]
    async fn waits_out_the_grace_period_for_the_annotation_then_writes_without_an_mtu() {
        let watch = Watch::start(0, node(1, None), vec![]).await;
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
        let watch = Watch::start(0, node(1, None), vec![(soon, node(2, Some("1432")))]).await;
        let bridge = watch
            .bridge_within(TEST_MTU_GRACE, any_conflist)
            .await
            .expect("conflist not written when the annotation arrived");
        assert_eq!(bridge["mtu"], 1432);
    }

    #[tokio::test]
    async fn a_malformed_annotation_counts_as_none() {
        let watch = Watch::start(0, node(1, Some("1279")), vec![]).await;
        assert!(
            watch
                .bridge_within(TEST_MTU_GRACE * 3, without_mtu)
                .await
                .is_some(),
            "conflist not written without an mtu"
        );
    }

    #[tokio::test]
    async fn rewrites_the_conflist_when_the_mesh_mtu_changes() {
        let changed = (Duration::from_millis(300), node(2, Some("1420")));
        let watch = Watch::start(0, node(1, Some("1432")), vec![changed]).await;
        assert!(
            watch
                .bridge_within(Duration::from_secs(5), with_mtu(1420))
                .await
                .is_some(),
            "conflist not rewritten with the new mesh MTU"
        );
    }

    #[tokio::test]
    async fn drops_the_mtu_when_the_annotation_is_removed_after_the_grace_period() {
        // Two steps, each inside the client's read timeout, which would cut a quiet watch.
        let step = TEST_MTU_GRACE * 3 / 4;
        let changes = vec![(step, node(2, Some("1432"))), (step, node(3, None))];
        let watch = Watch::start(0, node(1, Some("1432")), changes).await;
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
