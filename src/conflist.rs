//! Renders the bridge/host-local/loopback CNI conflist for this node's own pod-bridge network.
//! Ported from `operators/router/src/bird.rs::render_cni_conflist` (github.com/slipmesh/operators),
//! generalized from one IPv4 CIDR to an arbitrary set - exactly what `Node.spec.podCIDRs` can
//! actually contain (one entry on today's IPv4-only cluster, two once/if dual-stack `podSubnets`
//! is ever turned on) - so this binary doesn't need rewriting when that happens.
//!
//! `bridge`/`host-local`/`loopback` are already present in `/opt/cni/bin` on every Talos node
//! (per Talos's own build Dockerfile), so the conflist is the only piece this binary produces.
//! `ipMasq: false` because inter-pod/inter-node traffic is routed - the `router` extension
//! announces this node's podCIDR over the mesh by matching the kernel routing table against a
//! configured prefix - and never NAT'd:
//! masquerading is nftables' job, only for external traffic. `mtu: 1420` matches the AmneziaWG mesh
//! interfaces' MTU, since cross-node pod traffic transits them; a default 1500 would silently
//! blackhole on PMTU. No explicit `ipam.routes` entry for `0.0.0.0/0`: `isDefaultGateway: true`
//! already makes `bridge` install that route, and adding it again via `ipam.routes` fails every pod
//! sandbox creation with `EEXIST`.

use anyhow::{Context, Result};
use std::net::IpAddr;

pub const BRIDGE_IFACE: &str = "cni0";

fn family_of(cidr: &str) -> Result<bool> {
    let addr = cidr
        .split('/')
        .next()
        .with_context(|| format!("{cidr:?} is not a CIDR"))?;
    let addr: IpAddr = addr
        .parse()
        .with_context(|| format!("{cidr:?} is not a valid CIDR"))?;
    Ok(addr.is_ipv6())
}

/// `ranges`: one range-set per address family present in `pod_cidrs`, IPv4 first - the shape
/// `host-local`'s own IPAM config expects (an array of pools, each itself an array of CIDRs
/// belonging to that pool).
pub fn render_conflist(pod_cidrs: &[String]) -> Result<String> {
    anyhow::ensure!(!pod_cidrs.is_empty(), "pod_cidrs must not be empty");

    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for cidr in pod_cidrs {
        if family_of(cidr)? {
            v6.push(serde_json::json!({ "subnet": cidr }));
        } else {
            v4.push(serde_json::json!({ "subnet": cidr }));
        }
    }
    let mut ranges = Vec::new();
    if !v4.is_empty() {
        ranges.push(v4);
    }
    if !v6.is_empty() {
        ranges.push(v6);
    }

    let conflist = serde_json::json!({
        "cniVersion": "1.0.0",
        "name": "slipmesh-pod-network",
        "plugins": [
            {
                "type": "bridge",
                "bridge": BRIDGE_IFACE,
                "isGateway": true,
                "isDefaultGateway": true,
                "ipMasq": false,
                "hairpinMode": true,
                "mtu": 1420,
                "ipam": {
                    "type": "host-local",
                    "ranges": ranges,
                },
            },
            { "type": "loopback" },
        ],
    });
    Ok(serde_json::to_string_pretty(&conflist)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("conflist must be valid JSON")
    }

    #[test]
    fn rejects_an_empty_list() {
        assert!(render_conflist(&[]).is_err());
    }

    #[test]
    fn rejects_an_invalid_cidr() {
        assert!(render_conflist(&["not-a-cidr".to_string()]).is_err());
    }

    #[test]
    fn single_ipv4_cidr_has_one_range_set() {
        let rendered = render_conflist(&["10.61.5.0/24".to_string()]).unwrap();
        let parsed = parse(&rendered);
        assert_eq!(parsed["plugins"][0]["type"], "bridge");
        assert_eq!(parsed["plugins"][0]["bridge"], BRIDGE_IFACE);
        assert_eq!(parsed["plugins"][0]["mtu"], 1420);
        assert_eq!(parsed["plugins"][0]["ipMasq"], false);
        assert_eq!(parsed["plugins"][0]["isDefaultGateway"], true);
        assert_eq!(parsed["plugins"][1]["type"], "loopback");
        let ranges = parsed["plugins"][0]["ipam"]["ranges"].as_array().unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0][0]["subnet"], "10.61.5.0/24");
    }

    #[test]
    fn dual_stack_cidrs_produce_two_range_sets_ipv4_first() {
        let rendered =
            render_conflist(&["fd00:61::/64".to_string(), "10.61.5.0/24".to_string()]).unwrap();
        let parsed = parse(&rendered);
        let ranges = parsed["plugins"][0]["ipam"]["ranges"].as_array().unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0][0]["subnet"], "10.61.5.0/24");
        assert_eq!(ranges[1][0]["subnet"], "fd00:61::/64");
    }
}
