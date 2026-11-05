# FluxAxiom

[中文](README.zh-CN.md)

A personal agent system. The current stage is P0a: a read-only repository-analysis CLI (`flux`).

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The default tests use a fake provider and do not touch the network.

## Run

The default fake engine does not dial out. It checks the CLI, the host, and the event path:

```bash
cargo run -p fluxaxiom-cli -- run --workspace <directory> --prompt "analyze this repository"
```

### Use a real model (DeepSeek)

1. Copy the template and fill in the key:

   ```bash
   cp .env.example .env     # Windows: copy .env.example .env
   ```

   Put `DEEPSEEK_API_KEY=sk-...` in `.env`. `.env` is listed in `.gitignore`. Do not commit it.

2. Run with `--engine deepseek`:

   ```bash
   cargo run -p fluxaxiom-cli -- run --engine deepseek --workspace <directory> --prompt "analyze this repository"
   ```

On startup, `flux` walks upward from the working directory and loads the first `.env`. An already-set environment variable wins over `.env`. A missing key fails before a run is created, so it does not leave an orphan run.

**Data leaves the machine.** `--engine deepseek` sends the authorized workspace to DeepSeek's cloud. Do not point it at a sensitive repository (ADR-020).

Two live API tests are ignored by default. They read the process environment only. They do not read `.env`. With no key they return immediately and the test is reported as passed, without dialing. Set the key in the current shell first:

```bash
# PowerShell: $env:DEEPSEEK_API_KEY = "sk-..."
export DEEPSEEK_API_KEY=sk-...
cargo test -p fluxaxiom-engine-custom --test sse -- --ignored
```

## Commands

| Command | What it does |
|---|---|
| `flux run` | Create a run, drive the engine, and print committed events as they land. Prints the report at the end. Exit code 0 only when the run is `Completed` |
| `flux status <run_id>` | Print the run snapshot |
| `flux events <run_id> [--after-sequence N] [--follow]` | Print durable events. `--follow` polls until the run is terminal |
| `flux artifacts <run_id> [--show [artifact_id]]` | List artifact summaries. `--show` prints the body |
| `flux trace <run_id> [--json] [--verify] [--show-request N] [--show-response N] [--include-reasoning]` | Show the model-call timeline. `--verify` replays the request from the context record and compares it with the `messages` that were sent |
| `flux sample export <run_id> [--out FILE]` | Export a sample JSON of references and summaries, including the rebuild verdict and feedback |
| `flux sample feedback <run_id> --verdict usable\|needs-edit\|redo [--note TEXT]` | Append a human verdict for one run. Check `--help` for the live flags |
| `flux ctx replay <run_id> --call N [--samples 5] [--strategy identity@1] [--engine fake\|deepseek]` | Resample saved model call N and print next-step agreement. Does not write a run. Only `identity@1` is accepted |

Main `run` flags: `--workspace`, `--prompt`, `--request-id` (idempotency key), `--engine fake|deepseek`, `--max-model-calls` (default 8), `--max-tool-calls` (default 32).

Ctrl-C cancels the current run (`Cancelled`). A second Ctrl-C force-exits. After a crash, an attempt whose lease has expired is marked `Orphaned` on the next startup, and the run becomes `Failed(host_restart)`.

### Data directory

In order: `--data-dir`, then `FLUXAXIOM_DATA_DIR`, then the platform default (`%LOCALAPPDATA%\FluxAxiom` on Windows, otherwise `$XDG_DATA_HOME/fluxaxiom` or `~/.local/share/fluxaxiom`).

## Layout

| Path | Contents |
|---|---|
| `apps/cli` | The `flux` binary. It is the composition root |
| `crates/agent-contract` | Run, attempt, event, and artifact contract types. Schemas live in `contracts/` |
| `crates/engine-api` | The narrow engine/host port and `WorkspaceRoot` |
| `crates/engine-custom` | Engine A: DeepSeek client, loop, read-only tools, and request-chain verification |
| `crates/context` | Context module: model profile, estimator, L0–L2 strategies, context records, and `replay`. It does not depend on the engine or the host |
| `crates/run-host` | SQLite store, events, budget, lease, and cancellation |
| `docs/research/context/` | EA-CTX measurement records. They store method, counts, and usage only. No keys, full prompts, reasoning text, or tool bodies |
| `docs/` | Architecture notes, stage plans, and ADRs |

