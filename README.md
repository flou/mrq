# mrq

A keyboard-driven TUI for the GitLab merge requests you need to review.

## Installation

Build from source:

```bash
cargo install --path .
```

## Configuration

Create a config file at `~/.config/mrq/config.toml` (or `$XDG_CONFIG_HOME/mrq/config.toml`):

```toml
[gitlab]
url = "https://gitlab.example.com"
# token = "glpat-..."  # or use token_command, $MRQ_GITLAB_TOKEN, $GITLAB_TOKEN

[[filter]]
name = "Waiting"
has_reviewer = false
scope = "instance"
labels = ["needs-review"]

[[filter]]
name = "Assigned to me"
scope = "assigned"
state = "opened"

[[filter]]
name = "Team mate 1"
scope = "instance"
author = "my.teammate"
```

Generate a commented default config, documenting every key and its default:

```bash
mrq init-config
```

### Filters

Each `[[filter]]` block is a tab. The order in the config file is the tab order and the
`1`..`9` shortcuts.

#### Scope

| `scope`            | merge requests                       | requires `path` |
| ------------------ | ------------------------------------ | ---------------- |
| `assigned`          | assigned to you                      | no                |
| `review_requested`  | where you are a reviewer             | no                |
| `authored`          | you opened                           | no                |
| `group`             | under a group (and its subgroups)    | yes               |
| `project`           | in one project                       | yes               |
| `instance`          | every one the token can see          | no                |

`assigned`, `review_requested` and `authored` are rooted at the current user. `group`,
`project` and `instance` are unscoped searches and are the only scopes that accept the
narrowing arguments below — `labels`, `not_labels`, `author`, `assignee`, `reviewer`,
`has_reviewer`, `milestone`, `target_branch` and `updated_after_days`. Setting one of
those on `assigned`, `review_requested` or `authored` is a startup error rather than a
filter that silently ignores it.

#### Fields

| key                  | scopes                          | notes                                                                 |
| -------------------- | -------------------------------- | ---------------------------------------------------------------------- |
| `name`               | all                               | must be unique; used for the tab label and the cache file              |
| `scope`              | all                               | see table above                                                        |
| `state`              | all                               | `opened` (default) \| `merged` \| `closed` \| `all`                    |
| `show_drafts`        | all                               | per-filter override of `[ui].show_drafts`                              |
| `notify`             | all                               | per-filter override of `[notifications].enabled`                       |
| `max_results`        | all                               | clamped to `1..=500`, default `100`                                    |
| `path`               | `group`, `project`                | required; a GitLab full path like `acme/platform`, no leading/trailing slash |
| `include_subgroups`  | `group`                           | defaults to `true`                                                      |
| `labels`             | `group`, `project`, `instance`    | AND-ed                                                                  |
| `not_labels`         | `group`, `project`, `instance`    | excludes merge requests carrying any of these labels                   |
| `author`             | `group`, `project`, `instance`    | username                                                                |
| `assignee`           | `group`, `project`, `instance`    | username                                                                |
| `reviewer`           | `group`, `project`, `instance`    | username; mutually exclusive with `has_reviewer`                       |
| `has_reviewer`       | `group`, `project`, `instance`    | `true` = has any reviewer, `false` = has none; mutually exclusive with `reviewer` |
| `milestone`          | `group`, `project`, `instance`    | milestone title                                                        |
| `target_branch`      | `group`, `project`, `instance`    |                                                                          |
| `updated_after_days` | `group`, `project`, `instance`    | bounds the result set, useful for large groups/instances               |

`mrq check` validates a config file against these rules without starting the TUI.

## Usage

```bash
# Start the TUI
mrq

# Validate config and check connectivity
mrq check

# Print JSON Schema for config validation
mrq schema

# Generate shell completions
mrq completion zsh > ~/.local/share/zsh/site-functions/_mrq
mrq completion bash > /usr/local/etc/bash_completion.d/mrq
mrq completion fish > ~/.config/fish/completions/mrq.fish
```

## Shell Completions

Supported shells: `zsh`, `bash`, `fish`, `powershell`, `elvish`.

```bash
# zsh
mrq completion zsh > ~/.local/share/zsh/site-functions/_mrq
# Add to ~/.zshrc: fpath+=~/.local/share/zsh/site-functions && autoload -Uz compinit && compinit

# bash
mrq completion bash > /usr/local/etc/bash_completion.d/mrq
# Or source directly: source <(mrq completion bash)

# fish
mrq completion fish > ~/.config/fish/completions/mrq.fish

# powershell
mrq completion powershell | Out-String | Invoke-Expression
# Add to $PROFILE for persistence

# elvish
mrq completion elvish > ~/.config/elvish/lib/mrq.elv
# Add to ~/.config/elvish/rc.elv: use mrq
```
