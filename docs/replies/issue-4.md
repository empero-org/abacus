*DRAFT — awaiting the owner's go-ahead to post; do not paste yet.*

Thanks for the pointers — both are interesting, and we have recorded them rather than closing the issue.

Decision: not now, and not as a defect. Abacus already compacts: sessions are summarised as they grow, compacted while idle, and the budget comes from the model's detected window (`src/compaction.rs`, `src/model_info.rs`; `doctor` reports the compaction point). Whether another approach does better is unproven without measurement.

What would change this: numbers on the same sessions — tokens spent and tasks completed under current compaction versus a candidate. A proposal with those numbers would be read seriously; the budget is one structure (`CompactionBudget`), so a change there is contained.

Does that measurement cover your use case, or does yours look different?

## Sources

- `docs/decisions/i18n-and-token-cost.md`, section "## #4 — token cost": decision ("Not now, and not as a defect"), the two external projects (`yc-duan/fastctx`, `T-Auto/dsh-ops`), existing compaction / context-window behaviour, harness requirement, and actionable measurement plus `CompactionBudget`.
- No issue text was fetched (no `gh`); names and ask are as quoted in that record. Open points are left open above.
