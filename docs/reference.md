# Reference

## Command line

```
mrq [--config PATH] [--filter NAME] [--refresh-secs SECS] [--log PATH] [--log-level LEVEL]
mrq <subcommand>
```

| Option | Description |
| --- | --- |
| `--config <PATH>` | Config file. Overrides `$MRQ_CONFIG` and the default location. A missing file is an error. |
| `--filter <NAME>` | Open on this filter instead of the first. |
| `--refresh-secs <SECS>` | Refresh interval for this run. Overrides `refresh.interval_secs`; the 30 s minimum applies. |
| `--log <PATH>` | Write the log here instead of the state directory. |
| `--log-level <LEVEL>` | `trace`, `debug`, `info`, `warn` or `error`. |

| Subcommand | Description |
| --- | --- |
| `init-config [--force]` | Write a commented default config. `--force` overwrites an existing file. |
| `check [--offline]` | Validate the config, verify the token and test connectivity. `--offline` skips the network and needs no token. |
| `schema` | Print the JSON Schema of the config file. |
| `completion <shell>` | Print completions for `bash`, `elvish`, `fish`, `powershell` or `zsh`. |

## Files and environment

| Item | Location |
| --- | --- |
| Config | `--config`, then `$MRQ_CONFIG`, then `$XDG_CONFIG_HOME/mrq/config.toml`, then `~/.config/mrq/config.toml`. With none present, built-in defaults apply. |
| Cache | `$XDG_CACHE_HOME/mrq/<filter>.json`, falling back to `~/.cache/mrq`. Entries older than 7 days are ignored. |
| State | `$XDG_STATE_HOME/mrq`, falling back to `~/.local/state/mrq`. Holds the log and `notified.json`. |
| Token | `$MRQ_TOKEN`, `$GITLAB_TOKEN`, `gitlab.token_command`, `gitlab.token`, in that order. |

Unknown config keys are an error.

## `[gitlab]`

| Key | Default | Description |
| --- | --- | --- |
| `url` | `https://gitlab.com` | GitLab instance. |
| `token_command` | | Shell command whose trimmed stdout is the token. 10 s timeout. |
| `token` | | Literal token. Needs the `read_api` scope. |
| `timeout_secs` | `20` | Request timeout. |
| `max_concurrent_requests` | `4` | Parallel requests. |

## `[refresh]`

| Key | Default | Description |
| --- | --- | --- |
| `interval_secs` | `300` | Seconds between refreshes. Minimum `30`; lower values are rejected. |
| `jitter_secs` | `15` | Random `0..=jitter_secs` added to each cycle. Clamped to the interval. |
| `refresh_on_focus` | `true` | Refetch stale filters when the terminal regains focus. |
| `pause_when_unfocused` | `false` | Stop refreshing while unfocused. |

## `[ui]`

| Key | Default | Description |
| --- | --- | --- |
| `ascii` | `false` | Use ASCII instead of Unicode glyphs. |
| `show_drafts` | `false` | Show draft merge requests. |
| `relative_times` | `true` | Show times as relative. |
| `assigned_display` | `yes_no` | `yes_no`, `username` or `trigram`. |
| `approver_display` | `username` | Same values. |
| `reviewer_display` | `username` | Same values. |
| `merged_by_display` | `username` | Same values, for the `merged_by` column. |
| `link` | `title` | Hyperlinked column: `title`, `id`, `both` or `none`. |
| `mouse` | `false` | Capture the mouse. The details and comments popups capture it while open regardless, to select text. |
| `set_terminal_title` | `true` | Set the terminal title. |
| `columns` | see below | Column order. |
| `wide` | `false` | Start in wide mode. |

Default `columns`:
`approved, author, repo, id:wide, title, pipeline, assigned, approver:wide, reviewer:wide, age, updated, diff:wide`.

Available columns: `approved`, `author`, `repo`, `id`, `title`, `pipeline`, `assigned`, `approver`, `reviewer`, `merged_by`, `age`, `updated`, `diff`, `branch`. Each may appear once. A `:wide` suffix shows it only in wide mode. `merged_by` is not in the default columns; once added, it only appears while a merge request merged by someone other than its author is listed, and is blank for a self-merge.

## `[skin]`

| Key | Default | Description |
| --- | --- | --- |
| `name` | `catppuccin-mocha` | `catppuccin-mocha`, `catppuccin-macchiato`, `catppuccin-frappe`, `dracula`, `flexoki-dark`, `gruvbox-dark`, `monokai`, `nord`, `one-dark`, `rose-pine`, `solarized-dark`, `tokyo-night`. `auto` means the default. Short names such as `mocha` work. |

`[skin.colors]` overrides single swatches with `"#rrggbb"` values. Names: `rosewater`, `flamingo`, `pink`, `mauve`, `red`, `maroon`, `peach`, `yellow`, `green`, `teal`, `sky`, `sapphire`, `blue`, `lavender`, `text`, `subtext1`, `subtext0`, `overlay1`, `overlay0`, `surface2`, `surface1`, `surface0`, `base`, `mantle`, `crust`.

## `[sort]`