## Status and route (2026-11-05)

`main@2aed418` already had P0a A0–A4b and the A5 read-only security regressions. **G-A was frozen on 2026-10-29.** The five real use days and at least ten real tasks were not started. Formal acceptance is unfinished and must not be written as passed. That unfinished item no longer blocks Engine A. The current record is the [G-A acceptance report](docs/development/G-A验收报告.md).

**Engine B and the Codex app-server comparison are stopped.** The route deepens the single in-house Engine A (EA-01 through EA-04 and EA-G), then checks the real Agent Memory Engine API (M0), adds read-only recall that is off by default (M1), and runs a memory comparison (M-EVAL). Memory write-back (M2/M3) waits for a separate authorization plus idempotency and reconciliation evidence. While G-A is frozen, memory must not rewrite the no-memory baseline. The B3/B4/B6-A safety-effect line serves Engine A only and does not start because of this freeze. M1 does not need B3. Memory is not wired in.

- [ADR-025: Engine A and the independent memory boundary](docs/adr/ADR-025-engine-a-first-memory-integration.md)
- [ADR-026: the context module](docs/adr/ADR-026-context-module.md)
- [Engine A and Agent Memory Engine plan](docs/development/EngineA与AgentMemoryEngine演进方案.md)
- [P0 stage plan v1.1](docs/development/P0阶段执行方案%20v1.1.md)
- [EA-CTX execution plan](docs/development/上下文压缩专题执行方案（EA-CTX）.md)

## Context compression (EA-CTX, paused on 2026-11-05)

Context management is its own module. Before every model call, the engine calls `ContextManager::prepare` and receives the view to send plus one context record. The verifier calls `ContextManager::replay` and compares that view with the request that was sent. Facts are append-only: the system prompt, user text, assistant messages, and tool-result blobs are not rewritten or deleted because of compression. Edits live only in the context record. Events store `context_ref` and `context_strategy`.

The module does not read the call budget, open the network, open blobs, or depend on `engine-custom` or `run-host`.

### Preparing one request

`prepare` runs in this order:

1. **L0 forward dedup**, on every call. When a new tool result is identical to an earlier one in the same run (same tool, same canonical arguments, same result reference), the new body becomes a pointer stub such as `[unchanged: same result as round #3 call 1]`. Older bodies stay as they are.
2. **Estimate.** With a complete usage record, the estimate is the previous round's `prompt_tokens + completion_tokens`, plus tool text and user text that appeared after that response. A known `prompt_tokens` with a missing completion is only a lower bound. The missing completion is not filled in as 0. With no usage, countable characters are estimated in full. Profile v2 counts Han characters, ASCII letters, and ASCII digits. Whitespace and punctuation are not counted. `estimate_error` remains the uncalibrated 0.30.
3. **L1 supersede.** This is attempted only after the estimate passes the uncalibrated soft trigger of 128K. When a later `read_file` of the same file covers an earlier read, the earlier body becomes `[superseded: ...]`. Tool results from the last two model rounds stay intact. Below the hard limit, the edit is kept only when it frees at least `max(32K, current estimate / 4)`. Above the hard limit, it is kept even when it frees less. Otherwise the whole L1 attempt is reverted.
4. **L2 stale mask.** After L1 is kept or reverted, this is attempted only while the estimate is still above the 64K target. 64K is half the soft trigger and is also uncalibrated. Starting from the oldest tool result, original tool bodies outside the protection window are replaced with `[stale: tool-name arguments, size B, reference; call this tool again if needed]`, except the original body that this round's L0 pointer names. The keep-or-revert rule matches L1: the batch finishes, then the whole batch is kept or dropped. It does not stop in the middle at 64K.
5. **Hard limit.** If the estimate is still over the hard limit, or the hard limit is 0, that model request is not sent. The record is still saved. The run ends as `Failed(context_exhausted)` and may carry the partial report already produced. A failure while writing the record or the diagnostic is `Failed(engine_error)`.

