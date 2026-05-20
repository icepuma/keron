class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.3.1"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.3.1/keron_0.3.1_darwin_arm64.tar.gz"
      sha256 "080d50ba223ec3af46a04b8f9ae78f307a08155475f88ce72f086b1552065212"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.3.1/keron_0.3.1_darwin_amd64.tar.gz"
      sha256 "a2643e3a00a6a02c725cc28e712d1e04f0d45619e6b4a2de3f5b047d3788d502"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.3.1/keron_0.3.1_linux_arm64.tar.gz"
      sha256 "43d1f75efaa0d73ab11950acf440d091eff77ea7d7f60351b9f36299a709d919"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.3.1/keron_0.3.1_linux_amd64.tar.gz"
      sha256 "1a7901121fe49f76c030f50eaff426f0a149a24a0686a263c8e63d739b8b102d"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
