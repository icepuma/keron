-- Expand list-based package declarations.
install_packages("brew", { "git", "ripgrep", "fd" }, {
  state = "present"
})
