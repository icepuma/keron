class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.5.3"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.5.3/keron-v0.5.3-aarch64-apple-darwin.tar.gz"
      sha256 "030423f4d05832a0c54433b894c5952a77818cc6ebc7241052ff02039fc7963e"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.5.3/keron-v0.5.3-x86_64-apple-darwin.tar.gz"
      sha256 "62cc7ba5f93da7b3ae65d04df3d2feab6e2d71bf6e75514ab5408f744015dffe"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.5.3/keron-v0.5.3-aarch64-unknown-linux-musl.tar.gz"
      sha256 "e0c20873c83d94b3d816923bad890e6226dffb5f0c34ca9a6be2448154d1575b"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.5.3/keron-v0.5.3-x86_64-unknown-linux-musl.tar.gz"
      sha256 "d585b3b08ce56634a0e4903d6686c32fc18170d49c32f0dee94e214dac0ebf90"
    end
  end

  def install
    bin.install "keron"
    doc.install "README.md"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
