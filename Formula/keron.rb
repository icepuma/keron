class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.1.3"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.1.3/keron_0.1.3_darwin_arm64.tar.gz"
      sha256 "904e194b6a8048725713d43eb99f6f21f6601b2b832ecb014d3e29cb15ed7e59"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.1.3/keron_0.1.3_darwin_amd64.tar.gz"
      sha256 "a8f21fe1c21bc945840f2530532bbbbcf408e6a8423472abf42d862dccfbdf20"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.1.3/keron_0.1.3_linux_arm64.tar.gz"
      sha256 "70ccb7c140bb45d5fcb6687498b5c407c5c10524533d3812cef10c2994a37437"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.1.3/keron_0.1.3_linux_amd64.tar.gz"
      sha256 "48626e136bcaa571d8e6e4b148638b78f04a48fba6f50f1aa1c8e177c0a0cb3f"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
