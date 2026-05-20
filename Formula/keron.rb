class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.2.0"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.2.0/keron_0.2.0_darwin_arm64.tar.gz"
      sha256 "17e48b0aab48a001ebfdd249292bce64ce4c42c4b885986c9244622b36c3a43e"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.2.0/keron_0.2.0_darwin_amd64.tar.gz"
      sha256 "0bccf46a4535da69443048553c75dee353ffa3f67abf6f1b89d8a978f1219aae"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.2.0/keron_0.2.0_linux_arm64.tar.gz"
      sha256 "d1caf4f914acc61619fcc959a3b3a13e921e95fbe83b5d90658e44b1ca1da3ee"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.2.0/keron_0.2.0_linux_amd64.tar.gz"
      sha256 "a3b308d933077f2ad58f2a8d8c9b3877f9de542f4cdd06517c99bd27f6491fb7"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
