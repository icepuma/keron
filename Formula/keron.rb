class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.2.2"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.2.2/keron_0.2.2_darwin_arm64.tar.gz"
      sha256 "e21f32ac712bf1de50be14f40fdf0e0143126df853238f512f4f5b027962a380"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.2.2/keron_0.2.2_darwin_amd64.tar.gz"
      sha256 "558c5edf708024b8e2ccd100281167d541f4203baad6b109d0e1076e4c0703fd"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.2.2/keron_0.2.2_linux_arm64.tar.gz"
      sha256 "63b2865fb327d133df7a665f1e2abc489cbf42abc17a6db819d6c0598990cf88"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.2.2/keron_0.2.2_linux_amd64.tar.gz"
      sha256 "77f1bd2695f5d2e4e0f21e5ed1e1142fbac73543fe26c257be0d7e1861bc402d"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
