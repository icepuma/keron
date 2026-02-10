-- Expand list-based package declarations.
packages("brew", { "git", "ripgrep", "fd" }, {
  state = "present"
})
