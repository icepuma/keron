-- Developer machine overrides.
depends_on("./base.lua")

link("files/zshrc", "/tmp/keron-example-complex/.zshrc", {
  mkdirs = true,
  force = true
})

cmd("sh", { "-c", "echo dev-ready > /tmp/keron-example-complex/dev.marker" })
