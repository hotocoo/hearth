#[test]
fn reads_gpu_memory_from_amdgpu_sysfs_and_nvidia_smi() {
    let drm = std::env::temp_dir().join(format!("hearth-drm-{}", std::process::id()));
    let _ = fs::remove_dir_all(&drm);
    let dev = drm.join("card1/device");
    fs::create_dir_all(&dev).unwrap();
    fs::create_dir_all(drm.join("card1-DP-1")).unwrap();
    fs::write(dev.join("mem_info_vram_total"), "25753026560\n").unwrap();
    fs::write(dev.join("mem_info_vram_used"), "1073741824\n").unwrap();
    fs::write(dev.join("product_name"), "Radeon RX 7900 XTX\n").unwrap();
    // An APU: a small carve-out, a large GTT.
    let apu = drm.join("card2/device");
    fs::create_dir_all(&apu).unwrap();
    fs::write(apu.join("mem_info_vram_total"), "536870912").unwrap();
    fs::write(apu.join("mem_info_gtt_total"), "68719476736").unwrap();
    let amd = amd_gpus(&drm).unwrap();
    assert_eq!(amd.len(), 2);
    assert_eq!(amd[0].name, "Radeon RX 7900 XTX");
    assert!(!amd[0].unified_memory);
    assert!(amd[1].unified_memory && amd[1].total_bytes == Some(68_719_476_736));
    assert_eq!(amd[1].name, "AMD GPU card2");
    assert_eq!(amd[0].used_bytes, Some(1_073_741_824));
    fs::remove_dir_all(drm).unwrap();
    let nv = parse_nvidia_smi("NVIDIA GeForce RTX 4090, 24564, 1234\nbroken line\n");
    assert_eq!(nv.len(), 1);
    assert_eq!(nv[0].total_bytes, Some(24564 * 1024 * 1024));
    // A process on two GPUs is one entry, summed.
    let apps = parse_compute_apps("4242, 20480\n77, 512\n4242, 1024\nbad\n");
    assert_eq!(apps, [(4242, 21504 * 1024 * 1024), (77, 512 * 1024 * 1024)]);
    let amd = "pos:\t0\nflags:\t02100002\ndrm-driver:\tamdgpu\ndrm-client-id:\t17\ndrm-memory-vram:\t8388608 KiB\ndrm-memory-gtt:\t2048 KiB\n";
    assert_eq!(parse_drm_fdinfo(amd), Ok(Some((17, 8_589_934_592))));
    assert_eq!(parse_drm_fdinfo("pos:\t0\nflags:\t0100002\n"), Ok(None));
    // Newer kernels: resident device memory wins over the totals.
    let xe = "drm-client-id:\t4\ndrm-total-vram0:\t4 GiB\ndrm-resident-vram0:\t3 GiB\ndrm-resident-system:\t1 GiB\ndrm-memory-vram0:\t4 GiB\n";
    assert_eq!(parse_drm_fdinfo(xe), Ok(Some((4, 3 << 30))));
    let odd = "drm-client-id:\t5\ndrm-memory-vram:\t7 TiB\n";
    assert!(parse_drm_fdinfo(odd).is_err());
}

#[test]
fn the_tightest_cgroup_limit_over_the_process_binds() {
    let root = std::env::temp_dir().join(format!("hearth-cg-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let leaf = root.join("slice/app");
    fs::create_dir_all(&leaf).unwrap();
    let w = |dir: &std::path::Path, max: &str, current: &str, inactive: u64| {
        fs::write(dir.join("memory.max"), max).unwrap();
        fs::write(dir.join("memory.current"), current).unwrap();
        fs::write(
            dir.join("memory.stat"),
            format!("anon 1\ninactive_file {inactive}\n"),
        )
        .unwrap();
    };
    w(&leaf, "max", "100", 0);
    w(&root.join("slice"), "1000", "900", 50);
    // The leaf has no limit; its parent slice does, with 150 left.
    let cg = cgroup_memory_at(&root, "/slice/app");
    assert_eq!(
        (cg.max, cg.current, cg.available),
        (Some(1000), Some(900), Some(150))
    );
    // A smaller limit with more room still bounds the total.
    w(&leaf, "500", "10", 0);
    let cg = cgroup_memory_at(&root, "/slice/app");
    assert_eq!((cg.max, cg.available), (Some(500), Some(150)));
    assert_eq!(cgroup_memory_at(&root, "/nowhere"), Cgroup::default());
    fs::remove_dir_all(root).unwrap();
}

use super::*;

#[test]
fn kb_fields_convert_to_bytes() {
    let text =
        "MemTotal:       16303428 kB\nMemAvailable:    9000000 kB\nHugePages_Total:       0\n";
    assert_eq!(kb_field(text, "MemTotal"), Some(16_303_428 * 1024));
    assert_eq!(kb_field(text, "HugePages_Total"), Some(0));
    assert_eq!(kb_field(text, "Mem"), None);
}

#[test]
fn psi_avg10_reads_both_lines() {
    let text = "some avg10=1.50 avg60=0.10 avg300=0.00 total=123\nfull avg10=0.25 avg60=0.00 avg300=0.00 total=9\n";
    assert_eq!(psi_avg10(text, "some"), Some(1.5));
    assert_eq!(psi_avg10(text, "full"), Some(0.25));
}

#[test]
fn live_host_reading_is_plausible() {
    let host = host_memory().unwrap();
    assert!(host.available_bytes <= host.total_bytes);
}

#[test]
fn live_own_counts_and_limits() {
    let me = process_counts(std::process::id().cast_signed()).unwrap();
    assert!(me.open_files.unwrap() >= 3);
    assert!(me.threads.unwrap() >= 1);
    let limits = system_limits().unwrap();
    assert!(limits.files_open.unwrap() <= limits.files_max.unwrap());
    assert!(limits.threads_total.unwrap() >= me.threads.unwrap());
}

#[test]
fn proc_net_tcp_lines_parse() {
    let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0 100 0 0 10 0\n   1: 0100007F:C350 0100007F:1F90 08 00000000:00000000 00:00000000 00000000  1000        0 4343 1 0 100 0 0 10 0\n";
    let parsed = parse_proc_net_tcp(text);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].0, 4242);
    assert_eq!(parsed[0].1.local, "127.0.0.1:8080".parse().unwrap());
    assert_eq!(parsed[0].1.state, TcpState::Listen);
    assert!(parsed[0].1.remote.is_none());
    assert_eq!(parsed[1].1.state, TcpState::CloseWait);
    assert_eq!(parsed[1].1.remote, Some("127.0.0.1:8080".parse().unwrap()));
    let v6 = parse_proc_addr("00000000000000000000000001000000:0050").unwrap();
    assert_eq!(v6, "[::1]:80".parse().unwrap());
}

#[test]
fn live_listening_socket_is_seen() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let sockets = tcp_sockets(std::process::id().cast_signed()).unwrap();
    assert!(
        sockets
            .iter()
            .any(|s| s.state == TcpState::Listen && s.local.port() == port)
    );
}

#[test]
fn live_own_footprint_is_readable() {
    let fp = process_footprint(std::process::id().cast_signed()).unwrap();
    assert!(fp.footprint_bytes > 0);
}
