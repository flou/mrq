# Tutorial: review your first merge request

In this tutorial you install mrq, connect it to GitLab, and open a merge request from your terminal. It takes about five minutes.

You need a GitLab account, [mise](https://mise.jdx.dev) or a Rust toolchain, and a terminal with a Unicode font.

## 1. Install mrq

With [mise](https://mise.jdx.dev) (builds from the git repository using the Rust toolchain):

```bash
mise use -g rust  # if cargo isn't installed yet
mise use -g cargo:https://github.com/flou/mrq@branch:main
```

Or build from source, from a clone of the repository:

```bash
cargo install --path .
```

Check that it worked:

```bash
mrq --version
```

## 2. Create a token

In GitLab, go to **Preferences → Access tokens** and create a personal access token with the `read_api` scope. Copy it.

## 3. Create the config file

```bash
mrq init-config
```

This writes a commented config to `~/.config/mrq/config.toml`. Every line in it is commented out, so it behaves as if it did not exist. Open it and set your GitLab URL:

```toml
[gitlab]
url = "https://gitlab.example.com"
```

Leave `url` out if you use gitlab.com.

## 4. Provide the token

For this tutorial, export it in your shell:

```bash
export MRQ_TOKEN="glpat-..."
```

[How to keep the token out of your shell](howto.md#keep-the-token-out-of-the-config-file) shows better options for daily use.

## 5. Check the setup

```bash
mrq check
```

mrq validates the config, verifies the token and connects to GitLab. Fix any error it prints before you continue.

## 6. Start mrq

```bash
mrq
```

You see one tab, **Assigned**, listing the open merge requests assigned to you. Rows from a previous run appear immediately, and mrq refreshes them in the background.

## 7. Move around

| Key | What happens |
| --- | --- |
| `j` / `k` | move down / up |
| `d` | show the details of the selected merge request |
| `Shift-C` | show its comments |
| `Esc` | close a popup |
| `w` | show extra columns |
| `?` | list every key |

## 8. Open a merge request

Press `o` or `Enter` to open the selected merge request in your browser. Press `y` to copy its URL instead.

Press `q` to quit.

## Next steps

- Add more tabs: [How to configure filters](howto.md#add-a-filter).
- Look up every setting: [Reference](reference.md).
