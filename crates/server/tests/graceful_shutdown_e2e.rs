//! Graceful shutdown, end to end over a real process (Unix only — SIGTERM).
//!
//! Spawns the actual `xyzdb-server` binary, writes data, sends SIGTERM, and
//! asserts the server exits CLEANLY (status 0) within a bounded time — proving
//! the signal handler ran the drain+flush path rather than the process hanging
//! or being force-killed. Then it restarts on the same data dir and confirms the
//! data survived: the memtable flush + WAL are durable across a graceful stop,
//! and the clean-shutdown marker means the restart is clean.
#![cfg(unix)]

// SPDX-License-Identifier: BUSL-1.1
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use xyzdb_server::protocol::{self, STATUS_OK};

/// A likely-free port: bind :0, read the port, drop the listener. A small race
/// window remains, acceptable for a local test.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    l.local_addr().unwrap().port()
}

fn spawn_server(dir: &std::path::Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_xyzdb-server"))
        .args([
            "--path",
            dir.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--bind",
            "127.0.0.1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn xyzdb-server")
}

async fn wait_listening(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

async fn query(stream: &mut TcpStream, q: &str) -> (u8, String) {
    protocol::write_request_v1(stream, q).await.expect("send");
    let (status, bytes) = protocol::read_response_raw(stream).await.expect("recv");
    (status, String::from_utf8(bytes).expect("utf8"))
}

#[tokio::test]
async fn sigterm_shuts_down_cleanly_and_data_survives_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let port = free_port();

    // 1) Start, write 30 records into one gravity bucket (they land in the
    //    active memtable — the state a graceful stop must flush).
    let mut server = spawn_server(dir.path(), port);
    assert!(
        wait_listening(port, Duration::from_secs(15)).await,
        "server did not start listening"
    );
    {
        let mut s = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let (st, p) = query(&mut s, r#"LOBE "mem""#).await;
        assert_eq!(st, STATUS_OK, "LOBE: {p}");
        for i in 0..30 {
            let (st, p) = query(
                &mut s,
                &format!(r#"PUT {{*conv:"c1", id:"r{i}", body:"m{i}"}} IN "mem""#),
            )
            .await;
            assert_eq!(st, STATUS_OK, "PUT r{i}: {p}");
        }
        // Drop the client so there is no in-flight connection at signal time;
        // the drain path is exercised separately by the timeout logic.
    }

    // 2) SIGTERM → must exit cleanly (0) within the drain + flush budget. A hang
    //    or a non-zero status means the handler failed.
    // SAFETY: kill(2) with a pid we own and a standard signal; no memory touched.
    let killed = unsafe { libc::kill(server.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(killed, 0, "kill(SIGTERM) failed");
    let status = wait_exit(&mut server, Duration::from_secs(20))
        .expect("server did not exit within 20s of SIGTERM (hang?)");
    assert!(
        status.success(),
        "server exited non-zero on SIGTERM: {status:?}"
    );

    // 3) The clean-shutdown marker must exist: it is written only when the
    //    graceful path runs to the end. A SIGKILL / skipped-Drop exit would NOT
    //    write it, and the next open would rebuild ghosts + lean on the WAL. This
    //    goes RED the day graceful_shutdown stops running.
    let marker = dir.path().join("meta").join("clean_shutdown");
    assert!(
        marker.exists(),
        "clean-shutdown marker not written: {marker:?}"
    );

    // 4) Delete the WAL so recovery has NO replay crutch. This is the load-bearing
    //    distinction the whole commit is about: the OLD world (no graceful flush)
    //    would recover these 30 records via WAL replay and PASS a naive "data
    //    survived" check — so that check proves nothing new. With the WAL removed,
    //    survival can only mean the memtable was FLUSHED to SST on shutdown. Break
    //    the spatial/vectors flush and this step goes RED, while a WAL-replay-only
    //    world would fail here outright.
    let wal = dir.path().join("journal.wal");
    assert!(wal.exists(), "expected WAL at {wal:?}");
    std::fs::remove_file(&wal).expect("remove WAL");

    // 5) Restart on the same data dir (WAL gone) → the records must still be
    //    there, sourced from the flushed SSTs, not from a replay.
    let mut server2 = spawn_server(dir.path(), port);
    assert!(
        wait_listening(port, Duration::from_secs(15)).await,
        "server did not restart"
    );
    let mut s2 = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("reconnect");
    let (st, payload) = query(&mut s2, r#"SCAN "mem" WHERE conv="c1" LIMIT 100"#).await;
    assert_eq!(st, STATUS_OK, "SCAN after restart: {payload}");
    // First AND last writes survived a WAL-less restart → the full range reached
    // the SSTs via the shutdown flush.
    assert!(
        payload.contains("\"r0\"") && payload.contains("\"r29\""),
        "records lost across graceful restart with the WAL removed (flush broken?); payload: {payload}"
    );

    // 6) The restart must have taken the CLEAN-recovery path, not merely returned
    //    data. Open consumes the marker (present → remove → skip the unclean
    //    ghost rebuild). Step 3 proved it existed at shutdown; its absence now —
    //    open runs recovery synchronously before it starts listening, so it is
    //    gone by the time this query returned — proves the clean branch ran. Had
    //    open taken the unclean path (marker ignored / ghosts rebuilt), the
    //    marker would still be here. This is the startup-side analogue of the
    //    WAL-deletion check: it distinguishes "opened clean" from "opened anyway".
    assert!(
        !marker.exists(),
        "clean-shutdown marker not consumed on restart: open did not take the clean-recovery path ({marker:?})"
    );

    // Cleanup: the test is done; force-stop the second server.
    let _ = server2.kill();
    let _ = server2.wait();
}

/// Ticket 3 (0.9.2): graceful shutdown must flush the `vectors` keyspace too.
///
/// The base test writes non-vector records; this one declares a searchable
/// vector and writes HOISTED embeddings (>= `VECTOR_F32_MIN_DIMS`=64 dims, so
/// the list literal packs into a `Value::Vector` and lands in the `vectors`
/// keyspace — a shorter literal stays an inline `Value::List` and never
/// populates it). SIGTERM, DELETE the WAL, restart, then NEAREST: survival can
/// only mean the vectors memtable was FLUSHED on shutdown, not replayed. A real
/// process + SIGTERM + restart reads only disk, so no in-process leak can mask a
/// lost keyspace. Break shutdown's vectors flush and this goes RED — closing the
/// crash/compact/shutdown trilogy for vectors (F1 + T2 + this).
#[tokio::test]
async fn sigterm_flushes_hoisted_vectors_and_nearest_survives_restart() {
    // 64-D literal so the executor packs it into a `Value::Vector` (hoisted).
    fn emb(coords: &[(usize, f32)]) -> String {
        let mut v = vec![0.0f32; 64];
        for &(i, x) in coords {
            v[i] = x;
        }
        format!(
            "[{}]",
            v.iter()
                .map(|f| format!("{f:.1}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let port = free_port();

    let mut server = spawn_server(dir.path(), port);
    assert!(
        wait_listening(port, Duration::from_secs(15)).await,
        "server did not start listening"
    );
    {
        let mut s = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let (st, p) = query(&mut s, r#"LOBE "vec""#).await;
        assert_eq!(st, STATUS_OK, "LOBE: {p}");
        let (st, p) = query(&mut s, r#"VECTOR emb IN "vec""#).await;
        assert_eq!(st, STATUS_OK, "VECTOR: {p}");
        for (id, coords) in [
            ("r0", &[(0usize, 1.0f32)][..]),
            ("r1", &[(1, 1.0)][..]),
            ("r2", &[(2, 1.0)][..]),
        ] {
            let (st, p) = query(
                &mut s,
                &format!(
                    r#"PUT {{*conv:"c1", id:"{id}", emb:{}}} IN "vec""#,
                    emb(coords)
                ),
            )
            .await;
            assert_eq!(st, STATUS_OK, "PUT {id}: {p}");
        }
    }

    // SIGTERM → clean exit within the drain + flush budget.
    // SAFETY: kill(2) with a pid we own and a standard signal; no memory touched.
    let killed = unsafe { libc::kill(server.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(killed, 0, "kill(SIGTERM) failed");
    let status = wait_exit(&mut server, Duration::from_secs(20))
        .expect("server did not exit within 20s of SIGTERM (hang?)");
    assert!(
        status.success(),
        "server exited non-zero on SIGTERM: {status:?}"
    );

    // Delete the WAL so survival can ONLY come from the shutdown flush, not a
    // replay — the load-bearing distinction (mirrors the base test's step 4).
    let wal = dir.path().join("journal.wal");
    assert!(wal.exists(), "expected WAL at {wal:?}");
    std::fs::remove_file(&wal).expect("remove WAL");

    // Restart (WAL gone) → NEAREST must still rank the hoisted vectors: the
    // vectors keyspace reached the SSTs via the shutdown flush.
    let mut server2 = spawn_server(dir.path(), port);
    assert!(
        wait_listening(port, Duration::from_secs(15)).await,
        "server did not restart"
    );
    let mut s2 = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("reconnect");
    // The query touches all three axes so every record scores > 0; had the
    // vectors been lost, the records would be unscorable and NEAREST would
    // return none.
    let (st, payload) = query(
        &mut s2,
        &format!(
            r#"SCAN "vec" WHERE conv="c1" | NEAREST(emb, {}, 3, cosine)"#,
            emb(&[(0, 1.0), (1, 1.0), (2, 1.0)])
        ),
    )
    .await;
    assert_eq!(st, STATUS_OK, "NEAREST after restart: {payload}");
    assert!(
        payload.contains("\"r0\"") && payload.contains("\"r1\"") && payload.contains("\"r2\""),
        "hoisted vectors lost across graceful restart with the WAL removed \
         (shutdown vectors flush broken?); payload: {payload}"
    );

    let _ = server2.kill();
    let _ = server2.wait();
}
