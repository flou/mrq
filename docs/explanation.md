# Explanation

## Why filters are tabs

Reviewing is a set of questions asked repeatedly: what is waiting for me, what did my teammate open, what has nobody picked up. mrq turns each into a `[[filter]]` and shows it as a tab, so one keypress (`1`..`9`) answers one question.

## Two kinds of scope

`assigned`, `review_requested` and `authored` are rooted at you: GitLab already knows what they mean, so they take no further arguments. `group`, `project` and `instance` are searches, so they accept narrowing arguments such as labels or milestones.

mrq rejects a narrowing argument on a user-rooted scope at startup. Silently ignoring it would show a list that looks filtered but is not.

## Combining scopes

A list of scopes runs one query per scope and merges the results, with each merge request shown once. The cap for the filter's state caps the merged list and keeps the most recently updated, so a noisy scope cannot hide a quiet one for long: `max_results.open` for `opened`, `max_results.closed` for `merged` and `closed` (with `state = "all"`, each cap applies to its own rows). Only user-rooted scopes combine; a `group`, `project` or `instance` filter has its own narrowing arguments, which would not make sense applied to the other scopes.

## Refreshing politely

mrq often targets a shared GitLab instance, so it polls conservatively:

- The interval has a 30 second floor. Below it mrq refuses to start rather than quietly correcting the value.
- Random jitter keeps many instances from polling in the same second.
- Filters start staggered, each with its own timer.
- Errors back off, capped at the refresh interval.

You can always refresh by hand: `Ctrl-R` for everything, `r` for the current tab.

## The cache

On startup mrq paints the previous results of each filter from a cache file, flagged as cached in the status bar, while the live fetch runs. The first live fetch after a warm start raises no "new" markers or notifications, otherwise every start would announce everything as new. The cache holds no token and is private to your user.

## Notifications that stay useful

Notifications are deduplicated by merge request and kind, so a merge request matching two filters is announced once. If a refresh produces more than three events they collapse into a single summary. Pipeline changes and merges are off by default because they fire far more often, and a noisy default trains you to ignore all of them.

`on_merged_or_closed` needs a filter that includes merged or closed states. With the default `opened`, a merged merge request simply disappears from the results, so mrq could never tell it merged. mrq rejects that combination at startup instead.

## The activity feed

Notifications decide what is worth interrupting you for; the activity feed (`shift-A`) records everything they were chosen from. Every change mrq sees lands there whatever the `[notifications]` toggles say, including pipeline changes, merges and merge requests that left a filter. Dedup and focus gating do not apply. The one exception is a pipeline moving into an in-progress status (created, pending, preparing, waiting for a resource or running): only where it ends up is recorded, so a single run does not flood the feed.

A merge request that leaves one filter and shows up in another within 15 minutes is shown once, as a move. The two filters refresh on their own timers, so mrq pairs the departure with the arrival in either order. When filters overlap, a change seen through more than one of them (the same approval, say) is recorded once.

A filter that hit `max_results`, or whose fetch came back partial, is missing rows that still exist. Nothing is reported as having left that filter until a complete fetch arrives. Like notifications, the first fetch after startup records nothing.

The feed is kept in `activity.json` in the state directory, for a week and at most 500 entries. The status bar counts entries added since you last opened it.

## Why unknown keys are errors

A typo in a config should show itself at startup, not look like a bug later. For the same reason, conflicting key bindings and invalid combinations are fatal.

## The token

mrq needs only the `read_api` scope. It prefers sources that keep the secret outside the config file: environment variables, then a command such as a keychain lookup. A literal `token` is allowed, with a warning if the file is readable by others.
