-- Shared baseline configuration for all machines.
link("files/gitconfig", "/tmp/keron-example-complex/.gitconfig", {
  mkdirs = true,
  force = true
})

template("files/profile.tmpl", "/tmp/keron-example-complex/profile.toml", {
  mkdirs = true,
  force = true,
  vars = {
    profile = "base",
    user = "keron",
    shell = "/bin/zsh",
    path = env("PATH")
  }
})

cmd("sh", { "-c", "mkdir -p /tmp/keron-example-complex && echo base-ready > /tmp/keron-example-complex/base.marker" })
