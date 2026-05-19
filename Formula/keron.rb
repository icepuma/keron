class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "0.1.2"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.1.2/keron_0.1.2_darwin_arm64.tar.gz"
      sha256 "7577e1d95ec9690e71e0f20af2ed048e1fd6be3786da75a31d57cb6bbc4266cb"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.1.2/keron_0.1.2_darwin_amd64.tar.gz"
      sha256 "ec217fc723a2faebf894b20dc0e457ae9573e9d795104800572d14e7c7da9264"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/v0.1.2/keron_0.1.2_linux_arm64.tar.gz"
      sha256 "d627e8af9797cae4f8167687a191ecb73b211275fc9e494831b8a9f9dcd63299"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/v0.1.2/keron_0.1.2_linux_amd64.tar.gz"
      sha256 "f23925f6db98033633eb833a81edf89c8fe9c2328906984243601233b49ccac9"
    end
  end

  def install
    bin.install "keron"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
