# Process Flow Diagrams

Modules loaded from exact source-only catalog paths: flow-verifier plan mode and mermaid-diagrams syntax. No registration/slash calls.

| Process | File | Type | Role lanes | Sections |
|---|---|---|---|---|
| Session access | [session-access.mmd](session-access.mmd) | flowchart | single desktop_owner; components are not new human roles | 02 |
| Browse and stream | [browse-stream.mmd](browse-stream.mmd) | flowchart | single role | 01,04,05 |
| Query and cancellation | [query-cancel.mmd](query-cancel.mmd) | flowchart | single role | 02,03,05 |
| Verified edit | [verified-apply.mmd](verified-apply.mmd) | flowchart | single role | 03,06 |
| Import/export | [data-movement.mmd](data-movement.mmd) | flowchart | single role | 07 |
| Protection links | [protection-links.mmd](protection-links.mmd) | flowchart | single role | 08 |
| Operation outcome | [operation-state.mmd](operation-state.mmd) | stateDiagram-v2 | engine/target trigger labels | 03,05,06 |
| Manual transaction | [transaction-state.mmd](transaction-state.mmd) | stateDiagram-v2 | desktop_owner/target/engine triggers | 05 |
| Commit response loss | [commit-loss-sequence.mmd](commit-loss-sequence.mmd) | sequenceDiagram | user plus external systems | 02,03,06 |

| Local client state | [client-state-er.mmd](client-state-er.mmd) | erDiagram | local scoped records, no RBAC roles | 02,03 |

## Coverage
Six complete processes include denial/failure/unknown terminals. All decision outputs are labeled. Flowchart node budget <=20; state diagrams are additional because persistent states have multiple transitions; sequence is additional because target commit and local journal are independent systems.
Role lanes NOT APPLICABLE: one human role; component participants are explicitly named without inventing RBAC roles.
Flow nodes reference canonical internal commands in comments and match sections/index.md nodes.
Implementation code-to-flow matching NOT RUN: this suite is a planning baseline.
