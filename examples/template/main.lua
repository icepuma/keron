-- Render a config file from a local template.
template("files/profile.tmpl", "/tmp/keron-example-template/profile.conf", {
  mkdirs = true,
  force = true,
  vars = {
    name = "keron-user",
    shell = "/bin/zsh"
  }
})
