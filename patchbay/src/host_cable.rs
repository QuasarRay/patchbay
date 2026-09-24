//! Explicit host-namespace cables for externally owned (for example Incus) veths.
//!
//! The caller must serialize all network mutations and retain authority over the
//! external endpoint lifecycle. This API never enters, kills or deletes a guest
//! namespace. It does not call `init_userns`. No interface is adopted by name
//! alone: ifindices and bridge aliases are checked before every mutation.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, process::Command};

/// A borrowed host veth identity, captured after the guest has started.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Endpoint {
    /// Host-side veth name (not the guest interface name).
    pub name: String,
    /// Observed kernel interface identity; changes invalidate this attachment.
    pub ifindex: u64,
}

/// An explicitly owned Ethernet cable. Call `remove` before stopping guests.
/// There is deliberately no destructive Drop implementation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostCable {
    /// Newly created bridge name.
    pub bridge: String,
    /// Bridge identity recorded at creation.
    pub ifindex: u64,
    /// Unique run ownership marker stored as the bridge's ifalias.
    pub owner: String,
    /// Exactly two borrowed endpoints.
    pub ends: [Endpoint; 2],
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.len() <= 15
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn ip(args: &[&str]) -> Result<Value> {
    let out = Command::new("timeout")
        .args(["--kill-after=2", "10", "ip"])
        .args(args)
        .output()?;
    ensure!(
        out.status.success(),
        "ip failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    if out.stdout.is_empty() {
        Ok(Value::Null)
    } else {
        Ok(serde_json::from_slice(&out.stdout)?)
    }
}
fn inspect(name: &str) -> Result<Value> {
    ensure!(valid_name(name), "invalid kernel interface name");
    let v = ip(&["-j", "-d", "link", "show", "dev", name])?;
    let rows = v.as_array().context("expected link array")?;
    ensure!(rows.len() == 1, "ambiguous link identity");
    Ok(rows[0].clone())
}
impl Endpoint {
    /// Prepare a freshly created external veth for an unnumbered cable.
    /// The caller MUST own its lifecycle and hold the host mutation lock.
    /// Automatic IPv6 link-local addresses may exist after Incus starts a guest;
    /// only those may be removed. Any IPv4/global address or existing master is
    /// refused. Capture and recheck ifindex around the scoped sysctl mutation.
    pub fn prepare_owned(name: &str) -> Result<Self> {
        let v = inspect(name)?;
        ensure!(
            v["linkinfo"]["info_kind"] == "veth" && v.get("master").is_none(),
            "owned endpoint must be an unattached veth"
        );
        let endpoint = Self {
            name: name.into(),
            ifindex: v["ifindex"].as_u64().context("missing ifindex")?,
        };
        let addresses = ip(&["-j", "address", "show", "dev", name])?;
        let addresses = addresses[0]["addr_info"]
            .as_array()
            .context("address array")?;
        ensure!(
            addresses
                .iter()
                .all(|a| a["family"] == "inet6" && a["scope"] == "link"),
            "refusing a numbered host endpoint"
        );
        endpoint.verify()?;
        let setting = format!("/proc/sys/net/ipv6/conf/{name}/disable_ipv6");
        match std::fs::write(setting, "1\n") {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && addresses.is_empty() => (),
            Err(e) => return Err(e.into()),
        }
        endpoint.verify()?;
        let checked = Self::observe(name)?;
        ensure!(
            checked.ifindex == endpoint.ifindex,
            "endpoint changed during preparation"
        );
        Ok(endpoint)
    }
    /// Observe an unnumbered, unattached veth. An existing master is refused.
    pub fn observe(name: &str) -> Result<Self> {
        let v = inspect(name)?;
        ensure!(
            v["linkinfo"]["info_kind"] == "veth",
            "endpoint must be a veth"
        );
        ensure!(
            v.get("master").is_none(),
            "endpoint already belongs to a bridge"
        );
        let addresses = ip(&["-j", "address", "show", "dev", name])?;
        ensure!(
            addresses[0]["addr_info"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "host endpoint has IP addresses"
        );
        Ok(Self {
            name: name.into(),
            ifindex: v["ifindex"].as_u64().context("missing ifindex")?,
        })
    }
    fn verify(&self) -> Result<Value> {
        let v = inspect(&self.name)?;
        ensure!(
            v["ifindex"].as_u64() == Some(self.ifindex),
            "endpoint generation changed"
        );
        ensure!(
            v["linkinfo"]["info_kind"] == "veth",
            "endpoint kind changed"
        );
        Ok(v)
    }
}
impl HostCable {
    /// Join two borrowed veths with one private, unnumbered Linux bridge.
    /// L2 control protocols and ASIC/RDMA timing are outside this API's fidelity.
    pub fn connect(bridge: &str, owner: &str, ends: [Endpoint; 2]) -> Result<Self> {
        ensure!(
            valid_name(bridge) && bridge.starts_with("pc"),
            "bridge needs a pc prefix and <=15 bytes"
        );
        ensure!(
            owner.len() >= 16
                && owner.len() <= 80
                && owner
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid owner marker"
        );
        ensure!(
            ends[0].name != ends[1].name && ends[0].ifindex != ends[1].ifindex,
            "endpoints must differ"
        );
        for end in &ends {
            ensure!(
                end.verify()?.get("master").is_none(),
                "endpoint was adopted by another bridge"
            );
        }
        // Exclusive creation: never replace an existing device.
        ip(&[
            "link",
            "add",
            "name",
            bridge,
            "type",
            "bridge",
            "stp_state",
            "0",
            "mcast_snooping",
            "0",
        ])?;
        let setup: Result<Self> = (|| {
            ip(&["link", "set", "dev", bridge, "alias", owner])?;
            let cable = Self {
                bridge: bridge.into(),
                ifindex: inspect(bridge)?["ifindex"]
                    .as_u64()
                    .context("missing bridge ifindex")?,
                owner: owner.into(),
                ends,
            };
            // Do not allow automatic IPv6 addresses or a host route on a cable.
            std::fs::write(
                format!("/proc/sys/net/ipv6/conf/{bridge}/disable_ipv6"),
                "1\n",
            )?;
            for end in &cable.ends {
                end.verify()?;
                ip(&["link", "set", "dev", &end.name, "master", bridge])?;
                ip(&["link", "set", "dev", &end.name, "up"])?;
            }
            ip(&["link", "set", "dev", bridge, "up"])?;
            cable.verify()?;
            Ok(cable)
        })();
        match setup {
            Ok(cable) => Ok(cable),
            Err(error) => {
                // The caller owns the mutation lock. Deleting this newly created
                // bridge detaches borrowed veths without deleting them.
                let cleanup = ip(&["link", "delete", "dev", bridge]);
                bail!("cable setup failed: {error:#}; rollback: {cleanup:?}")
            }
        }
    }
    /// Revalidate owner, bridge generation and exact endpoint membership.
    pub fn verify(&self) -> Result<()> {
        let bridge = inspect(&self.bridge)?;
        ensure!(
            bridge["ifindex"].as_u64() == Some(self.ifindex)
                && bridge["ifalias"] == self.owner
                && bridge["linkinfo"]["info_kind"] == "bridge",
            "bridge ownership/generation mismatch"
        );
        let members = ip(&["-j", "link", "show", "master", &self.bridge])?;
        let actual: BTreeSet<_> = members
            .as_array()
            .context("members array")?
            .iter()
            .filter_map(|v| v["ifindex"].as_u64())
            .collect();
        let expected: BTreeSet<_> = self.ends.iter().map(|e| e.ifindex).collect();
        ensure!(actual == expected, "foreign or missing bridge member");
        for end in &self.ends {
            end.verify()?;
        }
        Ok(())
    }
    /// Impair A→B exactly once, on B's host-side egress (and vice versa).
    /// Delay is nanoseconds; the kernel may quantize it. This is not ASIC PFC.
    pub fn shape_towards(
        &self,
        destination: usize,
        delay_ns: u64,
        bandwidth_bps: u64,
        queue_packets: u32,
    ) -> Result<()> {
        ensure!(
            destination < 2 && delay_ns <= 60_000_000_000 && bandwidth_bps > 0 && queue_packets > 0,
            "invalid impairment"
        );
        self.verify()?;
        let end = &self.ends[destination];
        let out = Command::new("timeout")
            .args([
                "--kill-after=2",
                "10",
                "tc",
                "qdisc",
                "replace",
                "dev",
                &end.name,
                "root",
                "netem",
                "delay",
                &format!("{:.3}us", delay_ns as f64 / 1000.0),
                "rate",
                &format!("{bandwidth_bps}bit"),
                "limit",
                &queue_packets.to_string(),
            ])
            .output()?;
        ensure!(
            out.status.success(),
            "tc failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }
    /// Cut or restore both directions by changing the owned bridge only.
    pub fn set_up(&self, up: bool) -> Result<()> {
        self.verify()?;
        ip(&[
            "link",
            "set",
            "dev",
            &self.bridge,
            if up { "up" } else { "down" },
        ])?;
        Ok(())
    }
    /// Detach borrowed endpoints and remove only this owned bridge.
    /// Refuse cleanup if another actor has changed membership or ownership.
    pub fn remove(&self) -> Result<()> {
        self.verify()?;
        ip(&["link", "delete", "dev", &self.bridge])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_names_that_could_escape_scope() {
        for name in [
            "",
            "-all",
            "a/b",
            "x y",
            "abcdefghijklmnop",
            "$(id)",
            "eth0:1",
        ] {
            assert!(!valid_name(name));
        }
        assert!(valid_name("pc0123456789abc"));
    }
}
