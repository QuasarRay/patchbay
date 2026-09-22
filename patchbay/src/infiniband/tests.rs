use super::*;

fn topology() -> IbTopology {
    let mut t = IbTopology::new();
    t.add_node("h1", IbNodeKind::Hca, 1).unwrap();
    t.add_node("h2", IbNodeKind::Hca, 1).unwrap();
    t.add_node("s1", IbNodeKind::Switch, 3).unwrap();
    t.add_node("s2", IbNodeKind::Switch, 3).unwrap();
    for (id, a, pa, b, pb, up) in [
        ("host1", "h1", 1, "s1", 1, true),
        ("host2", "h2", 1, "s2", 1, true),
        ("core", "s1", 2, "s2", 2, true),
        ("backup", "s1", 3, "s2", 3, false),
    ] {
        t.add_link(id, [IbEndpoint::new(a, pa), IbEndpoint::new(b, pb)], up)
            .unwrap();
    }
    t
}

#[test]
fn invalid_cables_and_native_command_injection_are_rejected_atomically() {
    let mut t = topology();
    let before = t.render().unwrap();
    assert!(t.add_node("x\"\nQuit", IbNodeKind::Hca, 1).is_err());
    assert!(t.add_node(&"a".repeat(32), IbNodeKind::Hca, 1).is_err());
    assert!(t
        .add_link(
            "dup",
            [IbEndpoint::new("s1", 3), IbEndpoint::new("s2", 3)],
            true
        )
        .is_err());
    assert!(t
        .add_link(
            "zero",
            [IbEndpoint::new("s1", 0), IbEndpoint::new("s2", 0)],
            true
        )
        .is_err());
    assert!(t
        .add_link(
            "absent",
            [IbEndpoint::new("missing", 1), IbEndpoint::new("s2", 9)],
            true
        )
        .is_err());
    assert_eq!(before, t.render().unwrap());
}

#[test]
fn exporter_preserves_parallel_ports_and_initial_disconnection() {
    let t = topology();
    let text = t.render().unwrap();
    assert!(text.contains("[2] \"s2\"[2]"));
    assert!(!text.contains("[3]"));
    assert_eq!(t.links().count(), 4);
    assert_eq!(text.lines().filter(|l| l.contains("guid=")).count(), 4);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires compiled native ibsim, umad2sim, OpenSM and infiniband-diags"]
async fn native_management_and_fault_recovery() -> Result<()> {
    let root =
        std::env::var("IBSIM_TEST_DIR").context("set IBSIM_TEST_DIR to a fresh directory")?;
    let options = IbOptions {
        binary: std::env::var("IBSIM_BIN")?.into(),
        umad_library: std::env::var("UMAD2SIM_LIB")?.into(),
        state_dir: root.into(),
    };
    let mut f = IbFabric::start(Scope::Current, topology(), options).await?;
    assert_eq!(f.port_status("h1")?.lid, 0);
    f.start_subnet_manager(
        "h1",
        std::env::var("OPENSM_BIN").unwrap_or_else(|_| "opensm".into()),
    )
    .await?;
    let a = f.wait_active("h1", Duration::from_secs(20)).await?;
    let b = f.wait_active("h2", Duration::from_secs(20)).await?;
    assert_ne!(a.lid, b.lid);
    let discovery = f
        .run("h1", Command::new("ibnetdiscover"), Duration::from_secs(20))
        .await?;
    assert!(
        discovery.status.success(),
        "{}",
        String::from_utf8_lossy(&discovery.stderr)
    );
    for n in ["h1", "h2", "s1", "s2"] {
        assert!(String::from_utf8_lossy(&discovery.stdout).contains(n));
    }
    let mut trace = Command::new("ibtracert");
    trace.args([a.lid.to_string(), b.lid.to_string()]);
    assert!(f
        .run("h1", trace, Duration::from_secs(20))
        .await?
        .status
        .success());
    f.set_link_up("core", false).await?;
    let mut probe = Command::new("smpquery");
    probe.args(["-D", "-t", "100", "nodedesc", "0,1,2,1"]);
    assert!(!f
        .run("h1", probe, Duration::from_secs(10))
        .await?
        .status
        .success());
    assert!(!f.topology.links["backup"].up);
    f.set_link_up("backup", true).await?;
    f.wait_active("h2", Duration::from_secs(20)).await?;
    let mut recovered = Command::new("smpquery");
    recovered.args(["-D", "nodedesc", "0,1,3,1"]);
    let recovered = f.run("h1", recovered, Duration::from_secs(20)).await?;
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(String::from_utf8_lossy(&recovered.stdout).contains("h2"));
    f.server.stop().await?;
    assert!(f.set_link_up("core", true).await.is_err());
    assert!(!f.topology.links["core"].up);
    f.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires network namespaces and compiled native InfiniBand programs"]
async fn native_isolated_namespace() -> Result<()> {
    let root = format!("{}-namespace", std::env::var("IBSIM_TEST_DIR")?);
    let options = IbOptions {
        binary: std::env::var("IBSIM_BIN")?.into(),
        umad_library: std::env::var("UMAD2SIM_LIB")?.into(),
        state_dir: root.into(),
    };
    let lab = Lab::new().await?;
    let mut fabric = lab.start_infiniband(topology(), options).await?;
    fabric.start_subnet_manager("h1", "opensm").await?;
    fabric.wait_active("h2", Duration::from_secs(20)).await?;
    let result = fabric
        .run("h1", Command::new("ibnetdiscover"), Duration::from_secs(20))
        .await?;
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("h2"));
    fabric.shutdown().await?;
    fabric.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires compiled native ibsim and umad2sim"]
async fn native_cancelled_mutation_fails_closed() -> Result<()> {
    let options = IbOptions {
        binary: std::env::var("IBSIM_BIN")?.into(),
        umad_library: std::env::var("UMAD2SIM_LIB")?.into(),
        state_dir: format!("{}-cancelled", std::env::var("IBSIM_TEST_DIR")?).into(),
    };
    let mut fabric = IbFabric::start(Scope::Current, topology(), options).await?;
    killpg(
        Pid::from_raw(fabric.server.0.id().context("server PID")? as i32),
        Signal::SIGSTOP,
    )?;
    assert!(timeout(
        Duration::from_millis(100),
        fabric.set_link_up("core", false)
    )
    .await
    .is_err());
    assert!(fabric.failed);
    assert!(fabric.topology.links["core"].up);
    assert!(fabric.set_link_up("backup", true).await.is_err());
    fabric.shutdown().await?;
    Ok(())
}
