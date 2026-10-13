# anvil

What PRs should I be looking at?
Sorts open PRs into **Close**, **Action**, **Review** and **FYI** buckets and ranks them.

```
anvil [--explain] [--json] [--all] [--top N]
```

## Configuration

A YAML file at `~/.config/anvil/config.yaml`.

GitHub auth comes from `GITHUB_TOKEN`, or `GH_TOKEN` as a fallback.

```yaml
me: your-github-login          # resolved from the API if absent
orgs: [acme]                   # restrict searches to these orgs (empty = everywhere)

team:
  members: [alice, bob]        # logins counted as "team" authors
  repos: [acme/widgets]        # owner/name counted as "team" repos
  bots: ["renovate[bot]"]      # extra bot logins (GitHub-typed bots are detected anyway)

review_requests:
  teams: [acme/payments]       # only these teams' requests count, plus direct ones (empty = all my teams)
  exclude_teams: [acme/bot]    # requests to these teams never count

thresholds: { ... }            # see below
weights:    { ... }            # see below
```

## Arguments

| Flag | Default | Meaning |
|---|---|---|
| `--explain` | off | Print scoring factors under each PR |
| `--json` | off | Emit the report as JSON |
| `--all` | off | Also list the FYI bucket (PRs that matched no rule) |
| `--top N` | 10 | Max PRs per section, `0` = unlimited |

## Thresholds

| Key | Default | Meaning |
|---|---|---|
| `close_after_days` | 60 | PR age at which it becomes a closure candidate (if also stale or conflicting) |
| `close_stale_days` | 21 | Days without update before an old PR is **Close: stale** |
| `bot_stale_days` | 30 | Days without update before a bot PR is **Close: stale bot** |
| `recency_half_life_days` | 7 | How fast a PR loses priority as it sits untouched (see below) |
| `min_review_score` | 0 | Review/FYI entries scoring below this are hidden |
| `recent_comment_days` | 3 | A comment younger than this counts as recent discussion (see below) |

### Recency

Every PR gets a recency multiplier between 1.0 and 0.5 based on how many days
since it was last updated. A PR updated today gets 1.0. After one half-life it
has lost half of what it can lose, i.e. 0.75. After two half-lives, 0.625, and
so on. It never drops below 0.5, so an untouched PR is at worst halved, never
hidden.

| Days since update | 0 | 7 | 14 | 21 | 28 |
|---|---|---|---|---|---|
| Multiplier (half-life 7) | 1.0 | 0.75 | 0.625 | 0.56 | 0.53 |

A smaller half-life makes staleness bite faster. Exact formula:
`0.5 + 0.5 * 0.5^(stale_days / recency_half_life_days)`.

### Discussion and mentions

The last few dozen comments on each PR are fetched: issue comments, review
bodies and inline review comments. Comments by bots (GitHub-typed or listed in
`team.bots`) and comments by me are ignored.

- A PR has **recent discussion** when someone else commented within
  `recent_comment_days`.
- A PR **mentions me** when someone else wrote `@me` and I have not commented
  or reviewed since. Replying or reviewing clears it; a newer mention sets it
  again. A mention alone is enough to land a PR in **Review**, even in a repo
  nobody asked me to look at.

## Weights

### Review and FYI score

```
score = 100 × author × repo × size × recency × state × discussion  +  bonuses
```

The first six are **multipliers**, each between 0 and 1. Start at 100 and
each factor shaves some off, so a small team PR with green CI scores near 100
and a huge external PR with conflicts scores near 0. **Bonuses** are then
**added** on top, so a PR that was explicitly requested from you can still
float above a "better looking" PR nobody asked you about.

**Multipliers** (exactly one value is picked from each group):

| Group | Key | Default | Picked when |
|---|---|---|---|
| author | `author_team` | 1.0 | Author is in `team.members` |
| | `author_other` | 0.6 | Any other human |
| | `author_bot` | 0.25 | GitHub-typed bot or listed in `team.bots` |
| repo | `repo_team` | 1.0 | Repo is in `team.repos` |
| | `repo_other` | 0.6 | Any other repo |
| size | `size_bands` | ≤50→1.0, ≤200→0.85, ≤600→0.65, ≤1500→0.45 | First band whose `max_lines` ≥ additions+deletions; bands must be ascending |
| | `size_overflow` | 0.3 | Larger than every band |
| recency | *(from thresholds)* | | See [Recency](#recency) |
| state | `state_ci_failing` | 0.5 | CI is failing |
| | `state_conflicts` | 0.6 | Has merge conflicts |
| | `state_changes_requested_by_other` | 0.7 | Someone else requested changes |
| | `state_approved_by_other` | 0.8 | Someone else already approved |
| | *(none)* | 1.0 | None of the above |
| discussion | `discussion_quiet` | 0.8 | Nobody else commented within `recent_comment_days` |
| | *(none)* | 1.0 | There is recent discussion |

If several state conditions apply, the **lowest** one wins; they do not stack.

**Bonuses** (flat points, all that apply are summed):

| Key | Default | Added when |
|---|---|---|
| `bonus_requested_me` | 25 | A review is requested from me directly |
| `bonus_mentioned_me` | 20 | Someone `@mentioned` me and I have not responded (see [Discussion and mentions](#discussion-and-mentions)) |
| `bonus_rereview` | 15 | The PR changed since my last non-approving review |
| `bonus_requested_team` | 10 | A review is requested from one of my teams |

### Action score (my own PRs)

```
score = reason × recency × repo
```

`reason` is the **highest** matching value below; they do not stack. `recency`
and `repo` are the same multipliers as above.

| Key | Default | Matches when |
|---|---|---|
| `action_ci_failing` | 90 | CI is failing |
| `action_ready_to_merge` | 85 | Approved and CI green |
| `action_changes_requested` | 75 | Changes requested |
| `action_conflicts` | 70 | Has merge conflicts |

### Close score

Days since last update. Not configurable.
