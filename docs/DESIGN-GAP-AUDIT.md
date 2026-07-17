# Design-gap audit: cv task substrate + phoenix herdr vs mission-control lessons

*Produced 2026-07-16 by a 5-lane mine + adversarial-audit + synthesis workflow over
mission-control docs, its wake/comms canon, 574 fix-commits, the deleted engine
comments, and the product-vision docs. Each lesson was tested against the actual
cv/phoenix code with verdicts ignored/partial/honored/deliberately-rejected.*

# Design-Gap Audit Synthesis: cv task substrate + phoenix herdr

## 1. TOP GAPS (ignored / partially honored, deduplicated, ranked)

**G1. The verifier has no liveness signal — a dead or unarmed verifier is indistinguishable from truthfully-unlanded work.**
Sources: comms-canon ("Silence is never evidence of health"), fix-history ("Authored, tested — wired to nothing"), engine-wisdom ("A silently-dead automation reads as health"). Three independent lanes converged on this; it is the strongest signal in the audit. `cvd watch --verify-interval` defaults to 0 (off), persists no last-run marker, and only the verifier can emit MergedLocal/Landed. Fail direction is safe (landed looks unlanded, never the reverse) but the owner's primary surface — the debt view — becomes untrustworthy, silently.
*Recommendation (small):* `run_verify` writes `tasks/last_verify.json` (timestamp + interval); `cv task debt`, `/api/tasks/debt`, and the cockpit render "verified as of X / NEVER", warning when age > 2x interval. Make verify-interval default nonzero under `cvd watch`. ~50 lines, no new machinery.

**G2. Replay bricks on interior reducer-refused events, and one rejected append wedges the whole verify pass.**
Sources: fix-history ("The durable log is an ABI", "Gating the tree proves the tree", "Anomaly detection must re-verify at fire time"). Three related holes in one seam: (a) an interior line that parses but the reducer refuses makes `replay()` return Err, bricking CLI, cvd, and all appends at once; (b) the CAS append discards read warnings (store.rs:113) and will write against an incomplete model when a stale binary reads a newer log; (c) `run_verify` uses `?` on `append_verifier_event` (verify.rs:340), so one persistently-invalid append aborts verification for every remaining task on every tick.
*Recommendation:* read paths quarantine reducer-refused events (warn + counter + continue); the append path stays strict and refuses when the read produced any warning; `run_verify` converts a rejected append into a per-task warning and continues; stamp the log with a format version. All within the existing store, no new components.

**G3. A forged Landed sticks forever — Landed revisions are never re-verified.**
Sources: design-docs ("Trace forgeability before arming"), fix-history ("Identity must be attested by the kernel" — the residue the rejection doesn't cover). Any seat with Bash can echo a self-consistent Landed line into events.jsonl (copying the revision's own recorded patch_id), and `landed_revision_is_not_reverified` guarantees the verifier never contradicts it — permanently hiding unlanded work from the debt view. MC history shows agents under pressure take exactly this shortcut.
*Recommendation:* the periodic pass occasionally re-observes Landed revisions with `content_is_on_upstream` (cheap, monotone-true for genuine lands, so a forgery is contradicted on the next tick and genuine lands cost one merge-base each); warn on replay when a verifier-only event's `by` is not `cv-verify`. This is law 1 applied to itself, not authority machinery.

**G4. The bare command records the wrong identity — `--from` defaults to the literal strings 'cv' / 'agent'.**
Source: comms-canon ("Make the bare command correct"). Inbox/assignee/reviewer semantics key on exact endpoint strings; a forgotten flag silently detaches an agent from its own inbox and misattributes the durable record verbatim, forever. Fleets provably fumble flags at high rates.
*Recommendation:* phoenix sets `CV_ENDPOINT=agent:<pane>` at spawn (it already owns spawn); cv and cv-mcp derive the default from it, `--from` becomes an override; identity-bearing verbs (claim/propose/pass/refute) with no derived identity **require** an explicit `--from` instead of defaulting to a shared sink. Environment convention, zero attestation.

