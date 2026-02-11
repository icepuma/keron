-- Render a config file using a Proton Pass secret.
template("files/profile.tmpl", "/tmp/keron-example-proton-pass/profile.conf", {
  mkdirs = true,
  force = true,
  vars = {
    username = secret("pp://Personal/test/username")
  }
})
