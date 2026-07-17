# cv-sim replay-cost bench

The task log's CAS append replays the whole log before every write, so append cost
grows linearly with history and lifetime append cost grows quadratically. Measured,
not fixed (the fix is the snapshot+tail compaction plan in `task/store.rs`).

- machine: Apple M2 Max, 12 cores, macos/aarch64
- profile: release
- date: 2026-07-17
- method: synthetic fleet (`FleetScenario`, seed 20260716), log written directly at
  each size; replay is best of 3 full `TaskStore::replay` calls; append is the mean
  of 5 `append_agent_event` calls (each one replay + validate + one write).

| events | log size | full replay | single CAS append |
|-------:|---------:|------------:|------------------:|
| 1000 | 271 KiB | 1.4 ms | 2.7 ms |
| 5000 | 1.3 MiB | 7.1 ms | 13.5 ms |
| 20000 | 5.3 MiB | 53.6 ms | 75.1 ms |
| 50000 | 13.2 MiB | 178.7 ms | 183.2 ms |

Append ≈ replay at every size: the log walk dominates, the write is constant. Doubling history doubles every future append.
