//! Host `reserve` 在单次 drive 内存里按 `Run.budget` 记账。
//!
//! **作用**：用 Fake Engine 显式调用 `reserve`，不接模型、不接 Loop。
//! **边界**：已用量不入库。`BudgetExhausted` 同时表示额度不够与非 Running。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;

use async_trait::async_trait;
use fluxaxiom_contract::{Budget, RunStatus};
use fluxaxiom_engine_api::{
    ArtifactDraft, BudgetExhausted, BudgetLease, BudgetRequest, EngineAdapter, EngineDescriptor,
    EngineOutcome, EngineRunRequest, FailureReason, HostPort,
};
use fluxaxiom_run_host::RunHost;

mod common;

/// 在 Running 期间按脚本调用 `reserve`，并把结果记下来。
struct BudgetEngine {
    steps: Vec<BudgetRequest>,
    leases: Arc<std::sync::Mutex<Vec<Result<BudgetLease, BudgetExhausted>>>>,
}

#[async_trait]
impl EngineAdapter for BudgetEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    /// **作用**：按 `steps` 顺序调用 `reserve`，再交回 Completed。
    /// **边界**：不写 blob，不 observe。失败的 `reserve` 不中断后续步骤，方便断言余额是否被改过。
    async fn run(&self, _request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        let mut leases = self.leases.lock().unwrap();
        for step in &self.steps {
            leases.push(host.reserve(*step));
        }
        EngineOutcome::Completed {
            report: ArtifactDraft::report("budget probe".to_owned()),
        }
    }
}

/// 多线程同时 `reserve({1,0})`，用来证明不会超发。
struct ConcurrentBudgetEngine {
    workers: u32,
    successes: Arc<AtomicU32>,
}

