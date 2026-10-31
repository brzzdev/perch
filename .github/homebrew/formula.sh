#!/usr/bin/env sh
# Prints the Homebrew formula for one release: formula.sh <version> <SHA256SUMS>
set -eu

version="$1"
sums="$2"
release="https://github.com/brzzdev/perch/releases/download/v$version"

archive() {
  file="perch-$1.tar.gz"
  sha="$(awk -v file="$file" '$2 == file { print $1 }' "$sums")"
  if [ -z "$sha" ]; then
    echo "error: $sums has no checksum for $file" >&2
    exit 1
  fi
  printf '      url "%s/%s"\n      sha256 "%s"\n' "$release" "$file" "$sha"
}

# Each archive is captured first so a missing checksum fails the script rather
# than leaving a hole in the formula.
macos_arm="$(archive aarch64-apple-darwin)"
macos_intel="$(archive x86_64-apple-darwin)"
linux_arm="$(archive aarch64-unknown-linux-musl)"
linux_intel="$(archive x86_64-unknown-linux-musl)"

cat <<FORMULA
class Perch < Formula
  desc "Interactive Git branch and worktree switcher with merged-branch cleanup"
  homepage "https://github.com/brzzdev/perch"
  license "MIT"

  on_macos do
    on_arm do
$macos_arm
    end
    on_intel do
$macos_intel
    end
  end

  on_linux do
    on_arm do
$linux_arm
    end
    on_intel do
$linux_intel
    end
  end

  def install
    bin.install "perch"
    bash_completion.install "completions/perch.bash" => "perch"
    fish_completion.install "completions/perch.fish"
    zsh_completion.install "completions/_perch"
  end

  def caveats
    <<~EOS
      perch needs its shell integration to cd into worktrees. Load it with:
        perch shell install
    EOS
  end

  test do
    assert_match "perch #{version}", shell_output("#{bin}/perch --version")
  end
end
FORMULA
