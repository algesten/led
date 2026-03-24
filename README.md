# led

A lightweight, fast text editor for the terminal, written in Rust.

led is built on a functional reactive programming (FRP) architecture where the model is a pure reducer, all side effects live in drivers, and the UI is a derived function of state. The result is a responsive editor with rich features and a small footprint.

<!-- SCREENSHOT: hero — full editor view showing a Rust file with syntax highlighting,
     side panel file browser, tab bar, status bar with git branch, and LSP diagnostics
     in the gutter. This is the "first impression" shot. -->

## Features

### Language Server Protocol

Full LSP integration with out-of-the-box support for:

| Language              | Server                      |
|-----------------------|-----------------------------|
| Rust                  | rust-analyzer               |
| TypeScript/JavaScript | typescript-language-server  |
| Python                | pyright                     |
| C/C++                 | clangd                      |
| Swift                 | sourcekit-lsp               |
| TOML                  | taplo                       |
| JSON                  | vscode-json-language-server |
| Bash                  | bash-language-server        |

- **Completions** — fuzzy-filtered popup with auto-imports
- **Go to definition**
- **Rename** — workspace-wide symbol rename
- **Code actions** — quick fixes and refactors
- **Diagnostics** — inline errors and warnings with gutter indicators
- **Inlay hints** — toggleable type annotations
- **Format** — on-demand formatting with import sorting
- **Progress** — spinner in status bar during long operations

<!-- SCREENSHOT: lsp-completion — completion popup open on a Rust or TypeScript file,
     showing fuzzy-filtered results with detail text -->

<!-- SCREENSHOT: lsp-diagnostics — editor showing error/warning squiggles in the gutter
     and a diagnostic message in the status bar -->

### Syntax Highlighting

Tree-sitter powered highlighting for Bash, C, C++, JavaScript, JSON, Make, Markdown, Python, Rust, Swift, TOML, and TypeScript. Rainbow bracket coloring with matching bracket highlighting.

<!-- SCREENSHOT: syntax — a file showing rich syntax highlighting and rainbow brackets,
     ideally with a matching bracket pair visible -->

### Search

- **Project search** (`Ctrl+f`) — ripgrep-powered search across the workspace with case and regex toggles. Results grouped by file with a live preview pane.
- **Buffer search** (`Ctrl+s`) — incremental search within the current file with match highlighting and wrap-around navigation.
- **Find file** (`Ctrl+x Ctrl+f`) — fuzzy path completion for quick file opening.

<!-- SCREENSHOT: file-search — project-wide search open in the side panel showing
     results grouped by file, with a match previewed in the editor -->

### File Browser

Side panel file tree with expand/collapse, file preview on selection, and background open. Toggle with `Ctrl+b`.

### Git Integration

Current branch in the status bar. Per-file status (modified, staged, untracked) shown in the file browser. Line-level change indicators in the editor gutter.

<!-- SCREENSHOT: git-gutter — editor gutter showing green (added) and blue (modified)
     change indicators next to code, with the branch name visible in the status bar -->

### Session Persistence

led automatically saves and restores your session — open files, cursor positions, scroll positions, tab order, and undo history are preserved across restarts via a per-workspace SQLite database.

Multiple editor instances on the same workspace are supported. The primary instance holds a lock; secondary instances sync edits through the workspace database.

### Editing

- Undo/redo with Emacs-style linear history
- Mark and region (set mark, kill region, yank)
- Kill ring with text accumulation across consecutive deletes
- Smart indentation on newline
- Jump list — navigate back and forth through your position history
- Symbol outline via LSP document symbols
- Bracket matching and jump-to-matching-bracket
- Auto-close buffers to prevent resource exhaustion

## Install

```
cargo install --path led
```

Or build from source:

```
git clone https://github.com/user/led.git
cd led
cargo build --release
```

The binary will be at `target/release/led`.

## Usage

```
led [path]
```

Open a file or directory. With no argument, led opens the current directory.

| Flag              | Description                                               |
|-------------------|-----------------------------------------------------------|
| `--log-file PATH` | Write debug logs to a file                                |
| `--reset-config`  | Reset keybindings and theme to defaults, clear session DB |

## Configuration

Configuration lives in `~/.config/led/`.

| File         | Purpose          |
|--------------|------------------|
| `keys.toml`  | Key bindings     |
| `theme.toml` | Color theme      |
| `db.sqlite`  | Session database |

Run `led --reset-config` to restore default keybindings and theme.

