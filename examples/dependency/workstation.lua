-- Workstation-specific manifest depending on base settings.
depends_on("./base.lua")
link("files/tmux.conf", "/tmp/keron-example-dependency/.tmux.conf", { mkdirs = true, force = true })
cmd("sh", { "-c", "echo dependency-manifest > /tmp/keron-example-dependency/marker.txt" })
