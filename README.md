# sham

**S**hared **H**ierarchical **A**gent **M**emory — the *wow* is the sound you make
when project B's agent finds project A's decision rationale. (Yes, it's a towel
infomercial reference. We are choosing not to belabor it here.)

sham is a shared, hierarchical, cross-project memory for agents. It stores
*decision units* — a decision and the rationale behind it — as YAML co-located in
each repo, and serves them through a disposable SQLite cache with two retrieval
modes:

- **`search`** — cross-project vector discovery (find ideas that are *similar*)
- **`get`** — local graph context (why an idea matters: its parent + children)

The bet: an idea's *rationale* transfers across projects even when the feature
doesn't — so project B can answer "why Stripe?" from project A's actual decision
instead of re-deriving it.

## Status

Early, iterative development. The design has converged on paper (see
[`docs/DESIGN.md`](docs/DESIGN.md)) and we're now pushing it against
`rusqlite` / `candle` / borrow-checker reality. Expect the CLI surface to churn.

## The name

```
S hared
H ierarchical
A gent
M emory
```

...wow. It was either this or a towel.

## Development

We work in git worktrees off a freshly-pulled `main`, open PRs, and stop for
review. The discipline: branch, push, open a PR, wait. No self-merges.

## Roadmap

- [ ] Rust skeleton + SQLite schema, `add` / `get` for one project (local)
- [ ] embeddings backend (OpenRouter cloud / local ONNX-candle) — one model per index
- [ ] `search` cross-project + `--context` tree expansion
- [ ] global manifest, reconcile, GC (reachability), per-PR knowledge-diff
- [ ] citations / personal layer (`needs-recheck`, supersession)