**G5. No reader sanitizes — one ESC sequence in a task title corrupts every terminal that ever runs `cv task list`, eternally.**
Source: comms-canon ("Every consumer sanitizes untrusted message text itself"). The log is replayed forever, so this is not curable at the send path; the cockpit's safety is an accident of ratatui, not policy; cvd hands raw strings to foreign clients.
*Recommendation:* one strip-control-chars helper in cv-core (preserve `\n\t` where structural), applied unconditionally at every render site — CLI print paths, cockpit rendering, optionally flagged in cvd JSON. One function, N call sites.

**G6. Host-pressure containment was deleted and replaced with nothing.**
Source: design-docs ("Contain host pressure structurally, not by agent politeness"). Architecture-independent failure with live precedent (hbox physically power-cycled 2026-07-15); swarm-build is per-user convention on one host — exactly the politeness the lesson forbids.
*Recommendation:* not cv's domain — make the cgroup-wrapped build command the default in phoenix-spawned agent environments (spawn-path env/convention), and a minimal reboot-durable watchdog (launchd/systemd timer) that flags panes present in herdr but absent from board heartbeats past a threshold. Structural, outside the substrate, keeps law 4.

**G7. An idle agent with a non-empty inbox is woken by nothing — review latency degrades to human-attention latency.**
Sources: comms-canon ("Wake only when BOTH conditions hold"), engine-wisdom ("Idle capacity needs a level trigger"), design-docs ("Wake-on-idle" residue). Deliberate trade of the strip, but all three lanes note the throughput bill returns at fleet scale.
*Recommendation (two tiers, both small):* tier 1 — the retained session hook runs `cv task inbox <who>` at turn boundary/session start, so agents self-feed from the level with zero daemon machinery. Tier 2 (optional) — one-line nudge through the existing `agent.send` quiescence gate on inbox empty→non-empty while pane Idle, re-armed on empty; phoenix already built all three gates the canon demands.

**G8. Stuck states age invisibly — no surface renders age for claimed tasks or awaiting-review revisions.**
Sources: engine-wisdom ("truthful wait posture", "zombie-holder", "deadline-less urgency"), fix-history ("walk every obligation's lifecycle"), design-docs (AwaitingReview aging residue). Human eyes are now the entire escalation mechanism, but a 3-day-old claim renders identically to a 5-minute one, and AwaitingReview appears in neither the debt view nor any aged surface — a dead reviewer is honest state that nobody sees.
*Recommendation:* age column on list/inbox/cockpit rows (`opened_at`/`last_ts` already in TaskProjection); extend the debt view (or a sibling projection) with aged AwaitingReview entries. Pure projection work.

