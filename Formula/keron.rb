class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.5.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.5.2/keron-v0.5.2-aarch64-apple-darwin.tar.gz"
      sha256 "60ade90795ea27d87df348d8e4a7cd2af771266175f880b97ee1cc5c56c83e8c"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.5.2/keron-v0.5.2-x86_64-apple-darwin.tar.gz"
      sha256 "e357c4d471f5f542b35d987d5890c680aa277e9525f7ff47492b469d532b373f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.5.2/keron-v0.5.2-aarch64-unknown-linux-musl.tar.gz"
      sha256 "ba047151843c1b39c965beaaee1bd4de7b9a14a97b1e7cbf679e4798569df71b"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.5.2/keron-v0.5.2-x86_64-unknown-linux-musl.tar.gz"
      sha256 "4d9064491ec253ece3abefe6b33e24b1a0c44973272958c87a9fdbec902dd2d2"
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
