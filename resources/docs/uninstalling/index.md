---
description: "Remove Hot Dev from macOS, Linux, or Windows, however you installed it, with hot uninstall or by hand."
---

# Uninstalling Hot

`hot uninstall` removes the install it runs from, the way that install was made:

```bash
hot uninstall
```

It lists what it will remove and asks first. Use `--dry-run` to see the list
and remove nothing, or `--yes` to skip the question in a script.

| Install | What `hot uninstall` does |
|---------|---------------------------|
| Installer script (`~/.local` or `--prefix`) | Deletes `bin/hot` and `share/hot`, and removes the installer's `PATH` line |
| macOS per-user package | The same, then forgets the package |
| macOS package (`/usr/local`) | Deletes the install with sudo, then forgets the package |
| Debian package | Runs `sudo dpkg --purge` on the package that owns `hot` |
| Homebrew | Runs `brew uninstall hot`, with the `brew` of the Homebrew install that holds this `hot` |
| Windows, either scope | Runs the Hot uninstaller, which asks for administrator rights for a Program Files install |

`hot uninstall` also removes the shell completions Hot installed, unless another
`hot` is still on your `PATH`. The Debian package always removes its own.

The installer's `PATH` line stays if the folder holds other programs, as
`~/.local/bin` often does. Removing it could break them.

If you have more than one install, `hot uninstall` removes only the one that
ran, and tells you where the others are.

Older `hot` versions don't have this command. Run `hot update` first, or follow
the steps below.

`hot uninstall` removes nothing if `HOT_HOME` points inside the install, or the
install holds a database, `cache`, or `file` folder from a `HOT_HOME` that did.
They would be deleted with it, so move them out first.

On Windows, stop any running `hot` commands first. The uninstaller runs after
`hot uninstall` exits, and it skips files that are in use.

## What stays

Uninstalling never touches your work:

- Your projects, and the `.hot` folder in each one, which holds a local
  project's database and files.
- Anything under `HOT_HOME`, if you set it.
- Downloaded packages and caches, which are safe to delete:
  - **macOS:** `~/Library/Caches/hot`
  - **Linux:** `~/.cache/hot`, or `$XDG_CACHE_HOME/hot`
  - **Windows:** `%LOCALAPPDATA%\hot`

## Removing Hot by hand

Use these steps if `hot` no longer runs, or is too old for `hot uninstall`.

### Installer script or macOS per-user package

```bash
rm -rf ~/.local/bin/hot ~/.local/share/hot
```

If you installed with `--prefix`, use that folder in place of `~/.local`. An
install with `--system` on Linux without dpkg is in `/usr/local`, and needs sudo:

```bash
sudo rm -rf /usr/local/bin/hot /usr/local/share/hot
```

Then open your shell's startup file (such as `~/.zshrc`, `~/.bashrc`, or
`~/.bash_profile`), and delete the `# Added by the Hot installer` comment and
the `PATH` line under it. Keep the line if other programs live in that folder.
For fish, delete `~/.config/fish/conf.d/hot.fish`.

For the macOS per-user package, also run:

```bash
pkgutil --volume "$HOME" --forget dev.hot.hot.user.pkg
```

### macOS package

```bash
sudo rm -rf /usr/local/bin/hot /usr/local/share/hot /usr/local/share/zsh/site-functions/_hot /usr/local/etc/bash_completion.d/hot /usr/local/share/fish/vendor_completions.d/hot.fish
```

```bash
sudo pkgutil --forget dev.hot.hot.pkg
```

### Debian package

The package is called `hot-cli`. To check, run `dpkg -S /usr/bin/hot`.

```bash
sudo dpkg --purge hot-cli
```

Older packages leave their shell completions behind. To delete them:

```bash
sudo rm -f /usr/share/zsh/vendor-completions/_hot /etc/bash_completion.d/hot /usr/share/fish/vendor_completions.d/hot.fish /usr/local/share/fish/vendor_completions.d/hot.fish
```

### Homebrew

```bash
brew uninstall hot
```

To remove the tap as well:

```bash
brew untap hot-dev/hot
```

### Windows

Open **Settings > Apps > Installed apps**, find **Hot Dev** (or **Hot Dev (User)**
for a per-user install), and choose **Uninstall**. You can also use the **Uninstall** shortcut in the Start menu,
or run `uninstall.exe` from the install folder (`%LOCALAPPDATA%\Programs\Hot Dev`
or `C:\Program Files\Hot Dev`).
