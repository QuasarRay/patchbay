//! Native InfiniBand subnet management in a dedicated patchbay namespace.
//!
//! ibsim and UMAD consumers share an isolated transport namespace; `SIM_HOST`
//! selects their IB attachment. The IB graph is enforced by ibsim, independently
//! of Ethernet routes. This does not provide an RDMA verbs device, IPoIB, CUDA,
//! NCCL, or bandwidth/latency emulation. No forwarding algorithm is substituted
//! for OpenSM or ibsim.
mod control;
mod log;
mod topology;
use std::{
    fs,
    io::Write,
    os::unix::{fs::DirBuilderExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{Instant, timeout},
};
pub use topology::{IbEndpoint, IbLink, IbNode, IbNodeKind, IbTopology};

use crate::{Device, IfaceConfig, Lab};

static FABRIC: AtomicU64 = AtomicU64::new(0);
static COMMAND: AtomicU64 = AtomicU64::new(0);

/// Native programs and a fresh output directory. Build both artifacts from the
/// same pinned `vendor/ibsim` source tree.
#[derive(Clone, Debug)]
pub struct IbOptions {
    /// Path to the native ibsim executable.
    pub binary: PathBuf,
    /// Path to libumad2sim.so, preloaded only into managed UMAD processes.
    pub umad_library: PathBuf,
    /// A new directory; existing directories are never overwritten.
    pub state_dir: PathBuf,
}

/// Port state read from the running native simulator, never from the input graph.
#[derive(Clone, Copy, Debug)]
pub struct IbPortStatus {
    /// LID assigned by the subnet manager; zero before assignment.
    pub lid: u16,
    /// Native IB port state (1 = Down, 4 = Active).
    pub state: u8,
}

enum Scope {
    Namespace {
        device: Device,
        lab: Lab,
    },
    #[cfg(test)]
    Current,
}
impl Scope {
    fn spawn(&self, cmd: Command) -> Result<Child> {
        match self {
            Self::Namespace { device, .. } => device.spawn_command(cmd),
            #[cfg(test)]
            Self::Current => Ok({
                let mut cmd = cmd;
                cmd.spawn()?
            }),
        }
    }
    fn port(&self, base: String, node: String) -> Result<IbPortStatus> {
        let query = move || {
            let p = control::Client::connect(&base, &node)?.port()?;
            Ok(IbPortStatus {
                lid: p.lid,
                state: p.state,
            })
        };
        match self {
            Self::Namespace { device, .. } => device.run_sync(query),
            #[cfg(test)]
            Self::Current => query(),
        }
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        match self {
            Self::Namespace { device, lab } => {
                // The namespace may already have been explicitly removed by its owner.
                let _ = lab.remove_device(device.id());
            }
            #[cfg(test)]
            Self::Current => {}
        }
    }
}

struct Process(Child);
impl Process {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
        if let Some(pid) = self.0.id() {
            let status = waitid(
                Id::Pid(Pid::from_raw(pid as i32)),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            )?;
            if status == WaitStatus::StillAlive {
                return Ok(None);
            }
            // Kill pipe-holding descendants while the zombie leader still pins
            // this process-group identity. Only then let Tokio reap the leader.
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        self.0.try_wait()
    }
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    fn signal(&mut self) {
        if let Some(id) = self.0.id() {
            // Each child has its own process group. ESRCH means it already exited.
            let _ = killpg(Pid::from_raw(id as i32), Signal::SIGKILL);
            let _ = self.0.start_kill();
        }
    }
    async fn stop(&mut self) -> Result<()> {
        self.signal();
        self.0.wait().await?;
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.signal();
    }
}

/// Owns the native server, optional OpenSM process, graph and isolated namespace.
/// Dropping it terminates the owned process groups. Use `shutdown` to also wait
/// for reaping before returning from an async runtime.
pub struct IbFabric {
    server: Process,
    manager: Option<Process>,
    input: ChildStdin,
    output: ChildStdout,
    transcript: log::CappedFile,
    logs: log::Budget,
    service_logs: Vec<log::Completion>,
    commands: usize,
    topology: IbTopology,
    options: IbOptions,
    basename: String,
    failed: bool,
    scope: Scope,
}

impl Lab {
    /// Start native ibsim in an isolated namespace without Ethernet uplinks.
    /// Nodes/ports refer to the IB graph; UMAD processes attach by node name.
    pub async fn start_infiniband(
        &self,
        topology: IbTopology,
        options: IbOptions,
    ) -> Result<IbFabric> {
        topology.render()?;
        let name = format!("ibsim-{}", FABRIC.fetch_add(1, Ordering::Relaxed));
        let device = self
            .add_device(&name)
            .iface("ibctl", IfaceConfig::dummy())
            .build()
            .await?;
        IbFabric::start(
            Scope::Namespace {
                device,
                lab: self.clone(),
            },
            topology,
            options,
        )
        .await
    }
}

impl IbFabric {
    async fn start(scope: Scope, topology: IbTopology, mut options: IbOptions) -> Result<Self> {
        let text = topology.render()?;
        options.binary = fs::canonicalize(&options.binary)
            .context("native ibsim binary; build vendor/ibsim first")?;
        options.umad_library =
            fs::canonicalize(&options.umad_library).context("native umad2sim library")?;
        if options
            .umad_library
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|b| b.is_ascii_whitespace() || *b == b':')
        {
            bail!("LD_PRELOAD path cannot contain whitespace or colon");
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&options.state_dir)
            .context("ibsim state_dir must be new")?;
        options.state_dir = fs::canonicalize(&options.state_dir)?;
        fs::write(options.state_dir.join("topology.net"), text)?;
        let basename = format!(
            "pb-{}-{}",
            std::process::id(),
            FABRIC.fetch_add(1, Ordering::Relaxed)
        );
        let logs = log::Budget::new();
        let (stderr, stderr_done) = logs.capture(&options.state_dir.join("ibsim.stderr.log"))?;
        let mut cmd = Command::new(&options.binary);
        cmd.arg("-s").args([
            "-N",
            &(topology.nodes.len() + 1).to_string(),
            "-S",
            &(topology.nodes.len() + 1).to_string(),
            "-P",
            &(topology
                .nodes
                .values()
                .map(|n| usize::from(n.ports) + 1)
                .sum::<usize>()
                + 1)
            .to_string(),
        ]);
        cmd.arg(options.state_dir.join("topology.net"));
        cmd.current_dir(&options.state_dir)
            .env("IBSIM_SOCKNAME", &basename)
            .env_remove("LD_PRELOAD");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true);
        cmd.as_std_mut().process_group(0);
        let mut server = Process(scope.spawn(cmd)?);
        let input = server.0.stdin.take().context("ibsim stdin")?;
        let output = server.0.stdout.take().context("ibsim stdout")?;
        let transcript = logs.file(&options.state_dir.join("ibsim.console.log"))?;
        let mut fabric = Self {
            server,
            manager: None,
            input,
            output,
            transcript,
            logs,
            service_logs: vec![stderr_done],
            commands: 0,
            topology,
            options,
            basename,
            failed: false,
            scope,
        };
        fabric.prompt().await.context("native ibsim startup")?;
        // The console prompt is printed before socket setup by native ibsim.
        // A real control exchange is the readiness check, including startup errors.
        let first = fabric
            .topology
            .nodes
            .keys()
            .next()
            .expect("validated topology")
            .clone();
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            fabric.healthy()?;
            match fabric.port_status(&first) {
                Ok(_) => break,
                Err(error) if Instant::now() >= end => {
                    return Err(error.context("ibsim control socket readiness"));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        Ok(fabric)
    }
    /// The current IB graph, updated only after native console acknowledgement.
    pub fn topology(&self) -> &IbTopology {
        &self.topology
    }
    /// Directory containing native reports and process logs.
    pub fn state_dir(&self) -> &Path {
        &self.options.state_dir
    }
    fn healthy(&mut self) -> Result<()> {
        if self.failed {
            bail!("ibsim backend is failed; rebuild it before further operations");
        }
        if let Some(status) = self.server.try_wait()? {
            self.failed = true;
            bail!("native ibsim exited: {status}");
        }
        if let Some(manager) = &mut self.manager {
            if let Some(status) = manager.try_wait()? {
                bail!("OpenSM exited: {status}");
            }
        }
        Ok(())
    }
    async fn prompt(&mut self) -> Result<String> {
        let result = timeout(Duration::from_secs(5), async {
            let mut data = Vec::new();
            loop {
                let b = self.output.read_u8().await?;
                data.push(b);
                if data.ends_with(b"sim> ") {
                    break;
                }
                if data.len() > 1_048_576 {
                    bail!("ibsim console response too large");
                }
            }
            self.transcript.write_all(&data)?;
            Ok(String::from_utf8(data[..data.len() - 5].to_vec())?)
        })
        .await;
        match result {
            Ok(Ok(text)) => Ok(text),
            error => {
                self.failed = true;
                self.server.signal();
                bail!("ibsim console failed: {error:?}");
            }
        }
    }
    /// Connect or disconnect the native cable. OpenSM receives the upstream traps.
    pub async fn set_link_up(&mut self, id: &str, up: bool) -> Result<()> {
        self.healthy()?;
        let link = self.topology.links.get(id).context("unknown IB link")?;
        if link.up == up {
            return Ok(());
        }
        let [a, b] = &link.endpoints;
        let line = if up {
            format!(
                "Link \"{}\"[{}] \"{}\"[{}]\n",
                a.node, a.port, b.node, b.port
            )
        } else {
            format!("Unlink \"{}\"[{}]\n", a.node, a.port)
        };
        // Cancellation after sending a command leaves its outcome unknown.
        // Keep the backend failed until both acknowledgement and model commit,
        // so a dropped future cannot make the next call consume a stale prompt.
        self.failed = true;
        self.input.write_all(line.as_bytes()).await?;
        let response = self.prompt().await?;
        if !response.trim().is_empty() {
            bail!("ibsim rejected cable change: {response}");
        }
        self.topology.links.get_mut(id).expect("validated link").up = up;
        self.failed = false;
        Ok(())
    }
    /// Query the native first HCA port, or switch port zero.
    pub fn port_status(&mut self, node: &str) -> Result<IbPortStatus> {
        self.healthy()?;
        if !self.topology.nodes.contains_key(node) {
            bail!("unknown IB node {node}");
        }
        self.scope.port(self.basename.clone(), node.into())
    }
    fn prepare(&self, node: &str, cmd: &mut Command) -> Result<()> {
        if !self.topology.nodes.contains_key(node) {
            bail!("unknown IB node {node}");
        }
        cmd.current_dir(&self.options.state_dir)
            .env("LD_PRELOAD", &self.options.umad_library)
            .env("SIM_HOST", node)
            .env("IBSIM_SOCKNAME", &self.basename)
            .env_remove("SIM_SET_ISSM")
            .env_remove("IBSIM_SERVER_NAME")
            .env_remove("IBSIM_SERVER_PORT")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd.as_std_mut().process_group(0);
        Ok(())
    }
    /// Start an unmodified OpenSM in the foreground, attached to an HCA.
    pub async fn start_subnet_manager(
        &mut self,
        node: &str,
        opensm: impl AsRef<Path>,
    ) -> Result<()> {
        self.healthy()?;
        if self.manager.is_some() {
            bail!("OpenSM already started");
        }
        if self.topology.node(node).context("unknown SM node")?.kind != IbNodeKind::Hca {
            bail!("attach OpenSM to an HCA");
        }
        let cache = self.options.state_dir.join("opensm-cache");
        fs::create_dir(&cache)?;
        let mut cmd = Command::new(opensm.as_ref());
        self.prepare(node, &mut cmd)?;
        cmd.args(["-s", "1", "-f"]).arg("/dev/stdout");
        cmd.env("OSM_CACHE_DIR", &cache).env("OSM_TMP_DIR", &cache);
        let (stdout, out_done) = self
            .logs
            .capture(&self.options.state_dir.join("opensm.stdout.log"))?;
        let (stderr, err_done) = self
            .logs
            .capture(&self.options.state_dir.join("opensm.stderr.log"))?;
        cmd.stdout(stdout).stderr(stderr);
        self.service_logs.extend([out_done, err_done]);
        self.manager = Some(Process(self.scope.spawn(cmd)?));
        self.wait_active(node, Duration::from_secs(30)).await?;
        Ok(())
    }
    /// Wait for a real nonzero LID and Active port, with a finite deadline.
    pub async fn wait_active(&mut self, node: &str, duration: Duration) -> Result<IbPortStatus> {
        if duration.is_zero() || duration > Duration::from_secs(600) {
            bail!("IB readiness deadline must be in (0,600] seconds");
        }
        let end = Instant::now() + duration;
        loop {
            let port = self.port_status(node)?;
            if port.state == 4 && port.lid != 0 {
                return Ok(port);
            }
            if Instant::now() >= end {
                bail!("IB port {node} did not become Active: {port:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    /// Execute a native UMAD tool at a graph node. Returns its actual exit status.
    /// Pipes are continuously drained into capped files (16 MiB each, 64 MiB per fabric).
    /// At most 1024 commands retain logs; exhausted budgets fail without unbounded growth.
    pub async fn run(
        &mut self,
        node: &str,
        mut cmd: Command,
        deadline: Duration,
    ) -> Result<Output> {
        self.healthy()?;
        if deadline.is_zero() || deadline > Duration::from_secs(600) {
            bail!("IB command deadline must be in (0,600] seconds");
        }
        self.prepare(node, &mut cmd)?;
        if self.commands >= 1024 {
            bail!("IB command retention limit reached; start a new fabric");
        }
        self.commands += 1;
        let id = COMMAND.fetch_add(1, Ordering::Relaxed);
        let stdout = self
            .options
            .state_dir
            .join(format!("command-{id}.stdout.log"));
        let stderr = self
            .options
            .state_dir
            .join(format!("command-{id}.stderr.log"));
        let (out, out_done) = self.logs.capture(&stdout)?;
        let (err, err_done) = self.logs.capture(&stderr)?;
        cmd.stdout(out).stderr(err);
        let mut process = Process(self.scope.spawn(cmd)?);
        let status = match timeout(deadline, process.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                process.stop().await?;
                bail!("native IB command timed out");
            }
        };
        out_done.finish().await?;
        err_done.finish().await?;
        Ok(Output {
            status,
            stdout: fs::read(stdout)?,
            stderr: fs::read(stderr)?,
        })
    }
    /// Terminate and reap both native services. Safe to call more than once.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.failed = true;
        if let Some(manager) = &mut self.manager {
            manager.stop().await?;
        }
        self.manager = None;
        self.server.stop().await?;
        let mut error = None;
        while let Some(log) = self.service_logs.pop() {
            if let Err(e) = log.finish().await {
                error = Some(e);
            }
        }
        if let Some(error) = error {
            return Err(error.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
