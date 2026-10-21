//! 两个操作系统进程同时创建 Run，不能因 `SQLITE_BUSY` 失败。
//!
//! 同一进程里两把 `Mutex<Connection>` 串行化的是我们自己的锁，测不到 SQLite 写锁。
//! 所以把本测试二进制再拉起两次，各开一条连接到同一文件。
//! WAL + `busy_timeout` 5 秒让后来者等待，而不是立刻报忙。

use std::process::Command;

use fluxaxiom_run_host::RunHost;

mod common;

/// 子进程看到这个变量就只写库，不再继续拉孙子进程。
const WORKER_ENV: &str = "FLUXAXIOM_HOST_TEST_WORKER";
const DB_ENV: &str = "FLUXAXIOM_HOST_TEST_DB";
/// 每个进程插入的条数。要足以撞上写锁，又不必把测试拖成秒级等待。
const PER_WORKER: usize = 20;

#[test]
fn two_processes_write_without_sqlite_busy() {
    if let Ok(worker) = std::env::var(WORKER_ENV) {
        write_runs(&worker);
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host.db");
    // 父进程先迁移 schema，并保持连接，模拟另一个 `flux status` 在读的时候有人在写。
    let host = RunHost::open(&path).unwrap();

    let executable = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for worker in ["a", "b"] {
        children.push(
            Command::new(&executable)
                // `--exact` 避免过滤器把别的测试也跑进子进程。
                .args(["--exact", "two_processes_write_without_sqlite_busy"])
                .env(WORKER_ENV, worker)
                .env(DB_ENV, &path)
                .spawn()
                .expect("spawn concurrent writer"),
        );
    }

    for child in &mut children {
        let status = child.wait().expect("wait for concurrent writer");
        assert!(status.success(), "concurrent writer failed: {status}");
    }

    // 每个 worker 使用互不重复的 `request_id`，测的是并发插入，不是幂等碰撞。
    for worker in ["a", "b"] {
        for index in 0..PER_WORKER {
            let request_id = format!("{worker}-{index}");
            host.get_by_request_id(&request_id)
                .unwrap_or_else(|_| panic!("missing run {request_id}"));
        }
    }
}

fn write_runs(worker: &str) {
    let path = std::env::var(DB_ENV).expect("worker database path");
    let host = RunHost::open(path).unwrap();
    for index in 0..PER_WORKER {
        host.create_run(common::create_run(format!("{worker}-{index}")))
            .unwrap();
    }
}
