# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/flou/mrq/compare/v0.2.0...v0.3.0) - 2026-10-28

### Added

- *(term)* detect Rex for notifications, truecolor and hyperlinks
- *(config)* [**breaking**] group max_results into an { open, closed } table
- *(ui)* show the sort direction on the sorted column header
- *(config)* add per-filter sort override

### Fixed

- *(ui)* scroll list popups only when the selection nears an edge

### Other

- *(deps)* update jdx/packslip action to v1.11.2
- *(deps)* update rust crate unicode-width to v0.2.3
- *(gitlab)* negate excluded scopes server-side
- *(deps)* update release-plz/action action to v0.5.147
- *(deps)* update swatinem/rust-cache action to v2.9.3
- *(deps)* update rust crate jsonschema to 0.60.0
- *(deps)* update jdx/packslip action to v1.11.1
- *(deps)* update jdx/mise-action action to v5.2.4
- *(deps)* update rustls webpki crates

## [0.2.0](https://github.com/flou/mrq/compare/v0.1.0...v0.2.0) - 2026-10-16

### Added

- *(config)* [**breaking**] exclude a current-user relation from a filter's scope
- *(ui)* add a glyph and colour legend to the help popup
- *(ui)* add dim_drafts to choose which drafts are greyed out
- *(ui)* mark merged, closed and draft titles with a coloured state glyph
- *(activity)* add an activity feed popup listing recent changes

### Fixed

- *(deps)* update rustls and yoke-derive to clear deny gate
- *(cache)* flush before rename so readers never see an empty file
- *(fetch)* run the per-state queries concurrently
- *(fetch)* cap merged and closed separately so they cannot hide open MRs
- *(notify)* use OSC 777 on Ghostty and WezTerm so notifications carry the filter title

### Other

- *(deps)* update rust crate thiserror to v2.0.21
- *(deps)* update rust crate tokio to v1.53.3
- *(deps)* update rust crate tokio-util to v0.7.20
- *(deps)* update rust crate toml to v1.1.8
- *(deps)* update rust crate jsonschema to 0.58.0
- *(deps)* update jdx/packslip action to v1.8.0
- *(deps)* update jdx/mise-action action to v5.2.0
- *(deps)* update rust crate schemars to v1.2.3
- *(deps)* update rust crate serde_json to v1.0.154
- *(deps)* update rust crate rand to v0.10.3
- fix github rate limiting
- enforce dependency licenses and ship third-party attribution
- *(deps)* update rust crate jiff to v0.2.38
- *(deps)* update dtolnay/rust-toolchain digest to b725eda
- *(deps)* update release-plz/action action to v0.5.142
- Add renovate.json
- update README

## [0.1.0](https://github.com/flou/mrq/releases/tag/v0.1.0) - 2026-10-11

### Added

- *(ui)* add an opt-in STATUS column
- *(config)* add a global max_results, overridable per filter
- *(ui)* mark a fresh row with a thin gutter bar instead of solid blocks
- *(ui)* leave MERGED BY blank for a self-merge
- *(ui)* add an opt-in MERGED BY column
- *(ui)* confine mouse text selection to the details modal
- *(ui)* flag an author or assignee who cannot merge in the warning colour
- *(sort)* group merged and closed MRs at the bottom of the list
- *(ui)* dim merged and closed MRs listed next to opened ones
- *(config)* let a filter's scope be a list, merging the scopes' results
- *(schema)* check in config.schema.json and test it stays current
- *(ui)* add a reload_config action (shift-R) that re-reads the config file
- *(ui)* add per-filter columns and a :wide suffix for wide-only columns
- *(ui)* flag unresolved discussions with a 💬 and add a comments popup
- *(ui)* add ctrl-f/ctrl-b to move a full page in the table and popups
- *(ui)* add [ui].link to put the hyperlink on the ID, title, both or neither
- *(ui)* add a wide-only ID column between repo and title
- *(ui)* drop table side borders and tighten the approval column
- *(ui)* size approver and reviewer columns to their content
- *(ui)* highlight new merge requests with a cyan block and title
- *(config)* ui.wide starts mrq with wide mode on ([#1](https://github.com/flou/mrq/pull/1))
- *(app)* open the merge request diffs with shift-D
- *(ui)* mark tabs with unseen new merge requests
- *(config)* mrq check --offline validates the config with no network
- *(config)* rebind show_details to d and toggle_drafts to ctrl-a
- *(app)* richer search-box editing
- *(config)* default binds for tab/shift-tab, refresh_visible, log_menu
- make assigned/approver/reviewer column display configurable
- wrap details popup description to the popup's live width
- render MR description as markdown in details popup
- show merge request details in a popup
- add APPROVER and REVIEWER columns
- app name in notification
- show assignee trigram in ASG column
- strip/lto release profile
- allow enabling/disabling notifications per filter
- drop the APRV header for a tighter column
- widen the gap around REPO for readability
- size the REPO column to its content
- size the AUTHOR column to its content
- init project

### Fixed

- failing test in CI
- *(ui)* repaint list cells equal to the popup's after closing it
- format
- *(schema)* make the JSON schema match what config validation accepts
- *(app)* retry a transient identity-probe failure instead of quitting
- *(ui)* print each title link once, over the frame, instead of per cell ([#2](https://github.com/flou/mrq/pull/2))
- *(ui)* wrap every title cell in its own hyperlink after scrolling
- *(ui)* stop stale glyphs after scrolling a description with emoji
- *(ui)* status bar help hint follows the keymap
- *(app)* mouse clicks no longer reach the table through an open popup
- *(app)* filter popup lets q, j, k be typed into its query
- *(app)* a bare q closes a popup instead of quitting
- scroll Help/Log/Details popups one line per key press
- paint from cache before the identity probe
- add close popup keybind
- OSC 8 hyperlinks spanning rows, covering padding, or never closing

### Other

- automate releases with release-plz
- keep GitHub Actions up to date
- ship LICENSE in release archives and document the Cargo.lock refresh
- document prebuilt binaries and cutting a release
- add CI and multi-platform release workflows
- add screenshot to README
- *(readme)* add mise installation method
- add tutorial, how-to, reference and explanation pages
- *(schema)* describe every config field and enum value in the JSON schema
- *(ui)* index link spans and close the link if a write fails
- add mise check task
- document filters and scopes
