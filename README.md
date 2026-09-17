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

Generate a commented default config:

```bash
mrq init-config
```

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

## Development

Run all checks (what CI runs):

```bash
./scripts/check.sh
```

Run tests:

```bash
cargo test --all-targets
```

## Architecture

- **Single-threaded async** — one `mpsc` channel of `AppEvent` feeds a single owner of mutable state (no locks)
- **Read-only GitLab client** — GraphQL queries only, no mutations
- **Complexity budget** — queries are costed against the instance's complexity ceiling; over-budget queries return no data
- **macOS and Linux only**

## License

MIT
