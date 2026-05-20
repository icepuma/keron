class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.2.1"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.2.1/keron_0.2.1_darwin_arm64.tar.gz"
      sha256 "fbbbbb81d8d890ca569202c46a1ac2447d88fa6a5b1cf9b0439af105003da17e"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.2.1/keron_0.2.1_darwin_amd64.tar.gz"
      sha256 "80260ab4428abf7c75da17a95c3b0470c5de45b2c30207b63cfa6da9e8cd6090"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.2.1/keron_0.2.1_linux_arm64.tar.gz"
      sha256 "86dc9be3e06dd35ee38f5b73abe5c73cb997a32a7362715bb8b92bee3d31da8a"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.2.1/keron_0.2.1_linux_amd64.tar.gz"
      sha256 "b98a2db3894b370451336dc06ce845f9a72ea0fd24782f2b5542a2e4a351c91c"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