These three layers do not rewrite `system`, `user`, or any assistant message (content, reasoning, or tool calls).

Strategy ids written by `prepare`:

| Layers kept | Strategy id |
| --- | --- |
| L0 only | `l0@1` |
| L0 + L1 | `l0l1@1` |
| L0 + L2 | `l0l2@1` |
| L0 + L1 + L2 | `l0l1l2@1` |

`identity@1` is for older events that have no context record. `l1@1` and `l2@1` exist so a single-layer record can be replayed. `prepare` does not write those two ids when it keeps a layer. The plan named the whole chain `mask@1`. This branch names the chain by the layers that were actually kept.

### Records and verification

A new `prepare` writes schema 3. `spec` holds the hard limit, soft trigger, minimum tokens freed, protection window, and target. `replay` accepts schemas 1, 2, and 3. It does not read those four derived values. It rebuilds the view from `edits` only. Schema 1 has only the hard limit. Schema 2 has no target. A record contains no prompt, tool body, or reasoning text.

`flux trace --verify` checks that the `messages` sent at the time equal the serialized `replay` result. The comparison is on JSON values. Key order and whitespace are not differences. A valid L2 stub is `consistent`. A tampered stub, a stale edit inside the last two rounds, a stubbed request with no matching edit, and an edit whose request is unchanged are all `inconsistent rule=1`. An unknown strategy or a record that cannot be replayed is `unverifiable`.

### What this round finished

| Stage | Result |
| --- | --- |
| P1 | A standalone `context` crate, the DeepSeek Flash profile, the estimator, the hard limit, `context_exhausted`, context records, and the engine, host, and `trace` wiring |
| Measurement round | T0–T11 each have a record. Fields without a record stay unknown. T6 found no quality drop through 512K, so `effective_context` has no number and the soft trigger stays at 128K |
| P2 | One offline corpus count (T1) and `flux ctx replay` |
| P3 mechanism | L0, L1, and L2 run inside `prepare`. Rebuild tests for `l0l2@1` and `l0l1l2@1` cover a valid stub and the four rule-1 failures |
| P3 measurement | 2026-11-05, the fifth model call of task `mini-inventory-k5`. Five tool results outside the protection window, 873 bytes, were replaced by stubs. All five samples made the same next step as the unfilled baseline. Decision agreement is 1 and the loss is 0. `prompt_tokens` fell from 2964 to 2907 |

Cache usage from that measurement:

| Phase | prompt | hit | miss |
| --- | ---: | ---: | ---: |
| Before the edit, both calls | 2964 | 2816 | 148 |
| First call after the mask | 2907 | 384 | 2523 |
| The same masked request sent again | 2907 | 2688 | 219 |

The record is [P3-mask-decision-2026-11-05.md](docs/research/context/P3-mask-decision-2026-11-05.md).

### Still unfinished at this pause

These items stay unfinished. They are not a pass:

- There is no general number for the decision-equivalence threshold. The request above is about 2964 tokens, under 128K, so `prepare` would not run L2. The bytes replaced were an earlier 873 bytes. The larger tool results in the last two rounds were still present.
- The cache numbers come from that one edit. They are not a cache policy.
- There is still no real provider error sample that identifies a context-limit response. An unrecognized response stays `ProviderError`. The chain is not forced to run once more and retry.
- P4 is not in progress. The soft trigger stays at the uncalibrated 128K.
- P5 is not started. Old reasoning is not cleared. There is no summary layer, no fold, and no second model profile.
- G-A remains the freeze from 2026-10-29. Memory is still not connected, and it does not rewrite this no-memory baseline.

A later measurement on a longer task is stored as its own record. It does not replace the 2026-11-05 conclusion.
