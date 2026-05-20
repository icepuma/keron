class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.4.0"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.4.0/keron_0.4.0_darwin_arm64.tar.gz"
      sha256 "3d443ab08d405ad1dfa14e856e8911bfd30f7870726c97098979d41db505d289"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.4.0/keron_0.4.0_darwin_amd64.tar.gz"
      sha256 "1aa67a4eeb2691a190f4c28b3b65414cedcc5190645490730b3f193b771403cc"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.4.0/keron_0.4.0_linux_arm64.tar.gz"
      sha256 "7a7775a5145c684015a429f6e362c12ff534617b4fd52ac4f6c86f062754ce49"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.4.0/keron_0.4.0_linux_amd64.tar.gz"
      sha256 "ac8f6c8693155b7b37d87d0383d0e91c0b15a7d51e1be4ed6eaff8971100fd56"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
