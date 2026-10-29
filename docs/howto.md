# How-to guides

## Keep the token out of the config file

mrq reads the token from the first of these that is set:

1. `$MRQ_TOKEN`
2. `$GITLAB_TOKEN`
3. `gitlab.token_command`
4. `gitlab.token`

To fetch it from a password manager, set a command whose trimmed stdout is the token:

```toml
[gitlab]
token_command = "security find-generic-password -s gitlab-pat -w"
```

A non-zero exit is a startup error. The command times out after 10 seconds. A literal `token` works too, but mrq warns at startup if the file is readable by anyone but you; run `chmod 600` on it.

## Add a filter

Each `[[filter]]` block is a tab. Their order is the tab order and the `1`..`9` shortcuts.

```toml
[[filter]]
name = "Reviewing"
scope = "review_requested"
```

Names must be unique. Check the result with `mrq check --offline`.

## Watch a group or project

`group` and `project` scopes need a `path`:

```toml
[[filter]]
name = "Platform"
scope = "group"
path = "acme/platform"
labels = ["team::platform", "!wip"]
updated_after_days = 30
```

Use `updated_after_days` on large groups to bound the result set.

## Find merge requests nobody is reviewing

```toml
[[filter]]
name = "Waiting"
scope = "instance"
has_reviewer = false
labels = ["needs-review"]
```

`has_reviewer` and `reviewer` are mutually exclusive.

## Combine several scopes in one tab

```toml
[[filter]]
name = "Needs my attention"
scope = ["assigned", "review_requested"]
```

Each merge request appears once. Only `assigned`, `review_requested` and `authored` can be combined.

## Exclude a relation to you

Prefix a scope with `!` to drop merge requests matching that relation to you, regardless
of what included them. Useful for splitting an instance-wide tab from a personal one so
they don't overlap:

```toml
[[filter]]
name = "In review"
scope = ["instance", "!review_requested"]
labels = ["sre-review::ask"]
has_reviewer = true

[[filter]]
name = "Participation"
scope = ["review_requested", "assigned", "!authored"]
```

"In review" lists every merge request with a reviewer, except the ones where you
are a reviewer — those belong in "Participation" instead, which already excludes what
you authored so it never repeats "Mine".

## See merged and closed merge requests

Set `state = "all"` (or `merged` / `closed`). Merged and closed rows are grouped at the bottom, whatever the sort, and dimmed in a list that also holds open ones. A mark before the title tells the states apart: `✓` merged, `✗` closed (title struck through), `✎` draft — `M`, `C`, `D` with `ascii = true`.

Merged and closed merge requests are capped by `max_results.closed`, 40 by default. Change it for one filter:

```toml
[[filter]]
name = "Mine"
scope = "authored"
state = "all"
max_results = { closed = 100 }  # open keeps [gitlab].max_results.open
```

## Sort one filter differently

A filter's `sort` overrides `[sort]`; a key left out keeps the global value:

```toml
[[filter]]
name = "By age"
scope = "review_requested"
sort = { column = "age", order = "desc" }
```

## Build a READY review queue

Say what READY means, then add a tab showing only those merge requests, longest-waiting first:

```toml
[ready]
labels = ["sre-review::ask"]   # every one required
pipeline = true                # and a passing head pipeline

[[filter]]
name = "SRE queue"
scope = "group"
path = "acme/platform"
labels = ["sre-review::ask"]   # narrows the fetch; `ready` does the rest
ready = true
columns = ["author", "repo", "title", "pipeline", "ready", "updated"]
sort = { column = "ready", order = "asc" }
```

The `ready` column shows how long each merge request has been READY: since its draft was lifted or the label was added, whichever came last. A pipeline re-run does not reset it.

## Choose which columns to show

Set `columns` globally under `[ui]` or per filter. Add `:wide` to show a column only after pressing `w`:

```toml
[[filter]]
name = "Assigned"
scope = "assigned"
columns = ["approved", "author", "title", "age", "branch:wide"]
```

`approved`, `author`, `repo`, `title` and `pipeline` can never be wide-only.

## Rebind keys

```toml
[keys]
quit = ["q"]
open_mr = ["o"]
down = []        # unbind
```

Naming an action replaces its default keys. Two actions on one key is a startup error. See [key specs](reference.md#key-specs).

## Get desktop notifications

Notifications are on by default for new merge requests, approvals and new discussions. To add pipeline changes and merges:

```toml
[notifications]
on_pipeline_change = true
on_merged_or_closed = true   # needs a filter with state = "all", "merged" or "closed"
```

If your terminal has no notification support, run a command instead:

```toml
[notifications]
backend = "command"
command = "notify-send {title} {body}"
```

`{title}` and `{body}` are shell-quoted for you; quoting them again (`"{title}"`) would let a merge request title run shell code.

## Review what changed while you were away

Press `shift-A` to open the activity feed: new merge requests, ones that moved between filters or left them, approvals, discussions, pipeline changes and merges, newest first. Press `enter` on an entry to jump to its filter and select the merge request. The feed is independent of `[notifications]`, so turning a notification off does not hide the change here.

## Change the colours

Press `Ctrl-T` to try a skin for the session. To persist one:

```toml
[skin]
name = "tokyo-night"

[skin.colors]
red = "#ff5555"
```

## Install shell completions

```bash
mrq completion zsh > ~/.local/share/zsh/site-functions/_mrq
mrq completion bash > /usr/local/etc/bash_completion.d/mrq
mrq completion fish > ~/.config/fish/completions/mrq.fish
mrq completion powershell | Out-String | Invoke-Expression
mrq completion elvish > ~/.config/elvish/lib/mrq.elv
```

For zsh, add `fpath+=~/.local/share/zsh/site-functions && autoload -Uz compinit && compinit` to `~/.zshrc`.

## Validate the config in an editor

`config.schema.json` is checked in. Regenerate it with:

```bash
mrq schema > config.schema.json
```

## Cut a release

Bump `version` in `Cargo.toml`, refresh `Cargo.lock`, commit, then tag and push:

```bash
cargo check        # update Cargo.lock
git tag v0.2.0
git push origin v0.2.0
```

The `Release` workflow refuses a tag that does not match `Cargo.toml`, and the build runs
`cargo build --locked`, so `Cargo.lock` must track the new version too. It builds macOS and
Linux archives (x86_64 and aarch64), publishes them as a GitHub release, attaches a signed
`packslip.sigstore.json`, and ships the `LICENSE` in each archive. Run the workflow by hand
from the Actions tab to build the archives without publishing anything.
