class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.6.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.6.0/keron-v0.6.0-aarch64-apple-darwin.tar.gz"
      sha256 "d80916e585d47f68a03f2835aaef7a9c4dafa4d20426de7726cd29fb18020f33"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.6.0/keron-v0.6.0-x86_64-apple-darwin.tar.gz"
      sha256 "2bddcc1340e393969b111b418b4f8c60e9e28034fc2a9fb5861f9877ae139215"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.6.0/keron-v0.6.0-aarch64-unknown-linux-musl.tar.gz"
      sha256 "34e37b1a48cf4300a36fa53bc3b6f23c1d07eb0e52ee6089eb4bdc5e786af7ec"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.6.0/keron-v0.6.0-x86_64-unknown-linux-musl.tar.gz"
      sha256 "b2295f769edf06641db03f355d6c725093b5735ee0c5ba9a5c6a6d78e851b366"
    end
  end

  def install
    bin.install "keron"
    doc.install "README.md"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
