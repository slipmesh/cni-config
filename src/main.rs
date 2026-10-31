//! Watches this node's own `Node` object and keeps `/etc/cni/net.d/10-bridge.conflist` in sync
//! with `spec.podCIDRs` - see `conflist.rs`'s module doc comment for the rendering itself. A plain
//! Kubernetes DaemonSet rather than something that runs before Kubernetes: `podCIDR` only exists
//! once a node has joined a running cluster (kube-controller-manager's `--allocate-node-cidrs`),
//! so a live API dependency is the natural boundary, not a compromise.
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
const API_TIMEOUTS: ApiTimeouts = ApiTimeouts {
    watch_secs: 60,
    read: Duration::from_secs(65),
};

#[global_allocator]
static ALLOCATOR: memstats::CountingAllocator = memstats::CountingAllocator;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .without_time()
        .with_ansi(false)
        .init();
    tracing::info!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));

    let node_name =
        std::env::var("NODE_NAME").context("NODE_NAME env var must be set (downward API)")?;
    tracing::info!(node_name, "starting cni-config");
    tokio::spawn(memstats::report_every(MEMORY_REPORT_PERIOD));

    let config = kube::Config::infer()
        .await
        .context("failed to load the in-cluster Kubernetes config")?;
    let nodes: Api<Node> = Api::all(API_TIMEOUTS.client(config)?);

    tokio::select! {
        result = keep_conflist_current(nodes, &node_name, &API_TIMEOUTS, Path::new(CONFLIST_PATH)) => result,
        result = terminated() => {
            tracing::info!("SIGTERM received, exiting");
            result
        }
    }
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

/// Resolves when the pod is asked to stop. As PID 1 in its container the process has no default
/// handler for SIGTERM, so without this it would only go when the grace period runs out and the
/// kubelet sends SIGKILL.
async fn terminated() -> Result<()> {
    #[cfg(unix)]
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to install the SIGTERM handler")?
        .recv()
        .await;
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl-C")?;
    Ok(())
}

/// Watches this node's own `Node` object and rewrites the conflist whenever it differs from what
/// `spec.podCIDRs` renders to. A conflist that cannot be rendered or written ends the process
/// rather than waiting for the next event, which a quiet Node may not send for minutes: the
/// kubelet's restart lists the Node again and retries.
async fn keep_conflist_current(
    nodes: Api<Node>,
    node_name: &str,
    timeouts: &ApiTimeouts,
    path: &Path,
) -> Result<()> {
    let config = watcher::Config::default()
        .fields(&format!("metadata.name={node_name}"))
        .timeout(timeouts.watch_secs);
    let stream = watcher(nodes, config).default_backoff();
    let mut stream = std::pin::pin!(stream);

    while let Some(event) = stream.next().await {
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
        sync_conflist(path, &pod_cidrs)?;
    }

    Ok(())
}

/// Writes the conflist `pod_cidrs` render to unless the file already holds exactly that, so a
/// conflist removed or replaced since is restored on the next event.
fn sync_conflist(path: &Path, pod_cidrs: &[String]) -> Result<()> {
    let rendered = conflist::render_conflist(pod_cidrs)
        .with_context(|| format!("failed to render the CNI conflist for {pod_cidrs:?}"))?;
    if std::fs::read_to_string(path).is_ok_and(|current| current == rendered) {
        return Ok(());
    }
    write_conflist(path, &rendered)
        .with_context(|| format!("failed to write {}", path.display()))?;
    tracing::info!(?pod_cidrs, path = %path.display(), "CNI conflist written");
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
    const POD_CIDR: &str = "10.244.1.0/24";
    const TEST_TIMEOUTS: ApiTimeouts = ApiTimeouts {
        watch_secs: 1,
        read: Duration::from_secs(2),
    };

    /// A Kubernetes API stand-in that answers the watcher's list with this node and holds every
    /// watch open without events. The first `unanswered` connections are accepted and read but
    /// never answered, the way an API server going down mid-request leaves them.
    async fn serve_api(listener: TcpListener, mut unanswered: usize) {
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
                held.push(conn);
                continue;
            }
            let body = serde_json::json!({
                "apiVersion": "v1",
                "kind": "NodeList",
                "metadata": {"resourceVersion": "1"},
                "items": [{
                    "apiVersion": "v1",
                    "kind": "Node",
                    "metadata": {"name": NODE_NAME, "resourceVersion": "1"},
                    "spec": {"podCIDRs": [POD_CIDR]},
                }],
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

    /// Runs `keep_conflist_current` against `serve_api` and returns the conflist once written,
    /// or `None` if it isn't within `deadline`.
    async fn conflist_written_within(unanswered: usize, deadline: Duration) -> Option<String> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve_api(listener, unanswered));

        let config = kube::Config::new(url.parse().unwrap());
        let nodes: Api<Node> = Api::all(TEST_TIMEOUTS.client(config).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("10-bridge.conflist");
        let watch_path = path.clone();
        let watch = tokio::spawn(async move {
            keep_conflist_current(nodes, NODE_NAME, &TEST_TIMEOUTS, &watch_path).await
        });

        let written = tokio::time::timeout(deadline, async {
            loop {
                if let Ok(conflist) = std::fs::read_to_string(&path) {
                    return conflist;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .ok();
        watch.abort();
        written
    }

    #[test]
    fn restores_a_conflist_removed_since_it_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("10-bridge.conflist");
        sync_conflist(&path, &[POD_CIDR.to_string()]).unwrap();
        std::fs::remove_file(&path).unwrap();
        sync_conflist(&path, &[POD_CIDR.to_string()]).unwrap();
        assert!(path.exists(), "conflist not restored");
    }

    #[test]
    fn a_failed_write_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("10-bridge.conflist");
        assert!(sync_conflist(&path, &[POD_CIDR.to_string()]).is_err());
    }

    #[tokio::test]
    async fn writes_the_conflist_from_the_initial_list() {
        let conflist = conflist_written_within(0, Duration::from_secs(5))
            .await
            .expect("conflist not written from the node in the initial list");
        assert!(conflist.contains(POD_CIDR));
    }

    #[tokio::test]
    async fn recovers_from_a_request_the_api_never_answers() {
        let conflist = conflist_written_within(1, Duration::from_secs(10))
            .await
            .expect("conflist not written once the API answers");
        assert!(conflist.contains(POD_CIDR));
    }
}
