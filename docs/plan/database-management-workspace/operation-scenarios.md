# Operation Scenarios

## Roles and authority
NOT APPLICABLE: single role — desktop_owner is the current Windows principal under existing Workspace access. No new human RBAC. backup_service is an existing OS actor, not a new user role. Connection execution mode does not grant DB privileges.
Denied-state contracts still apply: wrong owner/window/Workspace/epoch returns scoped error without resource data; readonly unsupported cells remain readonly with reason; elevated Workbench is disabled before user state creation; unknown outcome blocks reapply but permits inspection.

## Menu scenarios

### Database Workbench — connection entry
User selects existing Workspace/Target. Native integrity/window checks precede user-store creation. Service authorizes canonical descriptor/active ownership; user supplies separate interactive credential if absent. TLS/source identity mismatch refuses; no service-secret export/fallback. UI always displays actual target/database/role/purpose, not alias/color alone.

### Explorer and table preview
Expand lazy schemas/tables; search named scope; load bounded typed page; follow FK or open diagram of same metadata version. Unknown count/permission/no match/not loaded/stale/error are distinct. Keyless table is bounded readonly preview, not exhaustive pagination. Refresh/filter/order creates new result generation and clears incompatible selection/edit review.

### SQL editor and transaction bar
Open target-bound tab, type/choose selection, explicitly run exact SQL. Explain defaults plan-only; Analyze is explicit executing action. Effective mode/isolation/limits visible. Script boundaries are proven and sequential; unseparable block is one unit. Successful earlier statements remain visible after later error. Manual transaction stays on one connection; closing a buffer does not commit/rollback it.

### Results and row details
Scroll row+column virtual viewport; cache evicts bounded pages but focus/selection remains logical. Exact copy differs from formatted copy. NULL/empty/u64/decimal/binary/timezone and truncated/unsupported are explicit. Slow consumer displays waiting/cancel requested; duplicate batch does not append twice. Capped/partial/interrupted is not complete or snapshot.

### Edit review and conflict
Create memory-only typed draft on certified base row. Review source identity/original/draft/types/parameters. Apply revalidates authority/schema, checks original under lock, executes generated bound write in standalone transaction. Conflict preserves draft and supported rollback evidence; Refresh basis requires a new review, never auto-apply. Unknown COMMIT remains durable; operator acknowledgment is separate from proof and no automatic retry is offered.

### Import/export
Review source/target and typed mapping. Import streams child batches; each declares atomic-per-batch and records range/digest/outcome. Unknown batch halts later work; previous commits remain visible. Export shown consumes existing result; export-all is an explicit safe new execution with declared consistency. Cancel/disk-full leaves partial artifact; complete filename appears only after complete terminal+flush/finalize.

### Protection panel and Recovery
Exact selected table IDs navigate existing Scope/Plan workflows. Optional backup-before-change shows a separate dependency and evidence; SQL still needs explicit action. Existing preflight/confirmation/manifest rules remain. Open completed restored DB only after actual target identity proof; new readonly session, no original target fallback. Metadata comparison does not upgrade L1/L3 evidence.

### Operation history and recovery
Journal stores metadata/digests/version/outcomes, not raw SQL/results. Repeated UI hints are ignored; gaps/restart pull authoritative operation status. Startup retires live handles; dispatched uncertain writes become unknown, not replay. DPAPI/disk/journal failure disables new dispatch while cleanup/status remain. Another app instance activates primary/refuses execution rather than taking over sent operations.

## End-to-end scenarios

| ID | Prerequisite | Flow | Completion evidence | Failure behavior |
|---|---|---|---|---|
| OP-01 | Existing allowed target; separate user credential | connect→browse→query→review edit→apply→refresh | exact typed result and confirmed transaction scope | conflict/unknown retains distinct state |
| OP-02 | Existing unprotected tables | browse→exact Scope→existing plan→backup | existing signed evidence, not grid success | no SQL auto-run on backup event |
| OP-03 | Existing valid restore proof | preflight→restore→actual restored target→readonly browse | restored identity and declared verification level | no original fallback |
| OP-04 | Long read and queued next run | run A→request cancel→A finishes/races→B admission | cancel settles before B or old socket retired | unknown write/interrupted read, not false rollback |
| OP-05 | Import file with >1 batch | preview→batch0 commit→batch1 unknown→restart | confirmed batch0 + unknown1 + stopped later batches | no auto replay/resume |
| OP-06 | Slow/offline UI consumer | stream→credit pause→duplicate/future ACK→reconnect | bounded memory, unique sequence apply, operation.get | expiry/partial state, no SQL reexecution |

## Screen flow
Workspace → Database Workbench → Target binding → Explorer/SQL/Results → Edit or Data movement → Operation status.
Workbench Protection → existing Backup/Recovery → validated restored Target → new readonly Workbench Session.

## Limits of this artifact
Scenarios are implementation/QA inputs. No production access, new engine tests, render/performance or user usability session has been executed by writing them.
