-- Workstation-specific resources, layered on top of base + dev.
depends_on("./base.lua")
depends_on("./dev.lua")

install_packages("brew", { "git", "ripgrep", "fd", "jq" }, {
  state = "present"
})

template("files/profile.tmpl", "/tmp/keron-example-complex/profile-workstation.toml", {
  mkdirs = true,
  force = true,
  vars = {
    profile = "workstation",
    user = "keron-workstation",
    shell = "/bin/zsh",
    path = env("PATH")
  }
})

cmd("sh", { "-c", "echo workstation-ready > /tmp/keron-example-complex/workstation.marker" })
