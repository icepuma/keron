-- Minimal manifest example.
link("files/zshrc", "/tmp/keron-example-simple/.zshrc", { mkdirs = true, force = true })
cmd("sh", { "-c", "echo simple-manifest > /tmp/keron-example-simple/marker.txt" })
