//! Renders the bridge/host-local/loopback CNI conflist for this node's own pod-bridge network,
//! from any set of CIDRs `Node.spec.podCIDRs` can hold: one on an IPv4-only cluster, one per
//! family on a dual-stack one.
//!
//! `bridge`/`host-local`/`loopback` already ship in `/opt/cni/bin` on every Talos node, so the
//! conflist is the only piece this binary produces.
//! `ipMasq: false` because inter-pod/inter-node traffic is routed - the `router` extension
//! announces this node's podCIDR over the mesh by matching the kernel routing table against a
//! configured prefix - and never NAT'd:
//! masquerading is nftables' job, only for external traffic. `mtu` is the mesh interfaces' MTU when
//! one is given, since cross-node pod traffic transits them; without one the bridge plugin leaves
//! the kernel's default in place. No explicit `ipam.routes` entry for `0.0.0.0/0`:
//! `isDefaultGateway: true` already makes `bridge` install that route, and adding it again via
//! `ipam.routes` fails every pod sandbox creation with `EEXIST`.

use anyhow::{Context, Result};
use sliplib_net::IpNet;

pub const BRIDGE_IFACE: &str = "cni0";

/// `ranges`: one range-set per address family present in `pod_cidrs`, IPv4 first - the shape
/// `host-local`'s own IPAM config expects (an array of pools, each itself an array of CIDRs
/// belonging to that pool).
pub fn render_conflist(pod_cidrs: &[String], mtu: Option<u32>) -> Result<String> {
    anyhow::ensure!(!pod_cidrs.is_empty(), "pod_cidrs must not be empty");

    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for cidr in pod_cidrs {
        let net =
            sliplib_net::parse(cidr).with_context(|| format!("{cidr:?} is not a valid CIDR"))?;
        let range = serde_json::json!({ "subnet": cidr });
        match net {
            IpNet::V4(_) => v4.push(range),
            IpNet::V6(_) => v6.push(range),
        }
    }
    let mut ranges = Vec::new();
    if !v4.is_empty() {
        ranges.push(v4);
    }
    if !v6.is_empty() {
        ranges.push(v6);
    }

    let mut bridge = serde_json::json!({
        "type": "bridge",
        "bridge": BRIDGE_IFACE,
        "isGateway": true,
        "isDefaultGateway": true,
        "ipMasq": false,
        "hairpinMode": true,
        "ipam": {
            "type": "host-local",
            "ranges": ranges,
        },
    });
    if let Some(mtu) = mtu {
        bridge["mtu"] = mtu.into();
    }
    let conflist = serde_json::json!({
        "cniVersion": "1.0.0",
        "name": "slipmesh-pod-network",
        "plugins": [bridge, { "type": "loopback" }],
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
        assert!(render_conflist(&[], None).is_err());
    }

    #[test]
    fn rejects_an_invalid_cidr() {
        assert!(render_conflist(&["not-a-cidr".to_string()], None).is_err());
    }

    #[test]
    fn rejects_a_cidr_without_a_valid_prefix() {
        for cidr in [
            "198.18.5.0",
            "198.18.5.0/33",
            "198.18.5.0/x",
            "2001:db8:61::/129",
        ] {
            assert!(
                render_conflist(&[cidr.to_string()], None).is_err(),
                "{cidr} accepted"
            );
        }
    }

    #[test]
    fn single_ipv4_cidr_has_one_range_set() {
        let rendered = render_conflist(&["198.18.5.0/24".to_string()], None).unwrap();
        let parsed = parse(&rendered);
        assert_eq!(parsed["plugins"][0]["type"], "bridge");
        assert_eq!(parsed["plugins"][0]["bridge"], BRIDGE_IFACE);
        assert_eq!(parsed["plugins"][0]["ipMasq"], false);
        assert_eq!(parsed["plugins"][0]["isDefaultGateway"], true);
        assert_eq!(parsed["plugins"][1]["type"], "loopback");
        let ranges = parsed["plugins"][0]["ipam"]["ranges"].as_array().unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0][0]["subnet"], "198.18.5.0/24");
    }

    #[test]
    fn dual_stack_cidrs_produce_two_range_sets_ipv4_first() {
        let rendered = render_conflist(
            &["2001:db8:61::/64".to_string(), "198.18.5.0/24".to_string()],
            None,
        )
        .unwrap();
        let parsed = parse(&rendered);
        let ranges = parsed["plugins"][0]["ipam"]["ranges"].as_array().unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0][0]["subnet"], "198.18.5.0/24");
        assert_eq!(ranges[1][0]["subnet"], "2001:db8:61::/64");
    }

    #[test]
    fn sets_the_given_mtu_on_the_bridge() {
        let rendered = render_conflist(&["198.18.1.0/24".to_string()], Some(1432)).unwrap();
        assert_eq!(parse(&rendered)["plugins"][0]["mtu"], 1432);
    }

    #[test]
    fn leaves_the_mtu_to_the_bridge_plugin_when_none_is_given() {
        let rendered = render_conflist(&["198.18.1.0/24".to_string()], None).unwrap();
        let bridge = &parse(&rendered)["plugins"][0];
        assert!(bridge.get("mtu").is_none(), "{bridge}");
    }
}
