# RBE branch audit — 2026-10-15

Audited all **39 remote heads** (main plus 38 older feature branches) against [`main` at f8cf7d64](https://github.com/Kate-alt-69/RBE/commit/f8cf7d644b9c1e226072eb0237f8a23a8c51990c), before publishing the new Task OID fix. Last activity below means the tip commit date, not the branch creation date. All 38 older feature tips are from September 25–October 3.

**Result:** 30 branches have their feature work incorporated into main (six are ancestors, four are entirely patch-equivalent, and 20 retain only staging/formatting/history differences after their implementation landed). Five are temporary tooling or a superseded TLS design. Two contain unmerged source-preparation prototypes. One contains a definite unlanded feature. No branches were merged or deleted during the audit.

## Work still needing attention

1. **FLD-033 credential-header singletons:** `ci/fld-033-auth-singletons-20260928` contains a Python staging patch and finalizer workflow, not a completed source change on main. Its intended behavior is to reject duplicate `Authorization` and `Proxy-Authorization` headers rather than comma-join credentials. On the audited main, `engine/crates/route-engine/src/discovery.rs:601` still joins every repeated text header and has no corresponding credential singleton tests. The [last finalizer run](https://github.com/Kate-alt-69/RBE/actions/runs/36395542204) failed at “Format and check route-engine.” Reapply the intended small change against current main and validate it; do not merge the stale workflow branch wholesale.
2. **Managed package builds:** the two LIB-022 source-preparation prototypes contain APIs that never landed under those names. Current `install-runtime/src/build_plan.rs` implements newer managed build planning, artifact re-verification, fresh extraction, and file digests through LIB-025/026. Keep these prototypes until their requirements are reconciled with that path. They do not complete the managed build executor: `backend install` still calls `prepare_prebuilt_activation_proofs`, which rejects packages declaring build steps. This is a real remaining RPX integration task, not evidence that all older branch code should be merged.
3. **TLS staging branch:** `rbe-lock-refresh-libhost-071` proposes STARTTLS as a `net:tcp` operation. Current main deliberately exposes a separate `net:tls` grant and broker, including `start_tls`, retained destination identity, certificate checks, and session ownership. Preserve the current privilege boundary; audit any remaining requirements rather than restoring the older API.

## Every older remote branch

“Incorporated” is about feature content, not a claim that every staging commit was merged or that the entire product is finished. Historical-file evidence means the branch’s changed implementation files have exact versions in main’s history; a few exports/snapshot files have subsequently evolved. All ordinary field feature implementations through FLD-032 are present in that history. FLD-028’s JSON helpers were later replaced by the stricter request JSON visitor.

| Branch | Last tip date | Assessment | Evidence |
| --- | --- | --- | --- |
| `ci/cli-004-promote-verified-graph-20260925` | 2026-09-25 | Incorporated | Every branch-only commit has a patch-equivalent commit on main. |
| `ci/fld-008-safe-integers-20260925` | 2026-09-25 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-016-cookie-snapshot-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-017-explicit-reject-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-018-explicit-null-body-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-019-query-identity-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-020-strict-query-encoding-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-021-cookie-identity-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-022-header-snapshot-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-023-strict-body-utf8-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-024-singleton-headers-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-025-forwarded-lines-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-026-land-20260927` | 2026-09-26 | Incorporated | Tip is an ancestor of main. |
| `ci/fld-026-proxy-protocol-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-027-forwarded-ip-20260927` | 2026-09-27 | Incorporated | Every branch-only commit has a patch-equivalent commit on main. |
| `ci/fld-028-json-number-identity-20260927` | 2026-09-27 | Incorporated | Every branch-only commit has a patch-equivalent commit on main. |
| `ci/fld-029-strict-json-objects-20260927` | 2026-09-27 | Incorporated | Every branch-only commit has a patch-equivalent commit on main. |
| `ci/fld-030-content-length-20260928` | 2026-09-28 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-031-user-agent-20260928` | 2026-09-28 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-032-host-authority-20260928` | 2026-09-28 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/fld-033-auth-singletons-20260928` | 2026-09-28 | Unlanded feature | Staged credential-header rejection patch; failed finalizer; absent from main. |
| `ci/lib-022-install-target-20260926` | 2026-09-26 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/lib-022-rebased-20260926` | 2026-09-26 | Unmerged prototype | Earlier graph/session preparation API; compare with current LIB-025/026 materialization before salvaging. |
| `ci/lib-022-source-preparation-20260925` | 2026-09-25 | Unmerged prototype | Earlier single-artifact preparation API; compare with current LIB-025/026 materialization before salvaging. |
| `ci/lib-023-durable-activation-20260927` | 2026-09-27 | Incorporated | Implementation files appear in main history; remaining commits are staging or alternate formatting. |
| `ci/lib-024-prebuilt-activation-20260927` | 2026-09-27 | Incorporated | Prebuilt activation landed as b5386269; current proof preparation and CLI activation are present. |
| `ci/lib-025-managed-build-plan-20260928` | 2026-09-28 | Incorporated | Managed planning landed as 5d07aa8e; current build_plan and snapshot integration are present. |
| `ci/lib-026-source-materialization-20260928` | 2026-09-28 | Incorporated | Source materialization landed as 5335beaa; exact implementation file appears in main history. |
| `ci/lib-027-source-identity-20260928` | 2026-09-28 | Incorporated | Tip is an ancestor of main. |
| `noop-check-mail` | 2026-09-30 | Incorporated | Tip is an ancestor of main. |
| `rbe-lock-refresh-libhost-071` | 2026-10-01 | Superseded design | Proposed STARTTLS inside net:tcp; main uses separate net:tls with explicit grants (bdd83971 and follow-ups). |
| `tmp-cn-baseline-repair` | 2026-10-02 | Temporary tooling | Ring dependency declaration is in main history; current locked release build succeeds. |
| `tmp-cn-lock-minimal` | 2026-10-02 | Temporary tooling | Ring declaration is in main history; stale lock-refresh workflow is not a feature. |
| `tmp-cn-lock-refresh` | 2026-10-02 | Temporary tooling | Old generated lock and refresh workflow; do not replace the current lock with this snapshot. |
| `tmp-cn222-validate` | 2026-10-02 | Temporary tooling | Validation marker only; no runtime implementation to recover. |
| `tmp-fld018-graft-20260926` | 2026-09-26 | Incorporated | Tip is an ancestor of main. |
| `tmp-ignore` | 2026-10-03 | Incorporated | Tip is an ancestor of main. |
| `tmp-svc-native-519-stage` | 2026-10-03 | Incorporated | Tip is an ancestor of main. |

## Local branches and pull requests

Local `codex/fix-cti-dynamic-routes`, `codex/ka-desktop-and-account`, and `work` are ancestors of the audited main. The current `codex/fix-task-oid-bindings` is new work from this session and is intentionally pending review, not an abandoned older branch. All three pre-existing RBE PRs (#1 Service/Video Manager, #2 desktop KA/auth plan, #3 dynamic route CTI identities) are merged; there was no older open PR at audit time.

## Method and limits

Fetched every remote head, compared ancestry and ahead/behind counts, checked patch equivalence with `git cherry`, inspected changed implementation blobs against main history, and reviewed divergent preparation/TLS code plus the FLD-033 staging patch and workflow result. Temporary workflows were read as data and were not executed. The audit checks repository evidence; it cannot reconstruct requirements that existed only in unavailable earlier chat messages. No old branches were individually compiled, and incorporated features are not certified production-ready by this historical audit.

Cleanup candidates are the 30 incorporated branches plus the five temporary/superseded branches, after normal owner review. Retain the two preparation prototypes until reconciliation and FLD-033 until its source change is implemented and validated.
