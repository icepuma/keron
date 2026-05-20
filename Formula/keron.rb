class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.3.0"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.3.0/keron_0.3.0_darwin_arm64.tar.gz"
      sha256 "decc80cde4dd942726d0b68ae28e94fb77904c87f10fcb2d23faf24f73ab60fb"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.3.0/keron_0.3.0_darwin_amd64.tar.gz"
      sha256 "3234ff5b43059daf986782a0785447437f2fa75be9044f9a2c8efd5c65652af4"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.3.0/keron_0.3.0_linux_arm64.tar.gz"
      sha256 "308d20bc733795a09eb58d947d19a1ede82c2ed1e618df11e3d16b2968c444a3"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.3.0/keron_0.3.0_linux_amd64.tar.gz"
      sha256 "2ce1499ba3e1abdcc3b3090b693933638018f20ba8c674ced78a5c142fe44987"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
