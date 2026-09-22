# Native InfiniBand management fabric

Enable the `ibsim` Cargo feature and initialize recursive submodules. Build the
pinned `vendor/ibsim/ibsim` and `vendor/ibsim/umad2sim` Makefile targets with the
system libibmad/libibumad development packages. Install OpenSM and
infiniband-diags as native executables.

`IbTopology` describes HCAs, switches, physical ports and cables. Pass it to
`Lab::start_infiniband` with `IbOptions` containing absolute native artifact paths
and a fresh state directory. Start OpenSM with `start_subnet_manager`, query the
actual port state/LID with `port_status` or `wait_active`, and run real UMAD tools
with `run`. Change cable state through `set_link_up`; OpenSM receives native
traps and recomputes forwarding. The graph changes only after native console
acknowledgement. A timeout or dead server fails the operation.

ibsim and managed tools share a dedicated patchbay network namespace with no
Ethernet uplinks. ibsim's native abstract Unix datagram sockets carry MADs in
that namespace. `SIM_HOST` chooses the IB node attachment; OpenSM/ibsim enforce
the IB graph. This is not one kernel RDMA device per patchbay device. Native
UMAD clients attach to the first HCA port, or switch port zero. The adapter
uses the pinned upstream Rust wire types for real control exchanges.

`LD_PRELOAD` is scoped to managed UMAD children. Their working directory and
OpenSM cache are private to the fabric, isolating umad2sim's generated sysfs.
Explicit `shutdown` kills and reaps managed services; Drop kills owned process
groups. Console, diagnostic and OpenSM logs remain in the state directory.
Multiple fabrics have distinct namespaces and socket names.

Supported: real SMP/SA MAD execution, subnet discovery, LID assignment,
forwarding-table computation, management path tracing and cable fault recovery.
ibsim does **not** emulate verbs payloads, an RNIC, IPoIB, NCCL, CUDA, or packet
bandwidth/latency. There is no substitute collective or protocol implementation.

CI builds native ibsim and runs `native_management_and_fault_recovery` and
`native_isolated_namespace`. The first starts with a disconnected backup cable,
cuts the primary path, checks the actual diagnostic failure, reconnects the
backup and checks recovery; it also verifies dead-server errors do not update
the graph. These tests require Linux sockets/namespaces and native programs,
and are explicitly ignored in portable test runs.