#[async_trait]
impl EngineAdapter for ConcurrentBudgetEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    /// **作用**：开 `workers` 个线程，每个尝试预留一次模型调用。
    /// **边界**：成功次数记入原子计数。收尾仍是 Completed，不把耗尽当成 Run 失败（那是 Loop 的事）。
    async fn run(&self, _request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        let successes = Arc::clone(&self.successes);
        let mut handles = Vec::new();
        for _ in 0..self.workers {
            let host = Arc::clone(&host);
            let successes = Arc::clone(&successes);
            handles.push(thread::spawn(move || {
                if host
                    .reserve(BudgetRequest {
                        model_calls: 1,
                        tool_calls: 0,
                    })
                    .is_ok()
                {
                    successes.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        EngineOutcome::Completed {
            report: ArtifactDraft::report("concurrent budget".to_owned()),
        }
    }
}

fn budgeted_run(
    workspace: &std::path::Path,
    model: u32,
    tool: u32,
) -> fluxaxiom_run_host::CreateRun {
    let mut request = common::create_run("req_budget", workspace);
    request.budget = Budget {
        max_model_calls: model,
        max_tool_calls: tool,
    };
    request
}

/// 恰好用完上限后，下一次同型请求失败；`{0,0}` 始终成功且 Lease 相等。
#[tokio::test]
async fn reserve_exhausts_exactly_and_zero_request_is_free() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host.create_run(budgeted_run(&workspace, 2, 1)).unwrap();
    let leases = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = BudgetEngine {
        steps: vec![
            BudgetRequest {
                model_calls: 0,
                tool_calls: 0,
            },
            BudgetRequest {
                model_calls: 1,
                tool_calls: 0,
            },
            BudgetRequest {
                model_calls: 1,
                tool_calls: 0,
            },
            BudgetRequest {
                model_calls: 1,
                tool_calls: 0,
            },
            BudgetRequest {
                model_calls: 0,
                tool_calls: 0,
            },
            BudgetRequest {
                model_calls: 0,
                tool_calls: 1,
            },
            BudgetRequest {
                model_calls: 0,
                tool_calls: 1,
            },
        ],
        leases: Arc::clone(&leases),
    };
    host.drive_run(&created.run.run_id, &engine).await.unwrap();
    let leases = leases.lock().unwrap();
    assert_eq!(
        leases[0],
        Ok(BudgetLease {
            model_calls: 0,
            tool_calls: 0
        })
    );
    assert_eq!(
        leases[1],
        Ok(BudgetLease {
            model_calls: 1,
            tool_calls: 0
        })
    );
    assert_eq!(
        leases[2],
        Ok(BudgetLease {
            model_calls: 1,
            tool_calls: 0
        })
    );
    assert_eq!(leases[3], Err(BudgetExhausted));
    assert_eq!(
        leases[4],
        Ok(BudgetLease {
            model_calls: 0,
            tool_calls: 0
        })
    );
    assert_eq!(
        leases[5],
        Ok(BudgetLease {
            model_calls: 0,
            tool_calls: 1
        })
    );
    assert_eq!(leases[6], Err(BudgetExhausted));
}

/// 只有第二个维度不够的申请失败后，第一个维度也没有被扣。
///
/// **边界**：余额 `{2,3}`。先用掉 `{1,2}` 剩 `{1,1}`；`{1,2}` 的 model 够、tool 不够，必须整笔失败；
/// 随后 `{1,1}` 仍应成功。若实现先扣 model 再查 tool，最后一步会因 model 已被扣光而失败。
#[tokio::test]
async fn oversized_reserve_does_not_change_remaining() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let mut request = budgeted_run(&workspace, 2, 3);
    request.request_id = "req_atomic".to_owned();
    let created = host.create_run(request).unwrap();
    let leases = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = BudgetEngine {
        steps: vec![
            BudgetRequest {
                model_calls: 1,
                tool_calls: 2,
            },
            BudgetRequest {
                model_calls: 1,
                tool_calls: 2,
            },
            BudgetRequest {
                model_calls: 1,
                tool_calls: 1,
            },
        ],
        leases: Arc::clone(&leases),
    };
    host.drive_run(&created.run.run_id, &engine).await.unwrap();
    let leases = leases.lock().unwrap();
    assert_eq!(
        leases[0],
        Ok(BudgetLease {
            model_calls: 1,
            tool_calls: 2
        })
    );
    assert_eq!(leases[1], Err(BudgetExhausted));
    assert_eq!(
        leases[2],
        Ok(BudgetLease {
            model_calls: 1,
            tool_calls: 1
        })
    );
}

/// 并发预留不会超过 `max_model_calls`。
#[tokio::test]
async fn concurrent_reserves_do_not_over_issue() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let mut request = budgeted_run(&workspace, 10, 0);
    request.request_id = "req_race_budget".to_owned();
    let created = host.create_run(request).unwrap();
    let successes = Arc::new(AtomicU32::new(0));
    let engine = ConcurrentBudgetEngine {
        workers: 40,
        successes: Arc::clone(&successes),
    };
    host.drive_run(&created.run.run_id, &engine).await.unwrap();
    assert_eq!(successes.load(Ordering::SeqCst), 10);
}

/// 不同 Run 的计数互相独立：第一个 Run 用完额度，不影响第二个 Run。
///
/// **边界**：这里不覆盖「同一个 Run 再次 drive」。今天 `Accepted` 才能 drive，所以那条路径走不到；
/// A3 允许续跑时计数会重置，见执行方案 §4.7.4 第 3 项。
#[tokio::test]
async fn separate_runs_have_independent_budgets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();

    let mut first = budgeted_run(&workspace, 1, 0);
    first.request_id = "req_first".to_owned();
    let created_first = host.create_run(first).unwrap();
    let leases_first = Arc::new(std::sync::Mutex::new(Vec::new()));
    host.drive_run(
        &created_first.run.run_id,
        &BudgetEngine {
            steps: vec![
                BudgetRequest {
                    model_calls: 1,
                    tool_calls: 0,
                },
                BudgetRequest {
                    model_calls: 1,
                    tool_calls: 0,
                },
            ],
            leases: Arc::clone(&leases_first),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        leases_first.lock().unwrap()[0],
        Ok(BudgetLease {
            model_calls: 1,
            tool_calls: 0
        })
    );
    assert_eq!(leases_first.lock().unwrap()[1], Err(BudgetExhausted));
    assert_eq!(
        host.get_run(&created_first.run.run_id).unwrap().status,
        RunStatus::Completed
    );

    let mut second = budgeted_run(&workspace, 1, 0);
    second.request_id = "req_second".to_owned();
    let created_second = host.create_run(second).unwrap();
    let leases_second = Arc::new(std::sync::Mutex::new(Vec::new()));
    host.drive_run(
        &created_second.run.run_id,
        &BudgetEngine {
            steps: vec![BudgetRequest {
                model_calls: 1,
                tool_calls: 0,
            }],
            leases: Arc::clone(&leases_second),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        leases_second.lock().unwrap()[0],
        Ok(BudgetLease {
            model_calls: 1,
            tool_calls: 0
        })
    );
}

/// 预算耗尽时 Engine 仍可自行交回 `Failed(budget_exhausted)`；Host 写入对应终态。
#[tokio::test]
async fn engine_can_report_budget_exhausted_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let mut request = budgeted_run(&workspace, 0, 0);
    request.request_id = "req_failed_budget".to_owned();
    let created = host.create_run(request).unwrap();

    struct ExhaustedEngine;
    #[async_trait]
    impl EngineAdapter for ExhaustedEngine {
        fn descriptor(&self) -> EngineDescriptor {
            EngineDescriptor {
                engine_id: common::FAKE_ENGINE_ID.to_owned(),
                version: common::FAKE_ENGINE_VERSION.to_owned(),
            }
        }

        async fn run(&self, _request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
            let zero = host
                .reserve(BudgetRequest {
                    model_calls: 0,
                    tool_calls: 0,
                })
                .unwrap();
            assert_eq!(
                zero,
                BudgetLease {
                    model_calls: 0,
                    tool_calls: 0
                }
            );
            assert_eq!(
                host.reserve(BudgetRequest {
                    model_calls: 1,
                    tool_calls: 0,
                }),
                Err(BudgetExhausted)
            );
            EngineOutcome::Failed {
                reason: FailureReason::BudgetExhausted,
                partial: Some(ArtifactDraft::report("stopped early".to_owned())),
            }
        }
    }

    let outcome = host
        .drive_run(&created.run.run_id, &ExhaustedEngine)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        EngineOutcome::Failed {
            reason: FailureReason::BudgetExhausted,
            ..
        }
    ));
    let run = host.get_run(&created.run.run_id).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        run.terminal_reason,
        Some(fluxaxiom_contract::TerminalReason::BudgetExhausted)
    );
}
