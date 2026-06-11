# keron

A user-level dotfile and package manager driven by a small declarative language.

> **Status:** alpha (0.5.x) — the language and CLI will change without notice.
> Co-developed with AI, reviewed by humans.

## What it is

You write `.keron` files describing the state you want — symlinks, templated
files, packages, shell scripts. `keron apply` shows you an OpenTofu-style diff.
Add `--execute` and it applies after confirmation.

It runs entirely at the user level. Elevation (sudo / UAC) only happens when a
specific resource needs it, and only for that step.

## Example

```keron
val home: String = env("HOME") ?? keron_root()

reconcile {
  symlink(source = "./zshrc",     target = "${home}/.zshrc");
  symlink(source = "./gitconfig", target = "${home}/.gitconfig");
}
```

More examples (imports, structs, `match`, templates, packages) live under
`crates/keron-cli/tests/fixtures/`.

## Install

Download a binary for your platform from the
[Releases](https://github.com/icepuma/keron/releases) page — a single static
binary with no runtime dependencies. Supported targets: macOS arm64/amd64,
Linux arm64/amd64.

A Homebrew formula is included at `Formula/keron.rb`. A public tap is not yet
published.

## Use

```sh
keron apply ./manifest.keron              # show the plan
keron apply ./manifest.keron --execute    # apply after confirmation
keron format ./manifest.keron             # normalize a file in place
keron format . --check                    # verify formatting in CI
```

`<PATH>` may be a single `.keron` file or a directory of them (loaded in sorted
order).

## The language at a glance

- Static types: `String`, `Int`, `Double`, `Boolean`, nullable (`?`), lists,
  maps, structs, closed string unions.
- Control flow: `if`/`else`, `match`, `for` over lists and maps.
- Imports: `from "./other.keron" use a, b` — user files only; the stdlib is
  implicit.
- Resources: `symlink`, `template`, `shell`, package constructors (`brew`,
  `cask`, `cargo`, `winget`). `brew`/`cask` accept an optional tap URL
  (`brew("icepuma/keron/keron", "https://github.com/icepuma/keron")`)
  and a slash-qualified name (`brew("user/tap/formula")`) auto-derives
  the conventional `homebrew-<tap>` URL.
- `reconcile { ... }` blocks emit the resources to apply. `->` and source
  order group resources for readability, but are **not** a hard ordering
  guarantee: the executor batches package installs and runs every step that
  needs elevation after the unprivileged ones, so don't rely on `->` to
  sequence a privileged write before an unprivileged step that reads it.
- Eval-time file IO is confined to the keron root.

## License

MIT licensed (see [`LICENSE`](LICENSE)).

---

*Trivia — in Stargate SG-1, a "keron" is an energy particle used by the
Replicators; each Replicator block contains at least two million keron
pathways. Known to the Asgard, undiscovered by the Tau'ri. (SG-1, "Small
Victories".)*