| Key | Default | Description |
| --- | --- | --- |
| `column` | `updated` | Any column in `[ui].columns`. |
| `order` | `desc` | `asc` or `desc`. |
| `drafts_last` | `true` | Sink drafts below other rows. |

Merged and closed MRs are always grouped below the others. Ties break on `updated` descending, then id. `pipeline` sorts by severity: failed, running, pending, manual, canceled, skipped, success, none.

## `[notifications]`

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Master switch. |
| `backend` | `auto` | `auto`, `osc9`, `osc777`, `command` or `none`. |
| `command` | | Required for `command`. `{title}` and `{body}` are substituted. |
| `bell` | `true` | Ring the terminal bell. |
| `on_new_mr` | `true` | New merge request. |
| `on_approval` | `true` | New approval. |
| `on_new_discussion` | `true` | New discussion. |
| `on_pipeline_change` | `false` | Pipeline status change. |
| `on_merged_or_closed` | `false` | Needs a filter with `state` of `all`, `merged` or `closed`. |
| `only_when_unfocused` | `true` | Ignored if the terminal reports no focus events. |

## `[browser]`

| Key | Default | Description |
| --- | --- | --- |
| `command` | empty | Empty means `open` on macOS and `xdg-open` on Linux. |

## `[[filter]]`

Each block is a tab.

### Scopes

| `scope` | Merge requests | Needs `path` |
| --- | --- | --- |
| `assigned` | assigned to you | no |
| `review_requested` | where you are a reviewer | no |
| `authored` | you opened | no |
| `group` | under a group and its subgroups | yes |
| `project` | in one project | yes |
| `instance` | every one the token can see | no |

`scope` may be a list of `assigned`, `review_requested` and `authored`.

### Keys

| Key | Scopes | Description |
| --- | --- | --- |
| `name` | all | Required, unique. Tab label and cache file name. |
| `scope` | all | See above. |
| `state` | all | `opened` (default), `merged`, `closed`, `all`. |
| `show_drafts` | all | Overrides `[ui].show_drafts`. |
| `columns` | all | Overrides `[ui].columns`. |
| `notify` | all | Overrides `[notifications].enabled`. |
| `max_results` | all | `1..=500`, default `100`. |
| `path` | `group`, `project` | GitLab full path, e.g. `acme/platform`. No leading or trailing slash. |
| `include_subgroups` | `group` | Default `true`. |
| `labels` | `group`, `project`, `instance` | AND-ed. |
| `not_labels` | same | Excludes MRs with any of these. |
| `author`, `assignee` | same | Username. |
| `reviewer` | same | Username. Exclusive with `has_reviewer`. |
| `has_reviewer` | same | `true` has any reviewer, `false` has none. |
| `milestone` | same | Milestone title. |
| `target_branch` | same | Branch name. |
| `updated_after_days` | same | Only MRs updated within this many days. |

Narrowing keys on `assigned`, `review_requested` or `authored` are a startup error.

## Key bindings

### Key specs

A spec is `[modifier-]*key`. Modifiers: `ctrl`, `alt`, `shift`, `super` (case-insensitive). A key is one printable character or one of `enter esc tab backspace space up down left right home end pagedown pageup insert delete f1`..`f12`. `shift-S` and `S` are the same.

Set actions under `[keys]`. Naming an action replaces its keys, `[]` unbinds it, and a key claimed by two actions is a startup error.

### Defaults

| Action | Keys | Description |
| --- | --- | --- |
| `down` / `up` | `j`, `down` / `k`, `up` | Move |
| `page_down` / `page_up` | `ctrl-d`, `pagedown` / `ctrl-u`, `pageup` | Half page |
| `full_page_down` / `full_page_up` | `ctrl-f` / `ctrl-b` | Full page |
| `top` / `bottom` | `g`, `home` / `shift-G`, `end` | First / last row |
| `open_mr` | `o`, `enter` | Open in browser |
| `open_pipeline` | `p` | Open latest pipeline |
| `open_project` | `shift-O` | Open project |
| `open_diffs` | `shift-D` | Open diffs |
| `copy_url` | `y` | Copy MR URL |
| `copy_branch` | `shift-Y` | Copy source branch |
| `show_details` | `d` | Details popup |
| `show_discussions` | `shift-C` | Comments popup |
| `toggle_drafts` | `ctrl-a` | Show or hide drafts |
| `toggle_wide` | `w` | Show or hide wide columns |
| `sort_menu` | `shift-S` | Choose sort column |
| `invert_sort` | `shift-I` | Invert order |
| `skin_menu` | `ctrl-t` | Change skin for this session |
| `next_filter` | `]`, `right`, `tab` | Next tab |
| `prev_filter` | `[`, `left`, `shift-tab` | Previous tab |
| `filter_menu` | `f` | Filter switcher |
| `search` | `/` | Search |
| `clear_search` | `esc` | Clear search |
| `refresh` | `ctrl-r` | Refresh all filters |
| `refresh_visible` | `r` | Refresh current filter |
| `reload_config` | `shift-R` | Reload config |
| `help` | `?`, `f1` | Key help |
| `log_menu` | `shift-L` | Recent log lines |
| `quit` | `q`, `ctrl-c` | Quit |

`1`..`9` jump to the matching tab. `Esc` closes any popup.
