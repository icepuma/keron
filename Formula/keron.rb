class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.1.4"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.1.4/keron_0.1.4_darwin_arm64.tar.gz"
      sha256 "f2f65c4d658c464fd6e145c748b7ec5886a3bc55406c153e5f877b8fb33128f7"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.1.4/keron_0.1.4_darwin_amd64.tar.gz"
      sha256 "91772a1de096bd8dd7e160241e0d424a19ac067a7c4f17852035711d81c1e610"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.1.4/keron_0.1.4_linux_arm64.tar.gz"
      sha256 "8b1f2961c480d7cadf625c6e38a3e6312953c0b8b41f5392be33994e948de331"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.1.4/keron_0.1.4_linux_amd64.tar.gz"
      sha256 "9de800be7cbb98122a6e040aad8420ba736d032aa6c9ab297ea96c29883f5abd"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
