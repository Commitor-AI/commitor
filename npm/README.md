# commitor-cli (npm wrapper)

This package wraps the prebuilt binary for
[Commitor](https://github.com/Commitor-AI/commitor), an AI-powered Git commit
message generator / commit-splitting CLI.

On `npm install`, a small `postinstall` script downloads the matching prebuilt
binary for your platform from the GitHub Release for this version — no Rust
toolchain required.

```sh
npm install -g commitor-cli
commitor --help
```

The npm package version tracks the Rust crate version 1:1 (e.g. `0.2.0`).

## Supported platforms

- macOS `arm64` / `x64`
- Linux `x64`
- Windows `x64`

Other platforms: install from source with `cargo install commitor-cli`, or
download a binary manually from the
[releases page](https://github.com/Commitor-AI/commitor/releases).
