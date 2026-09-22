use std::{collections::BTreeMap, fmt::Write};

use anyhow::{bail, Context, Result};

/// The two native IB node types supported by this adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IbNodeKind {
    /// Channel adapter. Native UMAD applications attach to port one.
    Hca,
    /// Switch, including its internal management port zero.
    Switch,
}

/// Node exported to ibsim.
#[derive(Clone, Debug)]
pub struct IbNode {
    /// Native node identifier, also used by `SIM_HOST`.
    pub name: String,
    /// Node type.
    pub kind: IbNodeKind,
    /// Physical port count, excluding switch port zero.
    pub ports: u8,
}

/// A physical IB cable endpoint.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IbEndpoint {
    /// Node identifier.
    pub node: String,
    /// One-based physical port number.
    pub port: u8,
}
impl IbEndpoint {
    /// Construct an endpoint, validated when added to the topology.
    pub fn new(node: impl Into<String>, port: u8) -> Self {
        Self {
            node: node.into(),
            port,
        }
    }
}

/// Physical cable and administrative state.
#[derive(Clone, Debug)]
pub struct IbLink {
    /// Identifier used for fault injection.
    pub id: String,
    /// Exclusively owned physical ports.
    pub endpoints: [IbEndpoint; 2],
    /// Whether the cable is connected.
    pub up: bool,
}

/// An IB port graph. OpenSM, not patchbay, assigns LIDs and forwarding tables.
#[derive(Clone, Debug, Default)]
pub struct IbTopology {
    pub(super) nodes: BTreeMap<String, IbNode>,
    pub(super) links: BTreeMap<String, IbLink>,
}
impl IbTopology {
    /// Start an empty subnet.
    pub fn new() -> Self {
        Self::default()
    }
    /// Add a node with a native-compatible name and 1..255 physical ports.
    pub fn add_node(&mut self, name: &str, kind: IbNodeKind, ports: u8) -> Result<()> {
        if name.is_empty()
            || name.len() > ibsim::NODE_ID_CAPACITY
            || ports == 0
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            bail!("IB node needs a 1..31 byte alphanumeric/_/- name and 1..255 ports");
        }
        if self.nodes.contains_key(name) {
            bail!("duplicate IB node {name}");
        }
        if self.nodes.len() >= 1024 {
            bail!("IB topology supports at most 1024 nodes");
        }
        self.nodes.insert(
            name.into(),
            IbNode {
                name: name.into(),
                kind,
                ports,
            },
        );
        Ok(())
    }
    /// Add a cable. Down cables still reserve both endpoints.
    pub fn add_link(&mut self, id: &str, endpoints: [IbEndpoint; 2], up: bool) -> Result<()> {
        if id.is_empty() || self.links.contains_key(id) {
            bail!("empty or duplicate IB link ID");
        }
        if endpoints[0].node == endpoints[1].node {
            bail!("IB self-links are unsupported");
        }
        for e in &endpoints {
            let n = self.nodes.get(&e.node).context("unknown IB node")?;
            if e.port == 0 || e.port > n.ports {
                bail!("IB port out of range");
            }
            if self.links.values().any(|l| l.endpoints.contains(e)) {
                bail!("IB port already cabled");
            }
        }
        self.links.insert(
            id.into(),
            IbLink {
                id: id.into(),
                endpoints,
                up,
            },
        );
        Ok(())
    }
    /// Look up a node.
    pub fn node(&self, name: &str) -> Option<&IbNode> {
        self.nodes.get(name)
    }
    /// Nodes in lexical order.
    pub fn nodes(&self) -> impl Iterator<Item = &IbNode> {
        self.nodes.values()
    }
    /// Cables in identifier order.
    pub fn links(&self) -> impl Iterator<Item = &IbLink> {
        self.links.values()
    }
    /// Emit native topology text. Disconnected cables are omitted until restored.
    pub fn render(&self) -> Result<String> {
        if self.nodes.is_empty() {
            bail!("empty IB topology");
        }
        let ports: usize = self.nodes.values().map(|n| usize::from(n.ports) + 1).sum();
        if ports > 32768 {
            bail!("IB topology exceeds 32768 ports");
        }
        let mut out = String::from("# patchbay IB graph; LIDs and LFTs are assigned by OpenSM.\n");
        for (index, n) in self.nodes.values().enumerate() {
            let (var, kind) = match n.kind {
                IbNodeKind::Hca => ("caguid", "Hca"),
                IbNodeKind::Switch => ("switchguid", "Switch"),
            };
            writeln!(
                out,
                "{var}=0x{:016x}",
                0x1000_0000_0000_0000u64 + index as u64 * 256
            )?;
            writeln!(out, "{kind} {} \"{}\"", n.ports, n.name)?;
            let mut peers = BTreeMap::new();
            for l in self.links.values().filter(|l| l.up) {
                for side in 0..2 {
                    if l.endpoints[side].node == n.name {
                        peers.insert(l.endpoints[side].port, &l.endpoints[1 - side]);
                    }
                }
            }
            for (port, peer) in peers {
                writeln!(out, "[{port}] \"{}\"[{}]", peer.node, peer.port)?;
            }
            out.push('\n');
        }
        Ok(out)
    }
}