**G9. Resume argv is reconstructed from minimal templates, dropping flags the seat depended on.**
Source: design-docs ("Replay-what-ran"). `plan()` (agent_resume.rs:142-225) rebuilds `claude --resume <id>` from hardcoded templates; a seat launched with `--permission-mode`/`--allowedTools`/`--model` resumes without them — a permission-mode drop strands a headless seat on prompts. Recurs on every restart phoenix supports.
*Recommendation:* record observed spawn argv at detection time (process cmdline / pane's original command), build resume plans by upserting the resume flag into that recorded argv.

**G10. Fossils actively mislead: the server-stop fear gate cites deleted machinery, and phoenix docs describe the deleted MC system verbatim.**
Sources: product-surface ("restart scary", "owner has no manual"). `fleet_reboot_gate` demands `MC_ALLOW_FLEET_REBOOT=1` citing a guard hook and choreography the strip deleted — retraining the owner not to touch restart, the exact named failure. docs/ONBOARDING.md et al. describe comms/hooks/attestation that no longer exist; the mdbook has no tasks chapter.
*Recommendation:* rewrite server stop to persist-sessions-and-stop with a plain confirm; delete or rewrite docs/{ONBOARDING,WARROOM,CODEX_LOOP_MAP,AFK_AWAY}.md; add manual/src/tasks.md plus a one-page owner runbook (dispatch, glance, unblock, restart). Mostly deletion.

**G11 (minor). cv/cvd binaries carry no build commit.**
Sources: design-docs x2 ("deployed config is not loaded config", "binaries checkable against source"). Long-running cvd swapped often during development; "is the running verifier the new verifier" is timestamp inference. *Recommendation:* build.rs embedding git commit into `--version` and cvd's startup log — phoenix already has the pattern in build_info.rs. An hour of work; do it alongside G1.

## 2. FALSE ALARMS (flagged by the lens, dissolved by the architecture)

- **Kernel-attested identity as authority** (design-docs): identity strings mint nothing — landing authority is literal push access and landed-ness is observed; the legibility residue is what G3/G4 cover.
- **Away posture level** (comms-canon): no scheduled wakes or automation can mistake an empty chair for presence; if unattended runs return, a board status level is a zero-machinery convention.
- **Per-recipient wake coverage classification** (engine-wisdom): the system makes no delivery promises to misclassify — everyone is uniformly, honestly "uncovered, expected to poll".
- **Sticky declines / re-nag suppression** (engine-wisdom): no offer automation exists to decline.
- **Duplicate-watcher work stealing** (fix-history): the cockpit is a stateless poller; polling is idempotent broadcast, not a work queue.
- **Wake coalescing / notification stacking** (comms-canon): nothing interrupts anyone; N events during a turn produce zero interrupts.
- **Fleet-roll canary/halt ordering** (design-docs x2): no destructive fleet-cycle operation exists to sequence or halt.
- **Per-item rescan time bomb** (fix-history): the shape exists but the fuse is years long — task-lifecycle events only, not comms chatter. Watch it; the mtime+len replay cache is a rider on future cvd work, not an M3 item.

## 3. DELIBERATE REJECTIONS to document

1. **No attestation, receipts, or kernel identity** (law 3): trust relocated from attested claims to observed git state (law 1). Residue accepted: `by` is free text (mitigated by G3 re-verify + G4 env derivation, neither of which adds authority).
2. **No wake/delivery/interrupt machinery**: pull-only, board never read back for state. Cost accepted: idle-agent latency (mitigated by G7 tier-1 hook, which is a convention, not an engine).
3. **No captain/seats/grants/escalation**: anyone can Release, Reroute, Abandon — audited events with the real actor in `by`. "Anyone, audited" replaces "designated authority".
4. **No away/afk routing**: return-facing surface is durable state (cockpit, debt, inbox), never pushed alerts.
5. **Reviewer independence advisory-only**: warns, never gates; measured from transcripts at verdict time.
6. **Hash-chain/unforgeable event ids dropped** (reduce.rs deviation 1): compensated observationally by G3 rather than cryptographically.

## 4. M3 PRIORITY LIST (ordered by value to the human fleet owner)

1. **Verifier heartbeat + default-on verify-interval; render "verified as of" on debt/cockpit** (G1 — restores trust in the owner's primary surface; three lanes demanded it).
2. **Store hardening: quarantine-don't-brick replay, append fails closed on read warnings, run_verify continues past rejected appends, log format version** (G2 — protects every surface at once).
3. **Periodic re-verification of Landed revisions + warn on non-verifier `by`** (G3 — makes forgery self-defeating; keeps laws 1 and 3).
4. **CV_ENDPOINT identity derivation at spawn; require explicit `--from` when underived** (G4 — correctness of the durable record the owner reads).
5. **Age rendering on all task surfaces + aged AwaitingReview in the debt view** (G8 — completes the visibility-instead-of-escalation bet).
6. **Control-char strip helper at every render site** (G5 — one poisoned title otherwise haunts every reader forever).
7. **Turn-boundary inbox pull via the retained session hook** (G7 — fleet self-feeds; biggest throughput win per line of code).
8. **Fossil purge + owner manual: rewrite server-stop gate, delete stale MC docs, add tasks manual chapter and one-page owner runbook** (G10 — cheap, and currently actively misleading).

Below the line (do opportunistically, not M3-gated): G6 containment (spawn-env swarm-build default + heartbeat watchdog — high value but lives outside the substrate), G9 resume-argv superset, G11 build-commit in `--version` (rider on item 1), branch/worktree collision warn at propose (design-docs), verifier issue dedup against last-N (fix-history), layer-naming paragraph beside the four laws (comms-canon), unanswered-board-requests projection and `agent spawn --task <id>` bridge (product-surface).
