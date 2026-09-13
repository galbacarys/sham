# Shared Hierarchical Idea Memory — Concept & Converged Design (codename: shamwow)

> **Purpose.** A shared, cross-project agent memory: a SQLite-backed store of *decision
> units*, each a self-contained idea with its rationale, organized as a hierarchy.
> Memory lives as versioned YAML **inside each project**, a user-global manifest lists
> the projects to include, and a disposable incremental-build SQLite cache gives fast
> retrieval. Retrieval combines vector similarity (discovery) with the local tree
> (context/why), so an agent in one project can "accidentally" find and reuse the
> decision rationale recorded in another. Design has converged — see Open Decisions.

---

## 1. The Core Idea

Agents working across many projects often re-derive the same decision ("why use
Stripe?") because past rationale lives in another project's context. This memory
system makes **decision units** globally retrievable.

An agent writes its knowledge as a **hierarchical tree of ideas**, where each idea
carries its own rationale:

```yaml
project: A
ideas:
  billing:
    id: 01HX...            # stable ULID — CLI-injected
    hash: a1b2...          # content hash     — CLI-injected
    decision: "use Stripe"
    why:                       # children = rationale
      - id: 01HY...
        hash: c3d4...
        text: "idempotent webhooks"
      - text: "handles EU VAT reporting"
  postgres:
    - "serialization bug found in checkout"   # leaf w/ self-contained detail
```

When a later agent in another project searches for past memories about "billing,"
vector search finds the Stripe unit, and retrieval **expands to the local tree** to
surface *why* the decision was made — the rationale transfers even when the feature
doesn't.

## 2. What the Tree Is Actually For

> **Key reframe.** This is *not* "semantically close ideas end up in the same graph
> node." The hierarchy is project-scoped, so graph position encodes provenance, not
> meaning. The actual mechanism is **decision encapsulation**: each node is a
> self-contained unit of *decision + rationale*, and that bundle is what transfers.

Two complementary layers:
- **Vector layer** — *discovery / recall.* Finds similar ideas regardless of project tree.
- **Tree layer** — *framing / context.* Supplies *why* an idea matters (parent + children),
  turning a similarity hit into a *useful* link.

They are complementary, not redundant: embeddings have no notion of "is-rationale-for"
(a specific fix and the general principle it exemplifies embed close together), while
the tree has no notion of semantic distance.

## 3. Physical Shape — Sources in Repos, DB as a Build Cache

The mental model that makes everything else fall into place:

> **The YAML files are the source of truth, co-located with and versioned alongside each
> project's code. A user-global manifest lists which projects' memory dirs to include.
> The SQLite DB is a derived artifact — always disposable and fully re-derivable from
> the sources**, exactly like a compiler cache.

```
~/.config/sham/config.yaml            # user-global manifest: list of project memory paths
project-a/.memory/*.yaml              # memory co-located + versioned with the code
project-b/.memory/*.yaml
~/.cache/sham/sham.db                 # disposable incremental-rebuild cache
```

Properties that fall out:
- **No server, no sync layer.** Cloning a repo brings its memory; the manifest opts it
  into your personal store.
- **DB is not precious.** Corrupt it → delete and rebuild. New machine → point the
  manifest at the cloned repos and rebuild.
- **Memory is reviewed in the same PR as the decision it records** — travel with code.

## 4. Node Anatomy — Identity, Freshness, Reachability

Three separate notions, each doing exactly one job. This is the coherent core.

- **Stable node ID** (random **ULID** — sortable, timestamped, collision-safe)
  = *identity.* Survives moves, renames, re-parenting. Agents update ideas in place.
  Edges/citations point here. "Move" becomes a no-op on identity.
- **Content hash** = *freshness.* Detects that content changed → re-embed that node,
  keep its ID and edges.
- **Reachability over node IDs present in source** = *GC.* Prune what's no longer
  referenced. A pure set-difference / reachability check — no move logic.

What each lifecycle event does:
| Event | Stable ID | Content hash | Action |
|---|---|---|---|
| Edit in place | same | new | re-embed, keep ID + edges |
| Move / rename | same | same | update parent pointers only, citations intact |
| Delete | unreachable | — | prune node + its edges (GC) |

## 5. The Unit Boundary Rule

Defines "what, exactly, is **one** retrievable thing" — what gets embedded, what comes
back, how context is capped.

> **One unit = one idea-node + its immediate children (its rationale), embedded together
> as a single vector and retrieved atomically.**

- **An idea** = a node that carries its own "why" (has / deserves children).
- **A detail / justification** = a leaf; embedded only as part of its parent's unit.
- **Recursion:** if a rationale grows its own rationale (a debate, sources,
  counterarguments), it becomes a new nested idea/unit.

Roles are **relative, not fixed**: a node is an idea *with respect to its children* and
a detail *with respect to its parent*.

**Embedding invalidation must match the retrieval unit.** Because the embedding unit is
the node **plus its immediate children**, the hash that invalidates an embedding must
cover **the node's own content + the identity-and-hash of its immediate children**
(bounded to immediate children, not the whole subtree). Otherwise an in-place edit to a
child leaves the parent bundle stale in the index.

Pragmatic test for any authoring prompt: *"Does this node have / deserve a 'why'
underneath it?"* — yes ⇒ idea (own unit); no ⇒ detail (stay a leaf).

## 6. No Extra LLM — The Judgment IS the Structure

No runtime classification exists. The presenting agent makes the call *in the act of
writing*, encoded in **shape**, not a `type` field:

- Node given **children** ⇒ declaring "this is an idea, here's its why" ⇒ unit.
- Node written as a **leaf** ⇒ declaring "this is self-contained detail" ⇒ embeds with parent.

A single self-supporting justification can be inlined into the node text (making it a
leaf). **No field, no second model, no validation heuristic** (a "detect 'because' and
expand" rule would just be a classifier in disguise).

> **The cost of a misjudged boundary is low** — degradation, not failure. Be deliberately
> permissive.

## 7. CLI — Provisional Shape (`sham`)

The CLI owns the YAML (injecting IDs/hashes) and the cache, keeping everything
deterministic and imperative. Query time is pure vector+KNN+tree-walk — no LLM.
Two families of commands:

```bash
# Authoring (agent-facing) — project implied by context
sham add-project foo                          # scaffolding + name the project
sham add "billing" "chose to use Stripe"      # new idea; returns node ID; warns if a similar node exists
sham add --node-id <id> "justification X"     # attach a justification/leaf under an existing node
sham supersede <id> "new decision text"       # retract/replace: writes a `supersedes` edge; node stays as history
sham rm <id>                                  # retract an idea: remove the node; GC'd at reconcile
sham get "billing"                            # fetch the local tree (structural, no embedding)
sham search "billing"                         # cross-project vector search + expanded tree context

# Maintenance
sham reconcile   # incremental: re-embed changed units + GC unreachable IDs (transactional)
sham build       # explicit full rebuild (escape hatch vs lazy first-use)
sham export --project B > b.yaml
```

- **Two families.** *Authoring* commands mutate the tree — they write/update the persisted
  YAML (which stays the source of truth, keeping IDs + hashes correct) and are the primary
  interface for agents. *Maintenance* commands materialize and reconcile the SQLite cache.
- **Project is implied.** `add-project foo` establishes a default (nearest `.memory/`
  marker in the tree), so `add`/`get`/`search` need no project flag — one-project working
  set at a time.
- **`add` returns a node ID** — a stable handle the agent references later (`--node-id`) to
  attach rationale or update in place. **Self-correction / duplicate prevention:** before
  writing, `add` embeds the new node (one embed + a KNN probe) and, if a *semantically
  similar* node already exists, prints a **non-blocking warning** surfacing the existing
  node's **ID + location** so the agent can attach to it instead of creating a duplicate
  tree. Same embedding model — no extra LLM. Keep the threshold loose / recall-favoring,
  since it only warns and never gates.
- **`get` vs `search`.** `get` is *local + structural* (no embedding): by label or
  `--node-id`, returns the unit + subtree (`--expand <depth>`). `search` is *cross-project
  + vector*: ranked hits with provenance (project/file), similarity score, and expanded
  tree context. Keeps cheap local ops off the embedding path.
- **Retraction is a first-class verb.** `sham supersede <id> "..."` marks a node superseded
  and writes a `supersedes` edge (the node stays retrievable as history); `sham rm <id>`
  removes a node (GC'd at reconcile). Agents can express "this decision is no longer valid"
  without understanding GC mechanics.
- **Authoring is eager, reconcile is periodic + CI-wired.** `add` persists YAML *and*
  incrementally embeds the one unit so the next `search` is immediately fresh — the "no LLM
  at query" invariant holds because the add *is* the ingest. `reconcile` handles
  cross-project rebuild, GC of unreachable IDs, and pulling roots from freshly-cloned repos.
  **Reconcile must also run in CI (per-PR):** otherwise a freshly-cloned project isn't visible
  until a manual reconcile, and the §11 per-PR knowledge-diff would be stale at review time.
  Incremental + disposable makes per-PR reconcile cheap — wiring it into CI is feasible, not
  bloat.

### Implementation language — Rust-lean

Rust is the leaning for this project. The C-ABI question is really about the *embedding
runtime*, where Rust dominates:

- **`rusqlite`** (bundled SQLite) — first-class native bindings, no cgo-style friction.
- **`ort`** (ONNX Runtime) or **`candle`** (pure-Rust HF) — run local embeddings
  **in-process**, so there's **no Python sidecar**. This is the decisive win: Go's
  onnxruntime binding is thin (usually → shell out to Python).
- **`tokio`** for the parallel re-embed fan-out (the Go concurrency motivation is fully
  satisfied in Rust).
- Static single binary, strong typing for the CLI.

**One embedding model per index.** Cloud (OpenRouter) and local (HF/ONNX) embeddings live
in *different vector spaces* — they are **mutually exclusive configurations per database**,
not additive. Record which `model + dim` produced the index; switching models invalidates
every vector ⇒ rebuild, which is free because the DB is disposable. Support both behind an
interface (cloud-vs-local is a config swap, not a code branch); never blend models in one
index.

**Change detection — hash is truth, mtime is only a fast-path.** Hash-based staleness is
the source of truth for "did this change"; mtime+size is only a cheap short-circuit to
skip hashing unchanged files. **Never rely on mtime alone — git checkouts reset mtimes**
(after `git pull`/clone every file looks changed), so mtime-only would trigger full
re-embeds, and mtime-preserving copies could miss real edits. Hash first, short-circuit
with the cheap check.

**Incremental reconcile is transactional.** Run all changed-file re-embeds **and** orphan
GC inside a single SQLite transaction, so a crash mid-run never leaves a half-updated
index.

## 8. Node-Level Hashes + Stable IDs — the Payoff

Granularity is node-level, with **two IDs per node** (stable ULID + content hash).
This is what buys the "whole enchilada":

- **Update existing ideas in place** — same ID, new hash; an agent edits rather than
  replaces, history + edges survive.
- **Moves retain node IDs** — so **citations aren't broken by a move.** Moves become a
  cheap "old hash gone + new hash seen" reachability step with identity preserved.
- **Cross-project citation layer is safe to build** — stable IDs mean a citation survives
  the thing it cites being moved.

**IDs live *in* the YAML, not stashed only in the DB.** The CLI generates/manipulates the
YAML precisely so it owns ID creation — this is **load-bearing**: it keeps the DB fully
derivable from source, preserving the disposable-cache property. Putting IDs only in the
DB would silently make the DB-as-source-of-truth and break rebuildability. A cleaner
human-facing form, if ever wanted, is a *separate rendered view* the CLI generates from
the canonical-with-IDs file — a distinct question.

## 9. Citations Are Personal — The Two-Layer Split

Two kinds of data share the vector index but **must not share a transport** — they have
different provenance and reach:

- **Shared, git-backed layer** = per-project `.memory/*.yaml` decision units. Durable
  facts, versioned, travel with the code, rev-able. *"What we decided and why."*
- **Personal citation layer** = the correlation / citation graph — **your synthesis**
  ("this idea relates to that one, from the perspective of my work"), not facts.
  Opinionated, machine-specific, reflects your local clone set. Its own **personal
  meta-repo** (dotfiles-style), cloned alongside your projects — never pushed into the
  shared KB.

A citation is a **self-contained card, not a bare pointer**:

```
{citation}
  repo-id: <stable handle>    # name + last-known remote URL (advisory location, not required)
  node: 01HX...               # stable ULID within that repo
  snippet: "chose Stripe: idempotent webhooks, EU VAT"   # distilled rationale, INLINE
  note: "reuse this tradeoff pattern elsewhere"
```

The inline **snippet** is the key move: if the source repo is missing (new machine, lost
access), the citation renders as **degraded-but-useful** — distilled rationale + note,
just no live expansion. Graceful degradation, not fragility.

**Uneven reachability.** The shared layer owns *idea* reachability (hard GC of unreachable
node IDs — prune). The personal layer owns *citation* reachability, but **soft** — a
citation whose repo is missing is **parked as "awaiting clone," never pruned**. Neither
layer depends on the other's machine state.

**Migration.** Clone the meta-repo alongside your projects; refs whose repos you haven't
cloned render as "with snippet" until the repo is present. No broken edges in the shared
KB, because it never knows about citations.

Optional best-effort `sham resolve-missing` (offers to clone) is fine — but it is **never
a correctness requirement**, because making shared-KB integrity a function of every
machine's local clone set would be unbounded and stateful (the wrong dependency
direction).

**Reframed: "personal" is *epistemic type*, not privacy or machine-state.** The shared
layer holds decisions ↔ rationale — what the team *collectively* stands behind. The personal
layer holds *noticing* ("these two things rhyme") — individual, pre-evidential, and sharing
it would pollute the shared layer with associative noise that looks like decision-weight but
isn't. The personal layer is a **thinking tool / attention layer**, not a memory tier:
explore a noticing → it either graduates into a real shared decision unit or it doesn't —
a human/agent judgment call, **not** an explicit `promote` mechanic (that would over-engineer
a judgment). **No obligation to durability:** lighter GC, no status lifecycle, no PR review.
"Personal" = *perspectival* — the lens each agent uses to navigate the collective truth;
neither lens needs reconciling into a canonical view.

**Durable-personal, not precious.** The personal layer is a per-user artifact (migratable
between machines, dotfiles-style) but deliberately *not precious* — no obligation to
preserve it. Choose this over ephemeral-per-machine attention; it survives migration without
becoming load-bearing.

**Drift-check with `needs-recheck`.** "The model does the diff" only works if a card gets
*re-seen*, so the system surfaces *when there's a diff to look at*. Reconcile compares each
personal card's pinned node hash/status against the shared node's *current* hash/status; if
they differ beyond what the card recorded, flag the card `needs-recheck`. An agent then runs
the semantic diff — snippet vs. live content — deciding whether the insight still holds, has
strengthened, or is invalidated. The ULID resolves live when reachable; the snippet is the
prior state and the comparison point. A superseded/changed source surfacing as a surprise is
useful signal, not a bug.

## 10. Pruning & Compaction — Proposals, Never Actions

Compaction can delete or rewrite durable knowledge, so the CLI **never acts
automatically** — it emits a **proposal**, and a human (or the PR review) decides.
Automatic detection, human decision.

**Blunt structural heuristics** (run at reconcile; no embeddings, no LLM judging
staleness):

- **Subtree sprawl** — subtree beyond ~25 nodes or ~4 levels → split/summarize candidate.
- **Detail creep** — a "detail/leaf" grown children past ~2 levels → promote or collapse.
- **Cold / never-retrieved** — zero hits for ~60 days → archive candidate (needs a cheap
  `hits` counter bumped on retrieval).
- **Duplicate-cluster density** — ≥N near-dup warnings firing inside the same subtree
  region → sprawl / redundancy signal.

**Surfacing, not nagging.** Fire **statefully** — warn once when a signal *crosses* its
threshold, not on every call (or you desensitize the agent and spam the human). Emit one
terse line ("subtree billing sprawled past 25 nodes — run `sham check`"); a dedicated
`sham check` prints the full candidate list (id, label, path, which heuristic fired).
Cleanest home: the **per-PR knowledge diff** (§11) — the same step that shows "what this
PR learned" also flags new sprawl / duplicate clusters for consolidation, landing pruning
review right where the change is being reviewed.

### Skill design — keep it cheap to load

Goal: a few hundred tokens, never ~5k. Achieved by **size + split, not by shrinking the
design**:

- **Tiny SKILL.md** (~120 words ≈ 250–300 tokens): core verbs + rules + output contract.
  Everything else — schema, YAML format, edge cases — lives in a `references/` file the
  agent pulls **on demand**.
- **Terse-by-default output.** `add`/`get`/`search` return compact (id, label, one-line
  content, similarity score); full tree behind `--expand` / `--json`. `add` echoes the
  handle the agent needs to continue (its new id, or the similar-existing id).
- **`add` = exactly two positionals** (`label`, `content`) + optional `--node-id`; no
  variable-arity ambiguity. Multiple justifications = multiple `add --node-id` calls.
- **`get` vs `search`** by what you already know: *search* when unsure (returns
  candidates), *get* when you have an id/label (returns the tree).
- **Maintenance off the agent's vocab.** `reconcile` / `build` / `export` are human/CI
  terrain; the skill says "the index stays fresh automatically; you never run maintenance."

## 11. The Killer Benefit — Per-Commit / Per-PR Knowledge Diffs

Because memory YAML is **co-located and versioned with the code**, git history becomes a
**time axis over the knowledge graph**. The DB can be materialized at two commits and
diffed:

- What node IDs / hashes changed between commit A and commit B?
- What *new* knowledge arrived? What decisions were revised in place?

This produces **"here's exactly what was learned in this PR."** An agent can diff the
knowledge graph across the PR's branch and main, then post a **comment / note summarizing
the new knowledge** — new ideas added, rationale changed, decisions superseded. That is
"what did we learn here" turned into a first-class, reviewable artifact, generated from
the same source that records the knowledge. Powerful and *exactly* the "memory travels
with the code" property paying off.

## 12. Related Work

- **GraphRAG** (Microsoft) — community/cluster-based retrieval over a knowledge graph.
- **HippoRAG** — personalized PageRank over a KG to boost multi-hop recall.
- **MemGPT / Letta** — agent memory as a managed tier.
- SQLite fits: relational adjacency + FTS5 (text) + a vector index (e.g. `sqlite-vec`)
  in one file — no server. Rust path: `rusqlite` (bundled) + `sqlite-vec` bindings.

## 13. Failure Modes & Mitigations

Pressure-tested. The deepest risk isn't a bug in any one piece — the tool's core value
(cheap reuse of past reasoning) is in direct tension with what you actually want
(correctness). It trades *independent re-derivation* for cheap *consensus*, and consensus
is a prior that resists correction. Each failure mode below has a light, structural
answer; the irreducible core is called out at the end.

| # | Failure mode | Mitigation |
|---|---|---|
| 1 | **No supersession** — "changed our mind" isn't representable; contradictory decisions coexist with equal authority | Write-time `supersedes` capture (below) |
| 2 | **Memory as attractor / bias amplifier** — writer + reader is the same agent; a bad prior gets written, retrieved as ground truth, reinforced | `status`/`outcome` linkage — the graph can say "we know this may be wrong" |
| 3 | **Cross-project trust = contamination** — a rationale is a narrative, not proof; reused as if law | Provenance + `borrowed` flag (below) |
| 3b | **Security boundary leak** — shared memory crosses repo/client boundaries | Per-project `share:` scoping (below) |
| 4 | **Near-dup warning encourages staleness** — new, correct framing attaches under an old, wrong node | Warning branches attach-OR-supersede (below) |
| 5 | **Frozen semantics / no outcome feedback** — embeddings frozen on last edit; records "why decided," never "did it work" | status/outcome + lazy re-embed on cross-project retrieval |
| 6 | **Bad capture / review theater** — memory diffs are the most-skimmable PR change; tidy structure looks authoritative | Per-PR knowledge-diff as first-class review surface; maintenance stays proposal-not-action |

**Write-time supersede capture (kills #1).** You don't need an expensive git-history crawl
on every reconcile — you have the *previous* value **in hand at the moment of the update**.
When update-in-place fires, record an edge `new_supersedes_old` (old hash + old snippet) in
the same write. Git history is the **fallback** for reconstruction, not the routine path.
Directly feeds the per-PR diff ("you changed your mind here").

**Status/outcome linkage (kills #2, the heart), gated by `kind`.** The single
highest-leverage anti-attractor move. Coarse node `status` enum:
`hypothesis | decided | validated | superseded`, plus an optional `outcome` field the agent
fills in later ("we tried this — webhooks were NOT actually idempotent").

- **`kind: empirical | normative`** distinguishes empirical decisions ("did Stripe's webhooks
  hold up?") from normative conventions ("we use ADRs"). Empirical nodes can reach
  `validated`; normative nodes can only reach `superseded` (convention changed) or stay
  `decided` indefinitely — they get *entrenched* or *revisited*, not "*validated*."
- **`validated` is earned, not declared:** setting `validated` requires a non-empty `outcome`
  field, or it's indistinguishable from `decided` and carries no weight.
- **`kind` defaults to empirical** (the common case that wants validation) and is set
  explicitly only for conventions — minimal authoring tax. Records a rationale as falsified
  *without rewriting history*. This is what lets the graph represent "this may be wrong"
  instead of treating all rationale as equally-true-forever.

**Lifecycle trigger moments — built into existing commands, not new ones.** State transitions
don't happen because agents proactively revisit nodes (the agent that learns the truth is a
different session with no reason to look back). Instead, prompt at the moment the agent is
already looking:
- **On `search`:** a `decided` node older than a threshold appends a terse line —
  "this node is 90 days old and unvalidated — do you have outcome data?"
- **On `add`:** in a domain where `decided` / aged nodes already exist, surface "here's what's
  already decided in this space — any updates?" (double duty: duplicate-prevention +
  staleness-surfacing).
- **`aging` as a computed reconcile flag** (not a stored enum value): the per-PR diff surfaces
  aging nodes beside new knowledge — passive visibility, no interruption. Old *normative*
  nodes read as `stable` — a *positive* signal — not `aging`.
- These ride on `search`/`add`/`reconcile` output: zero new CLI verbs, fits the token-cheap
  skill.

**Provenance + `borrowed` flag (kills #3).** Coarse provenance per node:
`self | user-stated | from-project:<X> | inferred`. Cross-project pulls are stamped
`borrowed`; retrieval output distinguishes "claim from A" from "fact validated here," so a
reused rationale is labeled as imported rather than silently adopted as ground truth.

**Shareability scoping (kills #3b).** Manifest per-project `share: false` excludes a
project from the shared store / cross-project search. One boolean, no RBAC, but restores
the repo-boundary segregation that shared memory otherwise erases. Not optional for
client/private work.

**Duplicate warning branches attach-OR-supersede (kills #4).** The same one-probe near-dup
warning also surfaces the similar node's **status + age** and offers a second branch:
"similar node exists at <id>, status `decided`, age 90d — create a *new superseding* node."
Stops assuming match⇒attach, which was dragging new framing under stale parents.

**Lazy re-embed on cross-project retrieval (kills #5).** Don't re-embed everything. When a
long-unmodified node is pulled as a cross-project hit, re-embed just that node on that
retrieval (bounded, one call) so its vector reflects present meaning *before* it is reused
in a new context.

**The irreducible core.** A single agent that both writes and reads its own memory can
always bootstrap a wrong belief; no safeguard fully prevents that. `status`/`outcome` does
not *prevent* it — it makes the system **capable of noticing** and **cheap to correct**.
That is the best any memory tool achieves.

## Open Decisions (mostly locked)

- [x] YAML-as-source-of-truth, co-located per project; global manifest; DB = disposable build cache
- [x] Node-level granularity; two IDs (stable ULID + content hash); IDs in the YAML
- [x] Hash-as-truth change detection; mtime as fast-path only
- [x] GC = reachability over node IDs (pure set-difference)
- [x] Embedding invalidation covers node + immediate children (matches retrieval unit)
- [ ] Cross-project **citation layer** on top (mark pulled-foreign ideas, keep back-links) — enabled by stable IDs; v2
- [ ] **Per-PR knowledge-diff** comment generation — workflow/UX on top; serves as the home for pruning-warning surfacing; v2
- [x] Transactional incremental reconcile
- [x] Implementation: Rust-lean — rusqlite + ort/candle in-process embeddings; no cgo, no Python sidecar; tokio fan-out
- [x] One embedding model per index; record model+dim; switching ⇒ rebuild (model change invalidates all vectors)
- [x] CLI surface: add-project / add / get / search / reconcile (+ non-blocking near-duplicate warning on `add`)
- [x] Embedding backend behind an interface — OpenRouter cloud vs local HF/ONNX as a config swap
- [x] Codename: shamwow
- [x] Failure-mode mitigations (§13): write-time `supersedes` capture; `status`/`outcome` enum+field; provenance + `borrowed` flag; per-project `share:` scoping; near-dup warning branches attach-or-supersede; lazy re-embed on cross-project retrieval
- [x] `kind: empirical | normative` gate; `validated` earns only with a non-empty `outcome`; `kind` defaults to empirical
- [x] Lifecycle trigger moments ride on `search`/`add`/`reconcile` output — stale-node prompt, add-time domain-awareness, `aging` as a computed flag surfaced in the PR diff (normative old nodes read as `stable`)
- [x] Reconcile wired into CI (per-PR) so the §11 diff is current and freshly-cloned projects surface promptly
- [x] CLI retraction verbs: `sham supersede <id> "..."` / `sham rm <id>`
- [x] Personal layer reframed as epistemic *noticing* / thinking-tool (perspectival, not private); durable-personal-but-not-precious; reconcile-time `needs-recheck` drift flag
