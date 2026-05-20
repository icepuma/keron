class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.5.1"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.5.1/keron_0.5.1_darwin_arm64.tar.gz"
      sha256 "4f84835e684f3ef7aef1a550afb6352ba2390651a664aeab9eb8dbed8b3b5cc3"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.5.1/keron_0.5.1_darwin_amd64.tar.gz"
      sha256 "a6500385b9a11c4d48395b483456e28138303f3b2e1c47d4dc60e2ded26f414a"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.5.1/keron_0.5.1_linux_arm64.tar.gz"
      sha256 "d8a646110f562986107a82e78486d9b528f33043b12acf5eabf988c0de4be1b8"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.5.1/keron_0.5.1_linux_amd64.tar.gz"
      sha256 "9c3c6612da246b620f8c75107da4b567f9be63604023e5ea7aa1189b3f53db32"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
