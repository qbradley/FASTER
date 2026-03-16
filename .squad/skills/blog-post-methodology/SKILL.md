---
name: "blog-post-methodology"
description: "Proven structure and narrative strategy for technical blog posts spanning complex features"
domain: "documentation"
confidence: "high"
source: "2026-03-07 iteration blogs (1–4)"
---

## When to Use

When writing a blog post documenting a major iteration, feature release, or project milestone. This methodology produces posts that are both technically deep and compelling to read.

## Pattern

### Title Formula

`[Compelling stat or count] [Feature summary], and [Hook or Lesson]`

Examples:
- "1,525 Tests, 6 Crates, and a Vec<u8> That Humbled Us All"
- "Compaction, FFI, and How We Learned to Stop Worrying and Love the Epoch"

The title should answer: "Why should I read this?" with a concrete number or relatable moment.

### Opening (First 2–3 paragraphs)

- **Callback:** Reference the previous blog's closing statement or unresolved question. Creates continuity.
- **Hook:** Why does this iteration matter? What problem does it solve?
- **Scope:** Clearly state: "In this iteration we..." (3–4 main areas)

### Structure (Chronological Sections)

Organize by the *narrative journey*, not just feature categories:

```
1. The Problem We Solved (context)
2. What We Built (feature by feature with real code)
3. Why It Was Hard (a real bug, trade-off, or technical challenge)
4. By The Numbers (metrics table)
5. What Went Wrong (honest post-mortem)
6. Lessons for Users (actionable takeaways)
7. What's Next (teaser for upcoming work)
```

### "By The Numbers" Table

Compare across iterations. Example:

| Metric | Iter 1 | Iter 2 | Iter 3 | Iter 4 |
|--------|--------|--------|--------|--------|
| Test Count | 847 | 1,050 | 1,825 | 2,067 |
| Crates | 4 | 5 | 5 | 6 |
| Lines Doc | 120 | 245 | 460 | 530 |
| Unsafe Lines | 890 | 920 | 1,040 | 1,105 |

Tells a story: what grew, what stayed stable, what effort was required.

### Code Snippets

**Rule:** Every snippet must be real, tested code — not pseudocode or aspirational examples.

Choose snippets for **illumination**, not exhaustiveness:

- **CAS pointer swing:** Show the atomic operation that makes something work
- **Waker bridge:** The 5-line pattern that bridges sync and async
- **TreiberStack:** Why this pattern, what does it do?
- **SimulatedDevice:** How does the fault injection work?

Do NOT include:
- Full module listings (readers can grep for those)
- Obvious boilerplate
- Code that's already in the commit diff

Format with context:
```rust
// CAS loop to swing pointer from old to new
loop {
    match self.cas(old, new) {
        CasResult::Ok => break,
        CasResult::Retry(actual) => { old = actual; continue; }
    }
}
```

Include a 1-line comment explaining the illumination: "This atomic swap is how we guarantee no missed wakeups."

### "What Went Wrong (The Honest Part)" Section

This is the credibility section. Every iteration has a bug, a hard decision, or a learning. Be specific:

Example structure:
- **The Problem:** "We got a Miri failure on the epoch logic"
- **Why It Mattered:** "Epoch safety is critical — get it wrong and recovery fails silently"
- **How We Fixed It:** "We added deterministic simulation tests that reproduce the race condition"
- **What We Learned:** "Atomic operations are easy to get wrong; deterministic replay saves weeks of debugging"

Don't hide this. Users respect honest post-mortems. They show competence + humility.

### "Lessons for Squad Users" Section

Actionable takeaways for developers using the library. Examples:

- "If you see latency spikes during compaction, reduce K1 batch size"
- "When using the FFI, always check HandleTable for poisoned-lock recovery"
- "For Tokio async, the PendingFuture pattern works better than spawning tasks into the runtime"

These should be real, learned from implementation or testing.

### Closing (Fun Coda)

End on a human note. Example from 2026-03-07 blog:

"We also renamed our team from Star Wars to Lord of the Rings. Arwen, Gandalf, Aragorn, and friends will now be leading FASTER through production maturity. Fittingly, our alignment bug fix was the kind of cryptic-until-you-see-it problem only Aragorn (our king of low-level Rust) could unearth."

Then closing links:
- Link to previous posts
- Link to open issues or PRs
- Link to project repo

### Writing Tone

- **Technical depth:** Assume readers understand systems programming, atomics, async/await. Don't explain Rust basics.
- **Narrative flow:** Tell a story. "Here's what we tried, here's what went wrong, here's what we learned."
- **Specific over vague:** "We found a race condition in the CAS loop" beats "We had concurrency issues."
- **Numbers matter:** "2,067 tests" is more credible than "many tests."

### Layout & Formatting

- **Markdown headers:** Use H1 for title, H2 for major sections, H3 for subsections
- **Code blocks:** Always tag with language (```rust, ```bash, etc.)
- **Tables:** Use for metrics, comparisons, feature matrices
- **Emphasis:** Bold for names, italic for concepts, monospace for code terms
- **Line length:** ~80 chars per line keeps it readable in a terminal text reader

## Examples of This Methodology

All successful blogs in this project follow this structure:
- `blog-behind-the-scenes.md` — Iteration 1, founding principles
- `blog-iteration-2.md` — Hash index, hybrid log design
- `blog-iteration-3.md` — Epoch, checkpoint/recovery, test audit
- `blog-iteration-4.md` — Compaction, FFI, Tokio bridge, io_uring, DST

Each ~3000–4000 words, each built on the previous blog's teaser, each includes code snippets + metrics table + honest post-mortem.

## Confidence: high

Methodology proven across 4 shipped blog posts. Pattern consistent, readers understand progression, code examples remain relevant (tested code only).

## Learned From

2026-03-07: Wrote `blog-iteration-4.md` (4200 lines). Established formula: title hook + callback + chronological sections + metrics table + code snippets + honest post-mortem + lessons for users + fun coda. Pattern applies across all 4 iteration blogs.
