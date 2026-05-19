class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.1.0"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.1.0/keron_0.1.0_darwin_arm64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.1.0/keron_0.1.0_darwin_amd64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.1.0/keron_0.1.0_linux_arm64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.1.0/keron_0.1.0_linux_amd64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
