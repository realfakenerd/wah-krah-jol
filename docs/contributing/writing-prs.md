# Writing PRs, issues and review comments

A short guide for anyone sending work here, written by hand or with an AI assistant. It goes with the PR template (`.github/PULL_REQUEST_TEMPLATE.md`); the steps around it (claiming work, drafts, review, merging) are in [workflow.md](workflow.md). The aim is text a reviewer can read in two minutes and a newcomer can follow without already knowing the answer.

## The three rules that matter most

1. **Lead with the result.** The first sentence of a PR says what changes, in plain words, with the headline number if there is one. Mechanism and edge cases come later, or go in a collapsed section.
2. **Explain, don't just state.** For each point, say what goes wrong first, then one concrete example, then what to do and who gains. Explain every Skyrim or engine term the first time it appears; assume the reader knows programming but not Creation Engine formats.
3. **Be honest about evidence.** Tick only the commands you really ran, keep conditions next to the claims they limit ("on Windows", "not runtime-tested"), and give every number its source.

## Voice

- Write concise, full sentences. Cut filler, pleasantries and empty hedges ("maybe", "I think"), but never cut the reason. Headings, field labels and exact commands can stay as shorthand.
- Keep real conditions. "Could", "on Windows" or "not measured on a cold disk" change what a claim means, so they stay right beside it.
- Don't ask others to test or screenshot things for you. Show your own evidence.
- If an AI assistant wrote or helped with the text or the code, say so in one line at the top, for example `> Written with an AI assistant, checked by @you.` Keep it to that one line. See [AI_POLICY.md](../AI_POLICY.md).

## PR titles

PR titles must strictly adhere to the [Conventional Commits](https://www.conventionalcommits.org/) specification (`type(scope): description` or `type(scope)!: description`).

With a squash merge, the PR title becomes the exact commit message on `main` (the repository's squash-merge setting uses the PR title as the commit title). These titles drive changelog generation via `git-cliff` and tell maintainers which SemVer bump a release needs. See [releasing.md](releasing.md). Non-conventional titles are blocked in CI.

- **Format:** `type(scope): plain effect`, at most 72 characters, lowercase after the colon unless the description starts with a listed acronym (for example `fix(engine): NIF parser panic`). Leave out PR numbers and notes like "part 2".
- **Types:**
  - `feat`: New capability, record parser, or feature (maps to *Added*).
  - `fix`: Bug, panic, or visual defect correction (maps to *Fixed*).
  - `perf`: Performance or memory allocation optimization (maps to *Performance*).
  - `refactor`: Structural code cleanup without behavior change (maps to *Changed*).
  - `style`: Formatting, whitespace, or cosmetic code changes without logic changes (maps to *Changed*).
  - `revert`: Reverting a previous commit (maps to *Removed*).
  - `build`: Changes to build system, toolchains, or external dependencies (maps to *Tooling & CI*).
  - `docs`: Documentation, guides, or specifications (maps to *Documentation*).
  - `test`: Adding or correcting tests or fixtures (maps to *Tests*).
  - `chore`: Auxiliary tooling, dependencies, or maintenance tasks (maps to *Tooling & CI*).
  - `ci`: CI workflows, GitHub Actions, and release pipelines (maps to *Tooling & CI*).
- **Scopes:** the allowed scopes (crates such as `converter` and `engine`, and areas such as `ci`, `scripts` and `release`) and the allowed leading acronyms (`NIF`, `LOD`, `BSA`, `MO2` and so on) are listed in `.github/workflows/pr_title.yml`. That file is what CI enforces, so add a scope or acronym there when a new crate or term needs one. A scope is optional when a change crosses several subsystems or applies broadly.
- **Breaking changes (`!`):**
  - Append an exclamation point before the colon for breaking changes: `type(scope)!: description` (e.g., `feat(converter)!: advance world database schema to 10`).
  - In the PR body footer, include `BREAKING CHANGE: <explanation>` detailing migration impact (such as mandatory asset reconversion).

## Release branches

- **`main`**: Target branch for ongoing development and roadmap features.
- **`release/vX.Y.x`** (e.g. `release/v0.1.x`): Maintained for post-release hotfixes and critical patch releases without blocking feature work on `main`.
- Target `main` by default. Only target a release branch when explicitly backporting an approved patch.
- How to tag, publish and hotfix: [releasing.md](releasing.md).

## PR body

Fill in [the PR template](../../.github/PULL_REQUEST_TEMPLATE.md): a one-sentence summary on top, `**Merge after:** #N` if it needs another PR first, then Objective, Details & Implementation (long parts inside `<details>`), Verification & Testing, Visual / Benchmark Proof (leave it out if there is nothing to show), and For reviewers (behaviour change first). The template doesn't say these:

- **Tick only what you ran**, on the final version of the branch. If you ran something narrower or different (one crate instead of the workspace, `--lib` only), leave the box unticked and add a line `Actually run:` with the real command. `cargo nextest run --workspace --all-targets` together with `cargo test --workspace --doc` (nextest skips doctests, see CONTRIBUTING.md) counts as the test box.
- **Length:** aim for about 200 words of prose outside `<details>`, or about 450 for code PRs. Checklists, code blocks and command output don't count. These are targets: go over rather than drop a needed reason, and move mechanism into `<details>` first.
- **Evidence:** for a visual change, before/after pictures from the same command on both builds, cropped to the game window. For a speed change, numbers from a quiet machine, repeated, with the setup stated. For a command-line change, the real output (`$ command`, then what it printed).
- **Pictures:** host them where they won't change (a commit-pinned link, for example `raw.githubusercontent.com/<fork>/<commit>/<file>`), not a link that can move or expire.

## Draft PRs

Open a draft for anything that isn't ready for review yet: work in progress, or code that works while something is still undecided or unmeasured (a design choice for the maintainers, or a measurement you couldn't do). End **For reviewers** with **Still open (why this is a draft):** what is missing, and what would make it ready. Mark it ready for review once that is done.

## Issues

- **Title:** the ask or the problem in plain words.
- **Body:** a one-sentence ask, then why it matters (with an example), then the proposal as bullets, any evidence, and your open questions.
- **Tracking issues** (a list of related work): one sentence saying what the list is for, one line explaining any status words, then checkboxes grouped by area, one line each. Tick an item and add its PR number when it becomes a PR, by editing the list rather than posting a comment for each change.

## Commenting on someone else's work

- **Start with what you support,** specifically, not a bare "+1".
- **Keep three things apart:** what you support, what you would change, and what you have already built (link it). A suggestion is never phrased as a decision.
- **Disagree with reasons:** "I recommend X because Y; the trade-off is Z." No judgements of the person.
- **Questions over demands:** in a review of someone else's PR, number your points and ask. For example, "Could deleted records skip this check?" rather than "Deleted records must skip this check."

## Replying to a review

- Answer each point in the reviewer's order, numbered to match when they numbered theirs.
- Open each answer with **Fixed**, **No change** or **Unresolved**, then give the reason and the commit that has the fix.
- Mention known limits instead of hiding them.
