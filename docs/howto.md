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
labels = ["team::platform"]
not_labels = ["wip"]
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

## See merged and closed merge requests

Set `state = "all"` (or `merged` / `closed`). Merged and closed rows are grouped at the bottom, whatever the sort, and dimmed in a list that also holds open ones.

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

Bump `version` in `Cargo.toml`, commit, then tag and push:

```bash
git tag v0.2.0
git push origin v0.2.0
```

The `Release` workflow refuses a tag that does not match `Cargo.toml`. It builds macOS and
Linux archives (x86_64 and aarch64), publishes them as a GitHub release and attaches a
signed `packslip.sigstore.json`. Run the workflow by hand from the Actions tab to build
the archives without publishing anything.
