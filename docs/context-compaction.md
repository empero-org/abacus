# Context compaction

Open-ended loops and goals accumulate conversation context until the model's window fills. Abacus compacts automatically, in two tiers, so a long run stays coherent instead of degrading. Both tiers respect tool-call→tool-result grouping so a call is never orphaned from its result, and the work product on disk is never touched — only the conversation sent to the model is trimmed.

## Tier 0 — microcompaction (cheap, no model call)

Once the conversation outgrows a *fresh recent window* (the size a post-compaction tail would keep), the stale output of large, re-derivable tools is replaced with a one-line placeholder. The affected tools are `read_file`, `read_files`, `grep`, `glob`, `list_files`, `run_command`, and `git_diff`/`git_show`/`git_blame`/`git_log` — anything the model can simply read again from disk.

The **12 most recent** such results are always kept verbatim, so the model's active working set stays fully visible while older file bodies stop consuming the window on every subsequent request. Decisions and mutations (`edit_file`, `write_file`, `git_commit`, …) are **never** placeholdered — only re-derivable reads are.

Below the recent-window threshold the history is kept fully verbatim. This matters: blanking tool output on a small session just makes the model forget a finding it still needs and re-read it in a loop, so trimming only kicks in once the conversation is genuinely large.

## Tier 1 — rolling summary (one model call)

As the conversation nears the real context ceiling (~80% of the usable window, **including** the running summary that is re-injected each turn), Abacus keeps the system prompt and original user task verbatim, keeps a verbatim recent window, and summarizes the dropped middle into a persisted *running summary*.

The summary is **extended** (not regenerated) on each compaction and re-injected as a system message every turn, so the model retains the long arc of the task across many compactions. The summary survives session resume. If the summarizer call itself overflows, tool-result bodies are stripped from the middle outward, and a drop-only trace fallback keeps the loop alive as a last resort.

`/compact` is a separate **manual** quick-shrink (drop-only, no model call) for cutting context immediately; the automatic rolling compaction runs regardless during long loops and goals.

## How the budget is sized

Compaction thresholds and the output cap **scale with the chosen model**, so a 1M-context model compacts late and a 16k model compacts early, instead of one hardcoded number for all. The model's limits are resolved most-authoritative-first:

1. **Explicit override** — `--context-window` / `--max-output-tokens` (or the `agent.context_window` / `agent.max_output_tokens` settings). Token counts accept `k`/`m` suffixes (`--context-window 200k`, `--max-output-tokens 32k`).
2. **Auto-detection** from the provider's `/models` endpoint (best-effort, 2s timeout, non-fatal) — reads `context_length` and `max_completion_tokens`/`max_output_tokens` per model, tolerating the OpenAI `data` shape, a `models` key, a bare array, and string-typed values like `"256k"`. Detection can *raise* or fill in a window, but it will **never shrink a recognized family below its published context window** — many servers (Ollama especially) self-report a default `num_ctx` like 4k that would otherwise make Abacus compact hundreds of times too early on a model that actually handles far more.
3. **Heuristic table** of *currently shipping* frontier families, matched by family (not pinned version) so a new minor version still matches:

   | Family (name contains) | Context | Max output |
   | --- | --- | --- |
   | `gpt-5` | 1M | 128k |
   | `gemini` (≥1.5) | 1M | 65k |
   | `claude` (3/4/5) | 1M | 128k |
   | `deepseek-v4` | 1M | 384k |
   | `glm-5.2` | 1M | 131k |
   | `glm-5` / `glm-5.1` | 200k | 131k |
   | `glm-4.6` / `glm-4.7` | 200k | 128k |
   | `kimi-k2` | 256k | 32k |
   | `qwen3-coder` | 256k | 65k |

   Dead or pulled models (gpt-4, gpt-3.5, gpt-4o, claude-2) and ambiguous ones (DeepSeek V3, GLM-4.5, plain Qwen3) are intentionally absent — detection or the conservative default handles them rather than a stale entry over-sizing them.
4. **Conservative default** — 128k context / 8k output, which compacts a little early rather than overflowing.

Only an explicit override or a successful detection is treated as confident enough to actually send `max_tokens` to the provider; a heuristic/default estimate sizes the compaction budget only, so a model whose real output ceiling is unknown is never truncated from a guess.

## Checking what your run uses

`abacus doctor` prints the resolved limits and where they came from, e.g.:

```text
limits     1000000 context, 131072 output (heuristic); compacts at ~3000000 chars
```

If that context window is wrong for your model (for example a local server under-reporting `num_ctx`), pin it explicitly:

```sh
abacus --context-window 1m --max-output-tokens 32k
```
