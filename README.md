# mrq

A keyboard-driven TUI for the GitLab merge requests you need to review.

## Installation

With [mise](https://mise.jdx.dev) (builds from the git repository using the Rust toolchain):

```bash
mise use -g rust  # if cargo isn't installed yet
mise use -g cargo:https://github.com/flou/mrq@branch:main
```

Or build from source (see the [tutorial](docs/tutorial.md) for a guided first run):

```bash
cargo install --path .
```

## Documentation

- [Tutorial](docs/tutorial.md): install mrq and review your first merge request
- [How-to guides](docs/howto.md): filters, tokens, columns, keys, notifications
- [Reference](docs/reference.md): CLI, every config key, key bindings
- [Explanation](docs/explanation.md): how filters, refresh, cache and notifications work

## Quick start

```bash
mrq init-config   # write ~/.config/mrq/config.toml
mrq check         # validate config, token and connectivity
mrq               # start the TUI
```

The token needs the `read_api` scope and is read from `$MRQ_TOKEN`, `$GITLAB_TOKEN`, `token_command`, or `token`.

`config.schema.json` is the JSON Schema of the config; regenerate it with `mrq schema > config.schema.json`.
