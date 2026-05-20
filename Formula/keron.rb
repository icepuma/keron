class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.5.0"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.5.0/keron_0.5.0_darwin_arm64.tar.gz"
      sha256 "7360efb2908b70a4bda6d2bad788f63557313977ff0b39b7209f5d2432ad0b36"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.5.0/keron_0.5.0_darwin_amd64.tar.gz"
      sha256 "b99a63bd485bde9cf4e111949fba2943d095ec0961aa6a7144c68f877788dc0b"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.5.0/keron_0.5.0_linux_arm64.tar.gz"
      sha256 "092c131680e57aeda575e216c56a39048e580fdf41eb4d9f03dd8cf9086695d5"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.5.0/keron_0.5.0_linux_amd64.tar.gz"
      sha256 "00412b2d79b659651cdf0150c40ad6b1744f0a68980b87c234268850844ead22"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
