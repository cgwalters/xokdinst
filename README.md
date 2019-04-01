xokdinst
---

Wraps [openshift-installer](https://github.com/openshift/installer/) with a
few added features.

Quick start
---

Launch a cluster named `mycluster` (you may be more creative with names):

```
$ xokdinst launch mycluster
<fill out installer fields>
```

For more commands, just run `xokdinst --help`.

Why
---

The primary feature is that `xokdinst` by default has an opinionated place
to store configuration, in platform-specific "appdirs" as defined by
the Rust [directories crate](https://crates.io/crates/directories) - e.g. on Linux/Unix
this is `~/.config/xokdinst`.

Basically we're always using the `--dir` option of the upstream installer.
This makes it more convenient to manage multiple clusters.

`xokdinst` also has the concept of a "default config" for a given platform.
And if you only have used one platform, it becomes the default config.
When you run `launch` the first time, we introspect the platform chosen and
save the config as `config-<platform>.yaml`.

For example, running this:

```
$ xokdinst launch mycluster2
```

Will create a second cluster that inherits everything except the name
from the base.

Why not add this to the installer upstream?
---

It'd be a notable UX change.  We should consider it of course.

Why is this implemented in Rust
---

Originally it was in Python but I really feel the lack of static types there.
Go is annoying for "scripts" for a few reasons, mainly how verbose error
handling is versus Rust's simple and elegant `?` operator.
Also, I feel at home writing Rust.