### Custom Key Bindings

Key bindings are defined in TOML. Modifiers are `ctrl`, `alt`, and `shift`. Chord sequences use sub-tables.

```toml
[keys]
"ctrl+s" = "in_buffer_search"
"alt+enter" = "lsp_goto_definition"

# Chord: Ctrl+x followed by Ctrl+s
[keys."ctrl+x"]
"ctrl+s" = "save"
```

Context-specific bindings for the file browser and search panel use `[browser]` and `[file_search]` sections.

## Default Key Bindings

### Editing

| Key                 | Action                         |
|---------------------|--------------------------------|
| `Enter`             | Insert newline                 |
| `Backspace`         | Delete backward                |
| `Delete` / `Ctrl+d` | Delete forward                 |
| `Tab`               | Insert tab / accept completion |
| `Ctrl+k`            | Kill line                      |
| `Ctrl+/` / `Ctrl+_` | Undo                           |
| `Ctrl+Space`        | Set mark                       |
| `Ctrl+w`            | Kill region                    |
| `Ctrl+y`            | Yank (paste)                   |

### Navigation

| Key                    | Action                   |
|------------------------|--------------------------|
| `Up` / `Down`          | Move up / down           |
| `Left` / `Right`       | Move left / right        |
| `Home` / `Ctrl+a`      | Line start               |
| `End` / `Ctrl+e`       | Line end                 |
| `Page Up` / `Alt+v`    | Page up                  |
| `Page Down` / `Ctrl+v` | Page down                |
| `Ctrl+Home` / `Alt+<`  | File start               |
| `Ctrl+End` / `Alt+>`   | File end                 |
| `Alt+]`                | Jump to matching bracket |

### Tabs & Buffers

| Key          | Action       |
|--------------|--------------|
| `Ctrl+Left`  | Previous tab |
| `Ctrl+Right` | Next tab     |
| `Ctrl+x k`   | Kill buffer  |

### Files

| Key             | Action    |
|-----------------|-----------|
| `Ctrl+x Ctrl+s` | Save      |
| `Ctrl+x Ctrl+w` | Save as   |
| `Ctrl+x Ctrl+f` | Find file |

### Search

| Key                 | Action                  |
|---------------------|-------------------------|
| `Ctrl+f`            | Project search          |
| `Ctrl+s`            | Buffer search           |
| `Alt+1` (in search) | Toggle case sensitivity |
| `Alt+2` (in search) | Toggle regex            |

### LSP

| Key         | Action              |
|-------------|---------------------|
| `Alt+Enter` | Go to definition    |
| `Ctrl+r`    | Rename symbol       |
| `Alt+i`     | Code action         |
| `Ctrl+t`    | Toggle inlay hints  |
| `Alt+.`     | Next diagnostic     |
| `Alt+,`     | Previous diagnostic |
| `Ctrl+x i`  | Sort imports        |

### Jump List

| Key                   | Action       |
|-----------------------|--------------|
| `Alt+b` / `Alt+Left`  | Jump back    |
| `Alt+f` / `Alt+Right` | Jump forward |
| `Alt+o`               | Outline      |

### UI

| Key              | Action                             |
|------------------|------------------------------------|
| `Ctrl+b`         | Toggle side panel                  |
| `Alt+Tab`        | Toggle focus (editor / side panel) |
| `Ctrl+h e`       | Open messages                      |
| `Esc` / `Ctrl+g` | Abort / cancel                     |
| `Ctrl+z`         | Suspend                            |
| `Ctrl+x Ctrl+c`  | Quit                               |

### File Browser (when focused)

| Key         | Action             |
|-------------|--------------------|
| `Left`      | Collapse directory |
| `Right`     | Expand directory   |
| `Enter`     | Open selected      |
| `Alt+Enter` | Open in background |
| `Ctrl+q`    | Collapse all       |

## Architecture

led follows a strict FRP cycle:

```
State → Derived → Drivers → Model → State
```

- **State** — a single immutable `AppState` containing all editor state
- **Derived** — pure transforms from state into driver commands (no business logic)
- **Drivers** — handle side effects: terminal I/O, file system, LSP, git, syntax parsing, clipboard, timers, session persistence
- **Model** — a pure reducer `(State, Mutation) → State` composed from combinator chains

The codebase is organized as a Cargo workspace with 15 crates — each driver (LSP, git, syntax, file search, clipboard, etc.) is its own crate with a clean boundary.

## License

MIT
