# Explanation

## Why filters are tabs

Reviewing is a set of questions asked repeatedly: what is waiting for me, what did my teammate open, what has nobody picked up. mrq turns each into a `[[filter]]` and shows it as a tab, so one keypress (`1`..`9`) answers one question.

## Two kinds of scope

`assigned`, `review_requested` and `authored` are rooted at you: GitLab already knows what they mean, so they take no further arguments. `group`, `project` and `instance` are searches, so they accept narrowing arguments such as labels or milestones.

mrq rejects a narrowing argument on a user-rooted scope at startup. Silently ignoring it would show a list that looks filtered but is not.

## Combining scopes

A list of scopes runs one query per scope and merges the results, with each merge request shown once. `max_results` caps the merged list and keeps the most recently updated, so a noisy scope cannot hide a quiet one for long. Only user-rooted scopes combine; a `group`, `project` or `instance` filter has its own narrowing arguments, which would not make sense applied to the other scopes.

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

## Why unknown keys are errors

A typo in a config should show itself at startup, not look like a bug later. For the same reason, conflicting key bindings and invalid combinations are fatal.

## The token

mrq needs only the `read_api` scope. It prefers sources that keep the secret outside the config file: environment variables, then a command such as a keychain lookup. A literal `token` is allowed, with a warning if the file is readable by others.
