//! A real ibsim control client, using the pinned upstream safe wire types.
use std::{
    os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    },
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use ibsim::{ClientInfo, ControlMessage, ControlType, PortInfo, WireMessage};

static CONNECTION: AtomicU32 = AtomicU32::new(0x4000_0000);

fn address(name: String) -> Result<SocketAddr> {
    // Native make_name includes the trailing NUL in its abstract socket address.
    let mut name = name.into_bytes();
    name.push(0);
    Ok(SocketAddr::from_abstract_name(name)?)
}

pub(super) struct Client {
    control: UnixDatagram,
    _data: UnixDatagram,
    id: u32,
}
impl Client {
    pub(super) fn connect(base: &str, node: &str) -> Result<Self> {
        let id = CONNECTION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(1).filter(|n| *n <= i32::MAX as u32)
            })
            .ok()
            .context("ibsim connection ID overflow")?;
        let data = UnixDatagram::bind_addr(&address(format!("{base}:in{id}"))?)?;
        let control = UnixDatagram::bind_addr(&address(format!("{base}:ctl{id}"))?)?;
        control.set_read_timeout(Some(Duration::from_millis(300)))?;
        control.set_write_timeout(Some(Duration::from_millis(300)))?;
        control.connect_addr(&address(format!("{base}:ctl"))?)?;
        let info = ClientInfo::new(id, 0, false, node)?;
        let reply = exchange(&control, ControlMessage::connect(&info))?;
        let attached = reply.decode_payload::<ClientInfo>()?;
        if attached.id() >= ibsim::sys::IBSIM_MAX_CLIENTS as u32
            || attached.node_id() != node.as_bytes()
        {
            bail!("ibsim attached to an unexpected endpoint");
        }
        Ok(Self {
            control,
            _data: data,
            id: attached.id(),
        })
    }
    pub(super) fn port(&self) -> Result<PortInfo> {
        Ok(exchange(&self.control, ControlMessage::get_port(self.id))?.decode_payload()?)
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        // Best effort on teardown: server may already have exited. No response wait.
        let _ = self
            .control
            .send(&ControlMessage::disconnect(self.id).encode());
    }
}
fn exchange(socket: &UnixDatagram, request: ControlMessage) -> Result<ControlMessage> {
    socket.send(&request.encode())?;
    let mut bytes = vec![0u8; ControlMessage::WIRE_SIZE + 1];
    let n = socket.recv(&mut bytes)?;
    let reply = ControlMessage::decode(&bytes[..n])?;
    if reply.kind() == ControlType::Error
        || reply.kind() != request.kind()
        || reply.client_id() != request.client_id()
    {
        bail!("ibsim rejected or mismatched control request");
    }
    Ok(reply)
}
